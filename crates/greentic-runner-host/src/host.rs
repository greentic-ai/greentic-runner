use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::activity::{Activity, WelcomeFlowHint};
use crate::boot;
use crate::config::HostConfig;
use crate::engine::host::{SessionHost, StateHost};
use crate::engine::runtime::{FlowResumeStore, IngressEnvelope};
use crate::http::health::HealthState;
use crate::pack::PackRuntime;
use crate::runner::adapt_timer;
use crate::runner::engine::FlowEngine;
use crate::runtime::{ActivePacks, TenantRuntime};
use crate::secrets::{DynSecretsManager, default_manager};
use crate::storage::{
    DynSessionStore, DynStateStore, new_session_store, new_state_store, session_host_from,
    state_host_from,
};
use crate::wasi::RunnerWasiPolicy;
use greentic_deploy_spec::ids::{BundleId, DeploymentId, RevisionId};

#[derive(Clone, Debug)]
pub struct TelemetryCfg {
    pub config: greentic_telemetry::TelemetryConfig,
    pub export: greentic_telemetry::export::ExportConfig,
}

/// Builder for composing multi-tenant host instances.
pub struct HostBuilder {
    configs: HashMap<String, HostConfig>,
    telemetry: Option<TelemetryCfg>,
    wasi_policy: RunnerWasiPolicy,
    secrets: Option<DynSecretsManager>,
}

impl HostBuilder {
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
            telemetry: None,
            wasi_policy: RunnerWasiPolicy::default(),
            secrets: None,
        }
    }

    pub fn with_config(mut self, config: HostConfig) -> Self {
        self.configs.insert(config.tenant.clone(), config);
        self
    }

    pub fn with_telemetry(mut self, telemetry: TelemetryCfg) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    pub fn with_wasi_policy(mut self, policy: RunnerWasiPolicy) -> Self {
        self.wasi_policy = policy;
        self
    }

    pub fn with_secrets_manager(mut self, manager: DynSecretsManager) -> Self {
        self.secrets = Some(manager);
        self
    }

    pub fn build(self) -> Result<RunnerHost> {
        if self.configs.is_empty() {
            bail!("at least one tenant configuration is required");
        }
        let wasi_policy = Arc::new(self.wasi_policy);
        let configs = self
            .configs
            .into_iter()
            .map(|(tenant, cfg)| (tenant, Arc::new(cfg)))
            .collect();
        let session_store = new_session_store();
        let session_host = session_host_from(Arc::clone(&session_store));
        let state_store = new_state_store();
        let state_host = state_host_from(Arc::clone(&state_store));
        let secrets = match self.secrets {
            Some(manager) => manager,
            None => default_manager().context("failed to initialise default secrets backend")?,
        };
        Ok(RunnerHost {
            configs,
            active: Arc::new(ActivePacks::new()),
            health: Arc::new(HealthState::new()),
            session_store,
            state_store,
            session_host,
            state_host,
            wasi_policy,
            secrets_manager: secrets,
            telemetry: self.telemetry,
        })
    }
}

impl Default for HostBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Runtime host that manages tenant-bound packs and flow execution.
pub struct RunnerHost {
    configs: HashMap<String, Arc<HostConfig>>,
    active: Arc<ActivePacks>,
    health: Arc<HealthState>,
    session_store: DynSessionStore,
    state_store: DynStateStore,
    session_host: Arc<dyn SessionHost>,
    state_host: Arc<dyn StateHost>,
    wasi_policy: Arc<RunnerWasiPolicy>,
    secrets_manager: DynSecretsManager,
    telemetry: Option<TelemetryCfg>,
}

/// Handle exposing tenant internals for embedding hosts (e.g. CLI server).
#[derive(Clone)]
pub struct TenantHandle {
    runtime: Arc<TenantRuntime>,
}

impl RunnerHost {
    pub async fn start(&self) -> Result<()> {
        boot::init(&self.health, self.telemetry.as_ref())?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.active.replace(HashMap::new());
        Ok(())
    }

