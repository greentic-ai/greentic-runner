use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::activity::Activity;
use crate::boot;
use crate::config::HostConfig;
use crate::engine::host::{SessionHost, StateHost};
use crate::engine::runtime::IngressEnvelope;
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
        let channel = activity.channel().map(|value| value.to_string());
        let conversation = activity.conversation().map(|value| value.to_string());
        let user = activity.user().map(|value| value.to_string());
        let flow_type = activity
            .flow_type()
            .map(|value| value.to_string())
            .or_else(|| {
                runtime
                    .engine()
                    .flow_by_key(&pack_id, &flow_id)
                    .map(|desc| desc.flow_type.clone())
            });
        let payload = activity.into_payload();

        let envelope = IngressEnvelope {
            tenant: tenant.to_string(),
            env: std::env::var("GREENTIC_ENV").ok(),
            pack_id: Some(pack_id.clone()),
            flow_id: flow_id.clone(),
            flow_type,
            action,
            session_hint: session,
            provider,
            // M1.4: producer wires from the receiving provider instance.
            // Greentic-start's ingress dispatcher (M1.4c) populates this;
            // the runner-side seam carries None until that lands.
            messaging_endpoint_id: None,
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
