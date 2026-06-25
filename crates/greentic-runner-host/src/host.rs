use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::activity::Activity;
use crate::boot;
use crate::config::{Fast2FlowRoutingConfig, HostConfig};
use crate::engine::host::{SessionHost, StateHost};
use crate::engine::runtime::IngressEnvelope;
#[cfg(feature = "greentic-x-provider")]
use crate::greentic_x_provider::RunnerPackFast2FlowRoutingProvider;
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
#[cfg(feature = "greentic-x-provider")]
use greentic_x_runtime::{Fast2FlowMessageEnvelope, Fast2FlowRouteRequest};
#[cfg(feature = "greentic-x-provider")]
use serde::Deserialize;

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
        let mut next = (*self.active.snapshot()).clone();
        next.insert(tenant.to_string(), runtime);
        self.active.replace(next);
        tracing::info!(tenant, pack = %pack_path.display(), "pack loaded");
        Ok(())
    }

    pub async fn handle_activity(&self, tenant: &str, activity: Activity) -> Result<Vec<Activity>> {
        let runtime = self
            .active
            .load(tenant)
            .with_context(|| format!("tenant {tenant} not loaded"))?;
        let activity = apply_fast2flow_routing(&runtime, tenant, activity)?;
        if activity.action() == Some("response") && activity.flow_id().is_none() {
            return Ok(vec![activity]);
        }
        let (pack_id, flow_id) = resolve_flow_id(&runtime, &activity)?;
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
            .load(tenant)
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

fn apply_fast2flow_routing(
    runtime: &TenantRuntime,
    tenant: &str,
    activity: Activity,
) -> Result<Activity> {
    let config = &runtime.config().fast2flow;
    if !config.enabled || activity.flow_id().is_some() {
        return Ok(activity);
    }
    apply_fast2flow_routing_enabled(runtime, tenant, activity, config)
}

#[cfg(feature = "greentic-x-provider")]
fn apply_fast2flow_routing_enabled(
    runtime: &TenantRuntime,
    tenant: &str,
    activity: Activity,
    config: &Fast2FlowRoutingConfig,
) -> Result<Activity> {
    let Some(text) = activity.payload().get("text").and_then(Value::as_str) else {
        return Ok(activity);
    };
    if text.trim().is_empty() {
        return Ok(activity);
    }

    let mut envelope = Fast2FlowMessageEnvelope::new(text.trim().to_owned());
    if let Some(channel) = activity.channel() {
        envelope = envelope.with_channel(channel.to_owned());
    }
    if let Some(provider) = activity.provider_id() {
        envelope = envelope.with_provider(provider.to_owned());
    }
    let request = Fast2FlowRouteRequest {
        scope: config.scope.clone().unwrap_or_else(|| tenant.to_owned()),
        envelope,
        session_active: activity.session_id().is_some(),
        input_locale: "en".to_owned(),
        time_budget_ms: config.time_budget_ms,
        registry_path: config.registry_path.clone(),
        indexes_path: config.indexes_path.clone(),
        now_unix_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        metadata: Default::default(),
    };
    let provider = RunnerPackFast2FlowRoutingProvider::new(runtime.pack())
        .map_err(|err| anyhow!(err.to_string()))?
        .with_component_ref(config.component_ref.clone())
        .with_operation(config.operation.clone())
        .with_tenant(tenant.to_owned());
    let route = provider
        .route_intent_value(request)
        .map_err(|err| anyhow!(err.to_string()))?;
    let route: RunnerFast2FlowRouteResult = serde_json::from_value(route)
        .map_err(|err| anyhow!("failed to decode Fast2Flow route result: {err}"))?;

    match route.directive {
        RunnerFast2FlowDirective::Continue => Ok(activity),
        RunnerFast2FlowDirective::Dispatch {
            target,
            flow_type,
            entities,
        } => apply_fast2flow_target(activity, &target, flow_type, entities),
        RunnerFast2FlowDirective::Respond { message } => Ok(Activity::custom(
            "response",
            serde_json::json!({ "messages": [{ "text": message }] }),
        )
        .ensure_tenant(tenant)),
        RunnerFast2FlowDirective::Deny { reason } => Ok(Activity::custom(
            "response",
            serde_json::json!({ "messages": [{ "text": reason }] }),
        )
        .ensure_tenant(tenant)),
    }
}

#[cfg(feature = "greentic-x-provider")]
#[derive(Debug, Deserialize)]
struct RunnerFast2FlowRouteResult {
    directive: RunnerFast2FlowDirective,
}

#[cfg(feature = "greentic-x-provider")]
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RunnerFast2FlowDirective {
    Continue,
    Dispatch {
        target: String,
        #[serde(default)]
        flow_type: RunnerFast2FlowFlowType,
        #[serde(default)]
        entities: Vec<greentic_x_runtime::Fast2FlowRoutingEntity>,
    },
    Respond {
        message: String,
    },
    Deny {
        reason: String,
    },
}

#[cfg(feature = "greentic-x-provider")]
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunnerFast2FlowFlowType {
    #[default]
    Deterministic,
    Agentic,
}

#[cfg(not(feature = "greentic-x-provider"))]
fn apply_fast2flow_routing_enabled(
    _runtime: &TenantRuntime,
    _tenant: &str,
    _activity: Activity,
    _config: &Fast2FlowRoutingConfig,
) -> Result<Activity> {
    bail!("fast2flow routing requires the greentic-x-provider feature")
}

#[cfg(feature = "greentic-x-provider")]
fn apply_fast2flow_target(
    activity: Activity,
    target: &str,
    flow_type: RunnerFast2FlowFlowType,
    entities: Vec<greentic_x_runtime::Fast2FlowRoutingEntity>,
) -> Result<Activity> {
    let target = target.trim();
    if target.is_empty() {
        bail!("fast2flow dispatch target is empty");
    }
    let activity = match flow_type {
        RunnerFast2FlowFlowType::Deterministic => activity.with_flow_type("deterministic"),
        RunnerFast2FlowFlowType::Agentic => activity.with_flow_type("agentic"),
    };
    if let Some((pack_id, flow_id)) = target.split_once('/') {
        if pack_id.trim().is_empty() || flow_id.trim().is_empty() {
            bail!("fast2flow dispatch target `{target}` must be `pack_id/flow_id` or `flow_id`");
        }
        return Ok(attach_fast2flow_entities(
            activity.with_pack(pack_id.trim()).with_flow(flow_id.trim()),
            entities,
        ));
    }
    Ok(attach_fast2flow_entities(
        activity.with_flow(target),
        entities,
    ))
}

#[cfg(feature = "greentic-x-provider")]
fn attach_fast2flow_entities(
    activity: Activity,
    entities: Vec<greentic_x_runtime::Fast2FlowRoutingEntity>,
) -> Activity {
    if entities.is_empty() {
        return activity;
    }
    activity.with_payload_field(
        "fast2flow",
        serde_json::json!({
            "entities": entities,
        }),
    )
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

#[cfg(all(test, feature = "greentic-x-provider"))]
mod fast2flow_tests {
    use greentic_x_runtime::Fast2FlowRoutingEntity;

    use super::*;

    #[test]
    fn dispatch_target_attaches_flow_type_and_prefill_entities_to_payload() {
        let activity = Activity::text("show traffic tomorrow");
        let routed = apply_fast2flow_target(
            activity,
            "telco-x/prefix-traffic",
            RunnerFast2FlowFlowType::Agentic,
            vec![Fast2FlowRoutingEntity::new("date", "20260611").with_format("iso", "2026-06-11")],
        )
        .expect("target should route");

        assert_eq!(routed.pack_id(), Some("telco-x"));
        assert_eq!(routed.flow_id(), Some("prefix-traffic"));
        assert_eq!(routed.flow_type(), Some("agentic"));
        assert_eq!(
            routed.payload()["fast2flow"]["entities"][0]["normalized"],
            "20260611"
        );
        assert_eq!(
            routed.payload()["fast2flow"]["entities"][0]["formats"]["iso"],
            "2026-06-11"
        );
    }
}