    pub async fn load_pack(&self, tenant: &str, pack_path: &Path) -> Result<()> {
        let archive_source = if is_pack_archive(pack_path) {
            Some(pack_path)
        } else {
            None
        };
        let runtime = self
            .prepare_runtime(tenant, pack_path, archive_source)
            .await
            .with_context(|| format!("failed to load tenant {tenant}"))?;
        self.active.insert_pack(tenant, runtime);
        tracing::info!(tenant, pack = %pack_path.display(), "pack loaded");
        Ok(())
    }

    pub async fn handle_activity(&self, tenant: &str, activity: Activity) -> Result<Vec<Activity>> {
        let runtime = self
            .active
            .load_pack(tenant)
            .with_context(|| format!("tenant {tenant} not loaded"))?;
        self.dispatch_activity(&runtime, tenant, activity).await
    }

    /// Execute an activity against a specific deployment/bundle/revision runtime.
    ///
    /// Unlike [`handle_activity`](Self::handle_activity), which resolves the
    /// tenant-only (legacy) runtime, this targets a fully-qualified revision
    /// entry inserted by [`ActivePacks::insert_revision`]. A tenant can host
    /// several concurrent revisions under a traffic split, so the legacy
    /// tenant-only lookup cannot disambiguate them — the ingress revision
    /// dispatcher selects the revision and calls this.
    ///
    /// # Session isolation contract
    ///
    /// This method runs the selected revision's runtime against **whatever
    /// session/state stores that runtime was built with** (at
    /// [`TenantRuntime::load_revision`] time). It does *not* add a revision
    /// dimension to the session key: the session/resume/state backend keys on
    /// `(env, tenant, user)` plus pack/flow, **not** on the revision. If two
    /// live revisions of the same pack for one tenant share a single session
    /// backend, a `wait`/resume snapshot created by revision A can be fetched —
    /// or clobbered — by revision B during a traffic split, retry, or
    /// rebalance, resuming a snapshot against a different flow graph.
    ///
    /// Callers that load more than one revision per tenant onto one host
    /// (i.e. every traffic-split producer) **MUST give each revision an
    /// isolated session and state store** (a per-revision store instance, or a
    /// revision-namespaced backend) when calling `load_revision`. The shared
    /// `RunnerHost` stores (`session_store()`/`state_store()`) are only safe to
    /// reuse across revisions when at most one revision is ever live per
    /// tenant. The greentic-start activation path enforces this.
    pub async fn handle_activity_for_revision(
        &self,
        tenant: &str,
        deployment_id: DeploymentId,
        bundle_id: BundleId,
        revision_id: RevisionId,
        activity: Activity,
    ) -> Result<Vec<Activity>> {
        let runtime = self
            .active
            .load_revision(tenant, deployment_id, bundle_id, revision_id)
            .with_context(|| {
                format!(
                    "revision runtime not loaded for tenant {tenant} \
                     (deployment {deployment_id}, revision {revision_id})"
                )
            })?;
        self.dispatch_activity(&runtime, tenant, activity).await
    }

    /// Shared activity-execution body: resolve the flow, build the canonical
    /// ingress envelope, run the state machine, and normalize replies. Both the
    /// legacy and revision entry points funnel through here so flow resolution
    /// and reply shaping never drift between them.
    async fn dispatch_activity(
        &self,
        runtime: &TenantRuntime,
        tenant: &str,
        activity: Activity,
    ) -> Result<Vec<Activity>> {
        let (pack_id, flow_id) = resolve_flow_id(runtime, &activity)?;
        let action = activity.action().map(|value| value.to_string());
        let session = activity.session_id().map(|value| value.to_string());
        let provider = activity.provider_id().map(|value| value.to_string());
        let messaging_endpoint_id = activity
            .messaging_endpoint_id()
            .map(|value| value.to_string());
        let channel = activity.channel().map(|value| value.to_string());
        let conversation = activity.conversation().map(|value| value.to_string());
        let user = activity.user().map(|value| value.to_string());
        let welcome_flow_hint = activity.welcome_flow_hint().cloned();
        let resolved_flow_type =
            activity
                .flow_type()
                .map(|value| value.to_string())
                .or_else(|| {
                    runtime
                        .engine()
                        .flow_by_key(&pack_id, &flow_id)
                        .map(|desc| desc.flow_type.clone())
                });
        let payload = activity.into_payload();

        let mut envelope = IngressEnvelope {
            tenant: tenant.to_string(),
            env: std::env::var("GREENTIC_ENV").ok(),
            pack_id: Some(pack_id.clone()),
            flow_id: flow_id.clone(),
            flow_type: resolved_flow_type,
            action,
            session_hint: session,
            provider,
            messaging_endpoint_id,
            channel,
            conversation,
            user,
            activity_id: None,
            timestamp: None,
            payload,
            metadata: None,
            reply_scope: None,
        }
        .canonicalize();

        let hint_flow_type = welcome_flow_hint.as_ref().and_then(|hint| {
            runtime
                .engine()
                .flow_by_key(&hint.pack_id, &hint.flow_id)
                .map(|desc| desc.flow_type.clone())
        });
        apply_welcome_flow_override(
            runtime.session_store(),
            &mut envelope,
            welcome_flow_hint.as_ref(),
            hint_flow_type,
        )?;

        let result = runtime.state_machine().handle(envelope).await?;
        Ok(normalize_replies(result, tenant))
    }

