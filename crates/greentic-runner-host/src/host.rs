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
/// flow_type)` over to the producer-supplied [`WelcomeFlowHint`] before the
/// state machine sees it, when **all** three preconditions hold:
///
/// 1. The producer attached a hint
/// 2. The envelope carries a `messaging_endpoint_id`
/// 3. `FlowResumeStore::fetch` finds no active wait snapshot for this
///    envelope in this pack's session bucket
///
/// Any missing precondition is a silent no-op.
///
/// # Important: this is a safety net, NOT first-contact detection
///
/// `FlowResumeStore::fetch` only looks up **active wait snapshots**. A flow
/// that completed (or completed without ever calling `session.wait`) leaves
/// NO marker — so the wait-lookup returning `None` does NOT prove this is
/// the user's first turn on the endpoint. A post-completion turn would pass
/// this check and re-fire welcome.
///
/// Preventing that welcome-loop is the **producer's** responsibility — see
/// [`WelcomeFlowHint`]. The producer (greentic-start) is expected to
/// consult a durable welcome-seen marker before attaching the hint and only
/// attach it on actual first contact. The wait-check here just adds a
/// belt-and-braces safety net against accidentally re-routing a
/// mid-conversation turn (where a wait IS active).
///
/// Takes the session store + pre-resolved `hint_flow_type` as primitives so
/// the logic is unit-testable without a `TenantRuntime`. The caller is
/// responsible for the engine lookup that produces `hint_flow_type`.
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
    fn override_swaps_pack_flow_and_type_on_first_contact() {
        let store = new_session_store();
        let mut envelope = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut envelope, Some(&hint()), Some("welcome".into()))
            .expect("ok");
        assert_eq!(envelope.pack_id.as_deref(), Some("pack.welcome"));
        assert_eq!(envelope.flow_id, "flow.welcome");
        assert_eq!(envelope.flow_type.as_deref(), Some("welcome"));
    }

    #[test]
    fn override_clears_flow_type_when_hint_lookup_unresolved() {
        // If the producer's resolver can't find the welcome flow's type
        // (unknown flow in engine), the hint's flow_type ends up None —
        // downstream resolution defaults take over.
        let store = new_session_store();
        let mut envelope = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut envelope, Some(&hint()), None).expect("ok");
        assert_eq!(envelope.pack_id.as_deref(), Some("pack.welcome"));
        assert!(envelope.flow_type.is_none());
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
    fn override_documented_limit_post_completion_re_fires_welcome() {
        // CONTRACT: the runner-host's wait-lookup is NOT a first-contact
        // probe — it only finds active wait snapshots. A completed flow
        // leaves no marker, so a turn after completion will pass this
        // check and re-fire welcome. This test documents the limit so the
        // producer side (greentic-start) knows to consult its own
        // welcome-seen marker before attaching the hint.
        let store = new_session_store();
        let envelope = sample_envelope(Some("teams-legal"));
        seed_resume(&store, &envelope);
        // Producer-style cleanup: the flow finished and its wait was
        // cleared by the resume mechanism (mirror what `FlowResumeStore::
        // clear` does after a successful resume).
        let resume = FlowResumeStore::new(Arc::clone(&store));
        resume.clear(&envelope).expect("clear post-completion wait");

        // Now a subsequent turn arrives. The hint is still attached
        // (producer didn't check welcome-seen). Override fires — this is
        // the buggy welcome-loop the producer must prevent.
        let mut next = sample_envelope(Some("teams-legal"));
        apply_welcome_flow_override(&store, &mut next, Some(&hint()), Some("welcome".into()))
            .expect("ok");
        assert_eq!(next.pack_id.as_deref(), Some("pack.welcome"));
        assert_eq!(next.flow_id, "flow.welcome");
    }

    #[test]
    fn override_partitions_first_contact_per_endpoint() {
        // A user who has talked to `teams-legal` is first contact on
        // `teams-accounting` (endpoint partitions the session bucket via
        // `ep=<eid>::` prefix at canonicalize). Welcome flow must fire on
        // the second endpoint even though the user is known on the first.
        let store = new_session_store();
        let legal = sample_envelope(Some("teams-legal"));
        seed_resume(&store, &legal);

        let mut accounting = sample_envelope(Some("teams-accounting"));
        apply_welcome_flow_override(
            &store,
            &mut accounting,
            Some(&hint()),
            Some("welcome".into()),
        )
        .expect("ok");
        assert_eq!(accounting.pack_id.as_deref(), Some("pack.welcome"));
        assert_eq!(accounting.flow_id, "flow.welcome");
    }
}
