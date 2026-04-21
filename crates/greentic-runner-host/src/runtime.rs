use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use reqwest::Client;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

use crate::config::HostConfig;
use crate::engine::host::{SessionHost, StateHost};
use crate::engine::runtime::StateMachineRuntime;
use crate::oauth::{OAuthBrokerConfig, request_resource_token};
use crate::operator_metrics::OperatorMetrics;
use crate::operator_registry::OperatorRegistry;
use crate::pack::{ComponentResolution, PackRuntime};
use crate::runner::adapt_events_email::{
    EmailExecutionPlan, EmailSendRequest, build_email_execution_plan, execute_email_request,
};
use crate::runner::contract_cache::{ContractCache, ContractCacheStats};
use crate::runner::engine::FlowEngine;
use crate::runner::mocks::MockLayer;
use crate::secrets::{DynSecretsManager, canonicalize_secret_key, read_secret_blocking};
use crate::storage::session::DynSessionStore;
use crate::storage::state::DynStateStore;
use crate::trace::PackTraceInfo;
use crate::wasi::RunnerWasiPolicy;
use greentic_types::SecretRequirement;

const RUNTIME_SECRETS_PACK_ID: &str = "_runner";

/// Atomically swapped view of live tenant runtimes.
pub struct ActivePacks {
    inner: ArcSwap<HashMap<String, Arc<TenantRuntime>>>,
}