    pub async fn tenant(&self, tenant: &str) -> Option<TenantHandle> {
        self.active
            .load_pack(tenant)
            .map(|runtime| TenantHandle { runtime })
    }

    pub fn active_packs(&self) -> Arc<ActivePacks> {
        Arc::clone(&self.active)
    }

    pub fn health_state(&self) -> Arc<HealthState> {
        Arc::clone(&self.health)
    }

    pub fn wasi_policy(&self) -> Arc<RunnerWasiPolicy> {
        Arc::clone(&self.wasi_policy)
    }

    pub fn session_store(&self) -> DynSessionStore {
        Arc::clone(&self.session_store)
    }

    pub fn state_store(&self) -> DynStateStore {
        Arc::clone(&self.state_store)
    }

    pub fn session_host(&self) -> Arc<dyn SessionHost> {
        Arc::clone(&self.session_host)
    }

    pub fn state_host(&self) -> Arc<dyn StateHost> {
        Arc::clone(&self.state_host)
    }

    pub fn secrets_manager(&self) -> DynSecretsManager {
        Arc::clone(&self.secrets_manager)
    }

    pub fn tenant_configs(&self) -> HashMap<String, Arc<HostConfig>> {
        self.configs.clone()
    }

    async fn prepare_runtime(
        &self,
        tenant: &str,
        pack_path: &Path,
        archive_source: Option<&Path>,
    ) -> Result<Arc<TenantRuntime>> {
        let config = self
            .configs
            .get(tenant)
            .cloned()
            .with_context(|| format!("tenant {tenant} not registered"))?;
        if config.tenant != tenant {
            bail!(
                "tenant mismatch: config declares '{}' but '{tenant}' was requested",
                config.tenant
            );
        }
        let runtime = TenantRuntime::load(
            pack_path,
            Arc::clone(&config),
            None,
            archive_source,
            None,
            self.wasi_policy(),
            self.session_host(),
            self.session_store(),
            self.state_store(),
            self.state_host(),
            self.secrets_manager(),
        )
        .await?;
        let timers = adapt_timer::spawn_timers(Arc::clone(&runtime))?;
        runtime.register_timers(timers);
        Ok(runtime)
    }
}

impl TenantHandle {
    pub fn config(&self) -> Arc<HostConfig> {
        Arc::clone(self.runtime.config())
    }

    pub fn pack(&self) -> Arc<PackRuntime> {
        self.runtime.pack()
    }

    pub fn engine(&self) -> Arc<FlowEngine> {
        Arc::clone(self.runtime.engine())
    }

    pub fn overlays(&self) -> Vec<Arc<PackRuntime>> {
        self.runtime.overlays()
    }

    pub fn overlay_digests(&self) -> Vec<Option<String>> {
        self.runtime.overlay_digests()
    }
}

