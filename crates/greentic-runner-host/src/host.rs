use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::activity::Activity;
use crate::config::HostConfig;
use crate::pack::PackRuntime;
use crate::runner::engine::{FlowContext, FlowEngine};
use crate::runner::mocks::MockLayer;

#[cfg(feature = "telemetry")]
pub use greentic_telemetry::OtlpConfig as TelemetryCfg;
#[cfg(feature = "telemetry")]
use greentic_telemetry::init_otlp;

/// Builder for composing multi-tenant host instances.
pub struct HostBuilder {
    configs: HashMap<String, HostConfig>,
    #[cfg(feature = "telemetry")]
    telemetry: Option<TelemetryCfg>,
}

impl HostBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            configs: HashMap::new(),
            #[cfg(feature = "telemetry")]
            telemetry: None,
        }
    }

    /// Register a tenant configuration.
    pub fn with_config(mut self, config: HostConfig) -> Self {
        self.configs.insert(config.tenant.clone(), config);
        self
    }

    /// Attach telemetry configuration (requires the `telemetry` feature).
    #[cfg(feature = "telemetry")]
    pub fn with_telemetry(mut self, telemetry: TelemetryCfg) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Build a [`RunnerHost`].
    pub fn build(self) -> Result<RunnerHost> {
        if self.configs.is_empty() {
            bail!("at least one tenant configuration is required");
        }
        let configs = self
            .configs
            .into_iter()
            .map(|(tenant, cfg)| (tenant, Arc::new(cfg)))
            .collect();
        Ok(RunnerHost {
            configs,
            tenants: RwLock::new(HashMap::new()),
            #[cfg(feature = "telemetry")]
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
    tenants: RwLock<HashMap<String, Arc<TenantRuntime>>>,
    #[cfg(feature = "telemetry")]
    telemetry: Option<TelemetryCfg>,
}

struct TenantRuntime {
    config: Arc<HostConfig>,
    pack: Arc<PackRuntime>,
    engine: Arc<FlowEngine>,
    mocks: Option<Arc<MockLayer>>,
}

/// Handle exposing tenant internals for embedding hosts (e.g. CLI server).
#[derive(Clone)]
pub struct TenantHandle {
    config: Arc<HostConfig>,
    pack: Arc<PackRuntime>,
    engine: Arc<FlowEngine>,
}

impl RunnerHost {
    /// Initialise telemetry sinks (no-op when the `telemetry` feature is disabled).
    pub async fn start(&self) -> Result<()> {
        #[cfg(feature = "telemetry")]
        if let Some(cfg) = &self.telemetry {
            init_otlp(cfg.clone(), Vec::new()).map_err(|err| anyhow!(err.to_string()))?;
        }
        Ok(())
    }

    /// Drop loaded packs and release resources.
    pub async fn stop(&self) -> Result<()> {
        self.tenants.write().await.clear();
        Ok(())
    }

    /// Load a pack for the given tenant.
    pub async fn load_pack(&self, tenant: &str, pack_path: &Path) -> Result<()> {
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

        let pack = Arc::new(
            PackRuntime::load(pack_path, Arc::clone(&config), None, None)
                .await
                .with_context(|| format!("failed to load pack {}", pack_path.display()))?,
        );
        let engine = Arc::new(
            FlowEngine::new(Arc::clone(&pack), Arc::clone(&config))
                .await
                .context("failed to prime flow engine")?,
        );
        let runtime = Arc::new(TenantRuntime {
            config,
            pack,
            engine,
            mocks: None,
        });
        self.tenants
            .write()
            .await
            .insert(tenant.to_string(), runtime);
        tracing::info!(tenant, pack = %pack_path.display(), "pack loaded");
        Ok(())
    }

    /// Dispatch an activity to the appropriate flow and capture responses.
    pub async fn handle_activity(&self, tenant: &str, activity: Activity) -> Result<Vec<Activity>> {
        let runtime = self
            .tenants
            .read()
            .await
            .get(tenant)
            .cloned()
            .with_context(|| format!("tenant {tenant} not loaded"))?;
        let flow_id = resolve_flow_id(&runtime, &activity)?;
        let action = activity.action().map(|value| value.to_string());
        let session = activity.session_id().map(|value| value.to_string());
        let provider = activity.provider_id().map(|value| value.to_string());
        let payload = activity.into_payload();

        let result = runtime
            .engine
            .execute(
                FlowContext {
                    tenant,
                    flow_id: &flow_id,
                    node_id: None,
                    tool: None,
                    action: action.as_deref(),
                    session_id: session.as_deref(),
                    provider_id: provider.as_deref(),
                    retry_config: runtime.config.mcp_retry_config().into(),
                    observer: None,
                    mocks: runtime.mocks.as_deref(),
                },
                payload,
            )
            .await?;

        Ok(normalize_replies(result, tenant))
    }

    /// Retrieve a handle to the tenant runtime.
    pub async fn tenant(&self, tenant: &str) -> Option<TenantHandle> {
        self.tenants
            .read()
            .await
            .get(tenant)
            .map(|runtime| TenantHandle {
                config: Arc::clone(&runtime.config),
                pack: Arc::clone(&runtime.pack),
                engine: Arc::clone(&runtime.engine),
            })
    }
}

impl TenantHandle {
    /// Borrow the tenant configuration.
    pub fn config(&self) -> Arc<HostConfig> {
        Arc::clone(&self.config)
    }

    /// Borrow the loaded pack runtime.
    pub fn pack(&self) -> Arc<PackRuntime> {
        Arc::clone(&self.pack)
    }

    /// Borrow the flow engine.
    pub fn engine(&self) -> Arc<FlowEngine> {
        Arc::clone(&self.engine)
    }
}

fn resolve_flow_id(runtime: &TenantRuntime, activity: &Activity) -> Result<String> {
    if let Some(flow_id) = activity.flow_id() {
        return Ok(flow_id.to_string());
    }

    let flow_type = activity
        .flow_type()
        .ok_or_else(|| anyhow!("activity is missing a flow type hint"))?;
    runtime
        .engine
        .flow_by_type(flow_type)
        .map(|descriptor| descriptor.id.clone())
        .ok_or_else(|| anyhow!("no flow registered for type {flow_type}"))
}

fn normalize_replies(value: Value, tenant: &str) -> Vec<Activity> {
    match value {
        Value::Array(values) => values
            .into_iter()
            .map(|payload| Activity::from_output(payload, tenant))
            .collect(),
        other => vec![Activity::from_output(other, tenant)],
    }
}