impl ActivePacks {
    pub fn new() -> Self {
        Self {
            inner: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    pub fn load(&self, tenant: &str) -> Option<Arc<TenantRuntime>> {
        self.inner.load().get(tenant).cloned()
    }

    pub fn snapshot(&self) -> Arc<HashMap<String, Arc<TenantRuntime>>> {
        self.inner.load_full()
    }

    pub fn replace(&self, next: HashMap<String, Arc<TenantRuntime>>) {
        self.inner.store(Arc::new(next));
    }

    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ActivePacks {
    fn default() -> Self {
        Self::new()
    }
}

/// Runtime bundle for a tenant pack.
pub struct TenantRuntime {
    tenant: String,
    config: Arc<HostConfig>,
    packs: Vec<Arc<PackRuntime>>,
    digests: Vec<Option<String>>,
    engine: Arc<FlowEngine>,
    state_machine: Arc<StateMachineRuntime>,
    http_client: Client,
    mocks: Option<Arc<MockLayer>>,
    timer_handles: Mutex<Vec<JoinHandle<()>>>,
    secrets: DynSecretsManager,
    operator_registry: OperatorRegistry,
    operator_metrics: Arc<OperatorMetrics>,
    contract_cache: ContractCache,
}

#[derive(Clone)]
pub struct ResolvedComponent {
    pub digest: String,
    pub component_ref: String,
    pub pack: Arc<PackRuntime>,
}

/// Block on a future whether or not we're already inside a tokio runtime.
pub fn block_on<F: Future<Output = R>, R>(future: F) -> R {
    if let Ok(handle) = Handle::try_current() {
        handle.block_on(future)
    } else {
        Runtime::new()
            .expect("failed to create tokio runtime")
            .block_on(future)
    }
}

impl TenantRuntime {
    #[allow(clippy::too_many_arguments)]
    pub async fn load(
        pack_path: &Path,
        config: Arc<HostConfig>,
        mocks: Option<Arc<MockLayer>>,
        archive_source: Option<&Path>,
        digest: Option<String>,
        wasi_policy: Arc<RunnerWasiPolicy>,
        session_host: Arc<dyn SessionHost>,
        session_store: DynSessionStore,
        state_store: DynStateStore,
        state_host: Arc<dyn StateHost>,
        secrets_manager: DynSecretsManager,
    ) -> Result<Arc<Self>> {
        let oauth_config = config.oauth_broker_config();
        let pack = Arc::new(
            PackRuntime::load(
                pack_path,
                Arc::clone(&config),
                mocks.clone(),
                archive_source,
                Some(Arc::clone(&session_store)),
                Some(Arc::clone(&state_store)),
                Arc::clone(&wasi_policy),
                Arc::clone(&secrets_manager),
                oauth_config.clone(),
                true,
                ComponentResolution::default(),
            )
            .await
            .with_context(|| {
                format!(
                    "failed to load pack {} for tenant {}",
                    pack_path.display(),
                    config.tenant
                )
            })?,
        );
        Self::from_packs(
            config,
            vec![(pack, digest)],
            mocks,
            session_host,
            session_store,
            state_store,
            state_host,
            secrets_manager,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn from_packs(
        config: Arc<HostConfig>,
        packs: Vec<(Arc<PackRuntime>, Option<String>)>,
        mocks: Option<Arc<MockLayer>>,
        session_host: Arc<dyn SessionHost>,
        session_store: DynSessionStore,
        _state_store: DynStateStore,
        state_host: Arc<dyn StateHost>,
        secrets_manager: DynSecretsManager,
    ) -> Result<Arc<Self>> {
        let operator_registry = OperatorRegistry::build(&packs)?;
        let operator_metrics = Arc::new(OperatorMetrics::default());
        let pack_runtimes = packs
            .iter()
            .map(|(pack, _)| Arc::clone(pack))
            .collect::<Vec<_>>();
        let digests = packs
            .iter()
            .map(|(_, digest)| digest.clone())
            .collect::<Vec<_>>();
        let mut pack_trace = HashMap::new();
        for (pack, digest) in &packs {
            let pack_id = pack.metadata().pack_id.clone();
            let pack_ref = config
                .pack_bindings
                .iter()
                .find(|binding| binding.pack_id == pack_id)
                .map(|binding| binding.pack_ref.clone())
                .unwrap_or_else(|| pack_id.clone());
            pack_trace.insert(
                pack_id,
                PackTraceInfo {
                    pack_ref,
                    resolved_digest: digest.clone(),
                },
            );
        }
        let engine = Arc::new(
            FlowEngine::new(pack_runtimes.clone(), Arc::clone(&config))
                .await
                .context("failed to prime flow engine")?,
        );
        let state_machine = Arc::new(
            StateMachineRuntime::from_flow_engine(
                Arc::clone(&config),
                Arc::clone(&engine),
                pack_trace,
                session_host,
                session_store,
                state_host,
                Arc::clone(&secrets_manager),
                mocks.clone(),
            )
            .context("failed to initialise state machine runtime")?,
        );
        let http_client = Client::builder().build()?;
        Ok(Arc::new(Self {
            tenant: config.tenant.clone(),
            config,
            packs: pack_runtimes,
            digests,
            engine,
            state_machine,
            http_client,
            mocks,
            timer_handles: Mutex::new(Vec::new()),
            secrets: secrets_manager,
            operator_registry,
            operator_metrics,
            contract_cache: ContractCache::from_env(),
        }))
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    pub fn config(&self) -> &Arc<HostConfig> {
        &self.config
    }

    pub fn operator_registry(&self) -> &OperatorRegistry {
        &self.operator_registry
    }

    pub fn operator_metrics(&self) -> &OperatorMetrics {
        &self.operator_metrics
    }

    pub fn contract_cache(&self) -> &ContractCache {
        &self.contract_cache
    }

    pub fn contract_cache_stats(&self) -> ContractCacheStats {
        self.contract_cache.stats()
    }

    pub fn main_pack(&self) -> &Arc<PackRuntime> {
        self.packs
            .first()
            .expect("tenant runtime must contain at least one pack")
    }

    pub fn pack(&self) -> Arc<PackRuntime> {
        Arc::clone(self.main_pack())
    }

    pub fn overlays(&self) -> Vec<Arc<PackRuntime>> {
        self.packs.iter().skip(1).cloned().collect()
    }

    pub fn engine(&self) -> &Arc<FlowEngine> {
        &self.engine
    }

    pub fn state_machine(&self) -> &Arc<StateMachineRuntime> {
        &self.state_machine
    }

    pub fn http_client(&self) -> &Client {
        &self.http_client
    }

    pub fn oauth_config(&self) -> Option<OAuthBrokerConfig> {
        self.config.oauth_broker_config()
    }

    pub fn digest(&self) -> Option<&str> {
        self.digests.first().and_then(|d| d.as_deref())
    }

    pub fn overlay_digests(&self) -> Vec<Option<String>> {
        self.digests.iter().skip(1).cloned().collect()
    }

    pub fn required_secrets(&self) -> Vec<SecretRequirement> {
        self.packs
            .iter()
            .flat_map(|pack| pack.required_secrets().iter().cloned())
            .collect()
    }

    pub fn missing_secrets(&self) -> Vec<SecretRequirement> {
        self.packs
            .iter()
            .flat_map(|pack| pack.missing_secrets(&self.config.tenant_ctx()))
            .collect()
    }

    pub fn mocks(&self) -> Option<&Arc<MockLayer>> {
        self.mocks.as_ref()
    }

    pub fn register_timers(&self, handles: Vec<JoinHandle<()>>) {
        self.timer_handles.lock().extend(handles);
    }

    pub fn get_secret(&self, key: &str) -> Result<String> {
        if crate::provider_core_only::is_enabled() {
            bail!(crate::provider_core_only::blocked_message("secrets"))
        }
        if !self.config.secrets_policy.is_allowed(key) {
            bail!("secret {key} is not permitted by bindings policy");
        }
        let ctx = self.config.tenant_ctx();
        let canonical_key = canonicalize_secret_key(key);
        let bytes =
            read_secret_blocking(&self.secrets, &ctx, RUNTIME_SECRETS_PACK_ID, &canonical_key)
                .context("failed to read secret from manager")?;
        let value = String::from_utf8(bytes).context("secret value is not valid UTF-8")?;
        Ok(value)
    }

    pub fn build_events_email_execution_plan(
        &self,
        tenant: &greentic_types::TenantCtx,
        request: &EmailSendRequest,
    ) -> Result<EmailExecutionPlan> {
        let oauth = self
            .oauth_config()
            .ok_or_else(|| anyhow!("oauth broker config is not configured for tenant runtime"))?;
        build_email_execution_plan(&oauth, tenant, request)
    }

    pub async fn execute_events_email_request(
        &self,
        access_token: &str,
        request: &EmailSendRequest,
    ) -> Result<()> {
        execute_email_request(self.http_client(), access_token, request).await
    }

    pub async fn execute_events_email_with_oauth(
        &self,
        tenant: &greentic_types::TenantCtx,
        request: &EmailSendRequest,
    ) -> Result<()> {
        let plan = self.build_events_email_execution_plan(tenant, request)?;
        let token = request_resource_token(self.http_client(), &plan.token_request).await?;
        self.execute_events_email_request(&token.access_token, request)
            .await
    }

    pub fn pack_for_component(&self, component_ref: &str) -> Option<Arc<PackRuntime>> {
        self.packs
            .iter()
            .find(|pack| pack.contains_component(component_ref))
            .cloned()
    }

    pub fn pack_for_component_with_digest(
        &self,
        component_ref: &str,
    ) -> Option<(Arc<PackRuntime>, Option<String>)> {
        self.packs
            .iter()
            .zip(self.digests.iter())
            .find(|(pack, _)| pack.contains_component(component_ref))
            .map(|(pack, digest)| (Arc::clone(pack), digest.clone()))
    }

    pub fn resolve_component(&self, component_ref: &str) -> Option<ResolvedComponent> {
        self.pack_for_component_with_digest(component_ref)
            .map(|(pack, digest)| ResolvedComponent {
                digest: digest
                    .or_else(|| self.digest().map(ToString::to_string))
                    .unwrap_or_else(|| "unknown".to_string()),
                component_ref: component_ref.to_string(),
                pack,
            })
    }
}

impl Drop for TenantRuntime {
    fn drop(&mut self) {
        for handle in self.timer_handles.lock().drain(..) {
            handle.abort();
        }
    }
}