/// M1.5 welcome-flow override: swap the envelope's `(pack_id, flow_id,
/// flow_type)` to the producer-supplied [`WelcomeFlowHint`] when ALL of:
/// the hint is present, the envelope carries a `messaging_endpoint_id`,
/// the welcome-seen marker is absent for this `(tenant, env, eid, user)`
/// (set atomically on success), and `FlowResumeStore::fetch` finds no
/// active wait snapshot. Any missing precondition is a silent no-op.
///
/// **The welcome-seen marker is the durable first-contact gate.** Without
/// it, post-completion / no-wait / TTL-expired turns would re-fire welcome
/// because the wait-snapshot check is only positive while a flow is paused.
/// The marker lives in the shared session store under a synthetic scope
/// (`welcome-seen::ep=<eid>`) distinct from the flow's own conversation, so
/// flow-completion `clear_wait` does NOT drop it.
///
/// The wait-snapshot check is kept as a belt-and-braces safety net: the
/// marker check + write is two operations against the store with a small
/// race window (Phase D will add an atomic `register_wait_if_absent`).
/// The safety net guarantees an in-flight flow is never overridden even if
/// two concurrent first-ever turns both pass the marker probe.
///
/// `session_store` + `hint_flow_type` are passed as primitives so the logic
/// is unit-testable without a `TenantRuntime`; the caller does the engine
/// lookup that produces `hint_flow_type`.
fn apply_welcome_flow_override(
    session_store: &DynSessionStore,
    envelope: &mut IngressEnvelope,
    hint: Option<&WelcomeFlowHint>,
    hint_flow_type: Option<String>,
) -> Result<()> {
    let Some(hint) = hint else {
        return Ok(());
    };
    if envelope.messaging_endpoint_id.is_none() {
        return Ok(());
    }

    let marker = WelcomeContactStore::new(Arc::clone(session_store));
    if !marker.try_mark_first_contact(envelope)? {
        return Ok(());
    }

    let resume = FlowResumeStore::new(Arc::clone(session_store));
    let snapshot = resume
        .fetch(envelope)
        .map_err(|err| anyhow!("welcome-flow first-contact probe failed: {err}"))?;
    if snapshot.is_some() {
        return Ok(());
    }

    envelope.pack_id = Some(hint.pack_id.clone());
    envelope.flow_id = hint.flow_id.clone();
    envelope.flow_type = hint_flow_type;
    Ok(())
}

/// Persistent per-(tenant, env, eid, user) welcome-seen marker, layered on
/// the existing `SessionStore` trait so both in-memory and Redis backends
/// inherit it without a trait surface change.
///
/// The marker is registered as a TTL-less wait under a synthetic
/// [`ReplyScope`] keyed on the endpoint id. That scope is deliberately
/// disjoint from any flow's own conversation scope so the engine's
/// `clear_wait` (called on flow completion + on TTL expiry) cannot drop the
/// marker. The marker outlives the flow's wait by design.
struct WelcomeContactStore {
    store: DynSessionStore,
}

impl WelcomeContactStore {
    fn new(store: DynSessionStore) -> Self {
        Self { store }
    }

    /// Returns `true` iff this turn is true first contact: no marker
    /// existed and one has now been persisted. Returns `false` if the
    /// marker is already present (subsequent turn — override MUST NOT
    /// fire), if the envelope lacks a `messaging_endpoint_id` (no marker
    /// key derivable), or if any precondition on the store fails.
    ///
    /// Race window: check + mark is two store calls, not one atomic CAS.
    /// Two concurrent first-ever turns can both observe "no marker" and
    /// both fire welcome once — bounded harm, mitigated downstream by the
    /// wait-snapshot safety net in [`apply_welcome_flow_override`]. A real
    /// atomic primitive (`register_wait_if_absent`) is Phase D.
    fn try_mark_first_contact(&self, envelope: &IngressEnvelope) -> Result<bool> {
        let Some(scope) = welcome_marker_scope(envelope) else {
            return Ok(false);
        };
        let (ctx, user) = FlowResumeStore::contact_identity(envelope)
            .map_err(|e| anyhow!("welcome marker identity probe failed: {e}"))?;

        if self
            .store
            .find_wait_by_scope(&ctx, &user, &scope)
            .map_err(|e| anyhow!("welcome marker probe failed: {e}"))?
            .is_some()
        {
            return Ok(false);
        }

        let data = marker_session_data(&ctx, &user)?;
        let session_key =
            greentic_session::SessionKey::new(format!("welcome-marker::{}", scope.conversation));
        self.store
            .register_wait(&ctx, &user, &scope, &session_key, data, None)
            .map_err(|e| anyhow!("welcome marker register failed: {e}"))?;
        Ok(true)
    }
}

/// Synthetic [`ReplyScope`] keyed on `messaging_endpoint_id` so the marker
/// is partitioned per-endpoint AND disjoint from any real conversation
/// scope. Returns `None` when the envelope lacks an eid — the marker has
/// no meaningful bucket then, and the caller exits early.
fn welcome_marker_scope(envelope: &IngressEnvelope) -> Option<greentic_types::ReplyScope> {
    let eid = envelope.messaging_endpoint_id.as_deref()?;
    Some(greentic_types::ReplyScope {
        conversation: format!("welcome-seen::ep={eid}"),
        thread: None,
        reply_to: None,
        correlation: None,
    })
}

/// Minimal `SessionData` for the marker. The store accepts any record
/// aligned with `(ctx, user)`; the marker carries no flow semantics, so
/// the placeholder `flow_id`/`pack_id` is fixed and validates as an
/// identifier (ascii + `.`/`-`/`_`).
fn marker_session_data(
    ctx: &greentic_types::TenantCtx,
    user: &greentic_types::UserId,
) -> Result<greentic_session::SessionData> {
    use std::str::FromStr;
    let flow_id = greentic_types::FlowId::from_str("welcome-marker")
        .map_err(|e| anyhow!("welcome marker flow id: {e}"))?;
    let pack_id = greentic_types::PackId::from_str("welcome-marker")
        .map_err(|e| anyhow!("welcome marker pack id: {e}"))?;
    let cursor = greentic_types::SessionCursor::new("marker".to_string());
    let ctx = ctx.clone().with_user(Some(user.clone()));
    Ok(greentic_session::SessionData {
        tenant_ctx: ctx,
        flow_id,
        pack_id: Some(pack_id),
        cursor,
        context_json: "{}".to_string(),
    })
}

fn resolve_flow_id(runtime: &TenantRuntime, activity: &Activity) -> Result<(String, String)> {
    let engine = runtime.engine();
    if let Some(flow_id) = activity.flow_id() {
        if let Some(pack_id) = activity.pack_id() {
            if engine.flow_by_key(pack_id, flow_id).is_none() {
                bail!("flow {flow_id} not registered for pack {pack_id}");
            }
            return Ok((pack_id.to_string(), flow_id.to_string()));
        }
        if let Some(flow) = engine.flow_by_id(flow_id) {
            return Ok((flow.pack_id.clone(), flow.id.clone()));
        }
        bail!("flow {flow_id} is ambiguous; pack_id is required");
    }

    if let Some(flow_type) = activity.flow_type() {
        if let Some(pack_id) = activity.pack_id() {
            if let Some(flow) = engine
                .flows()
                .iter()
                .find(|flow| flow.pack_id == pack_id && flow.flow_type == flow_type)
            {
                return Ok((pack_id.to_string(), flow.id.clone()));
            }
            bail!("flow type {flow_type} not registered for pack {pack_id}");
        }
        if let Some(flow) = engine.flow_by_type(flow_type) {
            return Ok((flow.pack_id.clone(), flow.id.clone()));
        }
        bail!("flow type {flow_type} is ambiguous; pack_id is required");
    }

    let pack = runtime.pack();
    let flow_id = pack
        .metadata()
        .entry_flows
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no entry flows registered for tenant {}", runtime.tenant()))?;
    Ok((pack.metadata().pack_id.clone(), flow_id))
}

fn normalize_replies(result: Value, tenant: &str) -> Vec<Activity> {
    result
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![result])
        .into_iter()
        .map(|payload| Activity::from_output(payload, tenant))
        .collect()
}

fn is_pack_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("gtpack"))
        .unwrap_or(false)
}

#[cfg(test)]
mod welcome_flow_tests {
    use super::*;
    use crate::engine::runtime::IngressEnvelope;
    use crate::runner::engine::{ExecutionState, FlowSnapshot, FlowWait};
    use crate::storage::new_session_store;
    use greentic_types::ReplyScope;
    use serde_json::json;

    fn sample_envelope(endpoint_id: Option<&str>) -> IngressEnvelope {
        IngressEnvelope {
            tenant: "demo".into(),
            env: Some("local".into()),
            pack_id: Some("pack.default".into()),
            flow_id: "flow.default".into(),
            flow_type: Some("messaging".into()),
            action: Some("messaging".into()),
            session_hint: None,
            provider: Some("teams".into()),
            messaging_endpoint_id: endpoint_id.map(String::from),
            channel: Some("chan".into()),
            conversation: Some("conv".into()),
            user: Some("user-1".into()),
            activity_id: None,
            timestamp: None,
            payload: json!({}),
            metadata: None,
            reply_scope: Some(ReplyScope {
                conversation: "conv".into(),
                thread: None,
                reply_to: None,
                correlation: None,
            }),
        }
        .canonicalize()
    }

    fn hint() -> WelcomeFlowHint {
        WelcomeFlowHint {
            pack_id: "pack.welcome".into(),
            flow_id: "flow.welcome".into(),
        }
    }

    fn seed_resume(store: &DynSessionStore, envelope: &IngressEnvelope) {
        // Plant a snapshot in the exact bucket `fetch` would query so the
        // next call resolves to a resume — proves the override skips when a
        // session already exists.
        let resume = FlowResumeStore::new(Arc::clone(store));
        let state: ExecutionState = serde_json::from_value(json!({
            "input": { "text": "hi" },
            "nodes": {},
            "egress": []
        }))
        .expect("state");
        let wait = FlowWait {
            reason: Some("await-user".into()),
            snapshot: FlowSnapshot {
                pack_id: envelope.pack_id.clone().expect("pack_id"),
                flow_id: envelope.flow_id.clone(),
                next_flow: None,
                next_node: "node-2".into(),
                state,
            },
        };
        resume.save(envelope, &wait).expect("seed save");
    }

    #[test]
    fn override_is_no_op_when_hint_absent() {
        // Pre-M1.5 producers don't attach a hint — flow resolution must
        // stay exactly the same.
        let store = new_session_store();
        let mut envelope = sample_envelope(Some("teams-legal"));
        let before = envelope.clone();
        apply_welcome_flow_override(&store, &mut envelope, None, None).expect("ok");
        assert_eq!(envelope.pack_id, before.pack_id);
        assert_eq!(envelope.flow_id, before.flow_id);
        assert_eq!(envelope.flow_type, before.flow_type);
    }

    #[test]
    fn override_is_no_op_when_endpoint_id_absent() {
        // Non-messaging traffic carries no endpoint id and must never hit
        // the welcome-flow path even if the hint is somehow set.
        let store = new_session_store();
        let mut envelope = sample_envelope(None);
        let before = envelope.clone();
        apply_welcome_flow_override(&store, &mut envelope, Some(&hint()), Some("welcome".into()))
            .expect("ok");
        assert_eq!(envelope.pack_id, before.pack_id);
        assert_eq!(envelope.flow_id, before.flow_id);
    }

    #[test]
    fn override_swaps_pack_flow_and_threads_flow_type_through() {
        // Both axes covered: when the caller pre-resolved the welcome
        // flow's type, it lands on the envelope; when the resolver
        // returned None (unknown flow in engine), it lands as None and
        // downstream resolution defaults take over.
        for hint_flow_type in [Some("welcome".to_string()), None] {
            let store = new_session_store();
            let mut envelope = sample_envelope(Some("teams-legal"));
            apply_welcome_flow_override(
                &store,
                &mut envelope,
                Some(&hint()),
                hint_flow_type.clone(),
            )
            .expect("ok");
            assert_eq!(envelope.pack_id.as_deref(), Some("pack.welcome"));
            assert_eq!(envelope.flow_id, "flow.welcome");
            assert_eq!(envelope.flow_type, hint_flow_type);
        }
    }

    #[test]
    fn override_is_no_op_on_repeat_turn_with_existing_session() {
        // Resume path: an already-active session in the same bucket means
        // this isn't first contact. The user must continue on the resumed
        // flow, NOT be redirected to the welcome flow.
        let store = new_session_store();
        let envelope_template = sample_envelope(Some("teams-legal"));
        seed_resume(&store, &envelope_template);

        let mut envelope = envelope_template.clone();
        apply_welcome_flow_override(&store, &mut envelope, Some(&hint()), Some("welcome".into()))
            .expect("ok");
        assert_eq!(envelope.pack_id, envelope_template.pack_id);
        assert_eq!(envelope.flow_id, envelope_template.flow_id);
        assert_eq!(envelope.flow_type, envelope_template.flow_type);
    }

    #[test]
    fn override_is_no_op_post_completion_when_marker_present() {
        // POST-COMPLETION REGRESSION GUARD (Codex #201): the welcome-seen
        // marker is durable and survives flow completion. After welcome
        // fires once + the flow finishes (wait cleared), the next turn
        // must NOT re-fire welcome — the marker is the gate, not the
        // active-wait snapshot.
        let store = new_session_store();
        let mut first = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut first, Some(&hint()), Some("welcome".into()))
            .expect("first turn ok");
        assert_eq!(
            first.pack_id.as_deref(),
            Some("pack.welcome"),
            "first turn fires welcome"
        );

        // Simulate the welcome flow completing: the engine clears the
        // wait at end-of-flow (mirror `FlowResumeStore::clear`). The
        // marker must NOT be in the wait scope, so this clear has no
        // effect on the marker.
        let resume = FlowResumeStore::new(Arc::clone(&store));
        resume.clear(&first).expect("clear post-completion wait");

        // Second turn arrives — producer still attaches the hint (it
        // does not know flow-completion happened). The marker keeps the
        // override off.
        let mut second = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut second, Some(&hint()), Some("welcome".into()))
            .expect("second turn ok");
        assert_eq!(
            second.pack_id.as_deref(),
            Some("pack.default"),
            "second turn must NOT re-fire welcome"
        );
        assert_eq!(second.flow_id, "flow.default");
    }

    #[test]
    fn override_is_no_op_on_second_turn_after_marker_set() {
        // No-wait variant of the post-completion test: a welcome flow
        // without `session.wait` leaves no snapshot AT ALL. Marker is the
        // only thing standing between turn 2 and a welcome re-fire.
        let store = new_session_store();
        let mut first = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut first, Some(&hint()), Some("welcome".into()))
            .expect("first turn ok");
        assert_eq!(first.pack_id.as_deref(), Some("pack.welcome"));

        let mut second = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut second, Some(&hint()), Some("welcome".into()))
            .expect("second turn ok");
        assert_eq!(
            second.pack_id.as_deref(),
            Some("pack.default"),
            "second turn must NOT re-fire welcome"
        );
    }

    #[test]
    fn override_partitions_marker_per_endpoint() {
        // The marker is keyed by `(tenant, env, eid, user)` — a user
        // marked seen on `teams-legal` is still first contact on
        // `teams-accounting`. Welcome must fire independently on each
        // endpoint.
        let store = new_session_store();
        let mut legal = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut legal, Some(&hint()), Some("welcome".into()))
            .expect("legal first turn ok");
        assert_eq!(legal.pack_id.as_deref(), Some("pack.welcome"));

        let mut accounting = sample_envelope(Some("teams-accounting"));
        apply_welcome_flow_override(
            &store,
            &mut accounting,
            Some(&hint()),
            Some("welcome".into()),
        )
        .expect("accounting first turn ok");
        assert_eq!(
            accounting.pack_id.as_deref(),
            Some("pack.welcome"),
            "different endpoint = independent first contact"
        );
    }

    #[test]
    fn marker_is_not_written_when_hint_absent() {
        // Marker writes are gated on the hint+eid preconditions — a
        // pre-M1.5 turn (no hint) MUST NOT leak a marker, otherwise a
        // producer that later enables welcome would treat that user as
        // already-contacted and never fire the override.
        //
        // Mid-conversation users (active-wait safety net path) DO get a
        // marker — that's deliberate: they don't get retroactive welcomes.
        // This test only guards the no-hint gate.
        let store = new_session_store();
        let mut envelope = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut envelope, None, None).expect("ok");

        let mut next = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut next, Some(&hint()), Some("welcome".into()))
            .expect("ok");
        assert_eq!(
            next.pack_id.as_deref(),
            Some("pack.welcome"),
            "no marker leaked from hint-absent path"
        );
    }
}
