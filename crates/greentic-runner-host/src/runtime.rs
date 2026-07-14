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
        #[cfg(feature = "agentic-worker")] ext_llm_port: Option<crate::host::ExtLlmPort>,
        #[cfg(feature = "agentic-worker")] stream_observers: Option<
            crate::http::agent_stream::StreamObserverRegistry,
        >,
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
            #[cfg(feature = "agentic-worker")]
            ext_llm_port,
            #[cfg(feature = "agentic-worker")]
            stream_observers,
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
        #[cfg(feature = "agentic-worker")] ext_llm_port: Option<crate::host::ExtLlmPort>,
        #[cfg(feature = "agentic-worker")] stream_observers: Option<
            crate::http::agent_stream::StreamObserverRegistry,
        >,
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
        #[cfg_attr(not(feature = "agentic-worker"), allow(unused_mut))]
        let mut engine = FlowEngine::new(pack_runtimes.clone(), Arc::clone(&config))
            .await
            .context("failed to prime flow engine")?;

        // Wire Sorla remote-dispatch (NATS) into the flow engine BEFORE it is
        // moved behind an `Arc`. `set_remote_dispatch_handler` takes `&mut self`,
        // so the dispatcher must be attached while `engine` is still owned and
        // mutable. The response listener (which needs the post-build ingress
        // handle) is spawned further below, after the runtime is constructed.
        //
        // Gated on `GREENTIC_EVENTS_NATS_URL`: when unset, `sorla.call` stays
        // disabled and existing behaviour is unchanged. When set but NATS cannot
        // be reached we log a warning and continue (the engine simply has no
        // dispatch handler, so `sorla.call` nodes fail fast at execution time).
        //
        // Connected here (BEFORE the agentic-worker block below) rather than
        // its original post-block position so the agent-node handler
        // construction below can also thread the (possibly connected) client
        // into an `AuditSink` for `dw.agent` step audit events (EPIC-B B-3);
        // pure reordering, no behaviour change to the dispatch wiring itself.
        let dispatch_nats_client = match std::env::var("GREENTIC_EVENTS_NATS_URL") {
            Ok(nats_url) => match async_nats::connect(&nats_url).await {
                Ok(client) => {
                    engine.set_remote_dispatch_handler(Arc::new(
                        crate::runner::remote_dispatch::NatsDispatcher::new(client.clone()),
                    ));
                    Some(client)
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "GREENTIC_EVENTS_NATS_URL set but NATS connect failed; sorla.call disabled"
                    );
                    None
                }
            },
            Err(_) => None,
        };

        // Clone the (possibly connected) client for the audit sink (EPIC-B
        // B-2/B-3): threaded into `StateMachineRuntime::from_flow_engine` so
        // `TraceRecorder` can publish best-effort audit events over NATS, and
        // (as an `AuditSink`) into the `dw.agent` node handler below so agent
        // tool-call/tool-result steps are audited too. Cloned BEFORE the
        // response-listener loop further below moves `dispatch_nats_client`.
        // `None` when NATS is unset/unreachable, which keeps both audit paths
        // off by default (zero behaviour change).
        let audit_nats_client = dispatch_nats_client.clone();

        #[cfg(feature = "agentic-worker")]
        {
            use crate::runner::agent_node::{
                agent_configs_from_manifest, merge_agent_sources, merge_sidecar_into,
            };
            use std::collections::HashMap;

            // Collect agent configs from all New-manifest packs. When the same
            // agent_id appears in multiple packs the last pack wins (packs are
            // ordered: first = primary, rest = overlays). Collisions are logged
            // so operators can audit cross-pack conflicts.
            let mut pack_agents: HashMap<String, greentic_aw_runtime::AgentConfig> = HashMap::new();
            for pack in &pack_runtimes {
                let mut blobs = pack.manifest_agent_blobs();
                // Bridge: designer-built packs cannot populate `manifest.agents`
                // (old greentic-pack); they embed a `dw-agents.json` sidecar.
                // Fill any agent_id the manifest lacked (manifest stays authoritative).
                merge_sidecar_into(&mut blobs, pack.dw_agents_sidecar_blobs());
                if blobs.is_empty() {
                    continue;
                }
                let pack_id = pack.metadata().pack_id.clone();
                let configs = agent_configs_from_manifest(&pack_id, &blobs);
                for (agent_id, agent_config) in configs {
                    if let Some(existing) = pack_agents.get(&agent_id) {
                        tracing::warn!(
                            agent_id,
                            prior_pack = existing.agent_id.as_str(),
                            new_pack = pack_id.as_str(),
                            "agent_id collision across packs; last pack wins"
                        );
                    }
                    pack_agents.insert(agent_id, agent_config);
                }
            }

            // Operator config overrides pack-provided agents on collision.
            let merged_agents = merge_agent_sources(pack_agents, config.agents.clone());

            // First-boot ingest of any pack-baked knowledge corpus (W4 4c). Runs
            // BEFORE the agent runtime mounts its serving knowledge connection:
            // embedded SurrealDB allows one handle per store directory, so the
            // temporary ingest connection must open and drop before the serving
            // mount (inside build_agent_node_handler) opens its own. No-op without
            // the `knowledge-chronicle` feature or when no pack carries a corpus.
            #[cfg(feature = "knowledge-chronicle")]
            {
                let corpus = crate::runner::knowledge_corpus::collect(&pack_runtimes);
                crate::runner::knowledge_mount::ingest_corpus(&config.tenant_ctx(), corpus).await;
            }

            // DwAgent state-store selection. With GREENTIC_AW_REDIS_URL set, use the
            // Redis-backed stores (production multi-process default). Without it, when
            // built with `desktop-agent-ephemeral`, fall back to the process-global
            // in-memory stores so a single-process runner (e.g. the designer's
            // loopback test-chat sidecar) runs agentic-worker turns with NO external
            // infra. Otherwise DwAgent nodes stay disabled — unchanged server
            // behaviour (build_agent_node_handler returns None when Redis is unset).
            let redis_set = std::env::var("GREENTIC_AW_REDIS_URL")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            // Best-effort agent-step audit sink (EPIC-B B-3), built from the
            // same (possibly connected) NATS client the flow-level audit sink
            // (B-2) uses. `None` when NATS is unset/unreachable, which keeps
            // `dw.agent` execution on the plain `AgentRuntime::step` path
            // (zero behaviour change).
            let agent_audit_sink = audit_nats_client
                .clone()
                .map(crate::trace::audit_sink::AuditSink::new);
            // `merged_agents` is MOVED into `build_agent_node_handler` below
            // (redis/ephemeral branches); clone for the graph handler first so
            // it can resolve a graph node's `agent_ref` (SP1) against the same
            // process-level merged config map.
            let graph_agents = merged_agents.clone();
            // Also needed after `merged_agents` is moved into
            // `build_agent_node_handler`/`_ephemeral` below, to resolve the
            // in-process operala.call LLM key the same way (see the
            // `desktop-agent-ephemeral` block after the DwAgent wiring).
            #[cfg(feature = "desktop-agent-ephemeral")]
            let operala_agents = merged_agents.clone();
            let agent_handler = if redis_set {
                crate::runner::agent_node::build_agent_node_handler(
                    merged_agents,
                    config.tenant.clone(),
                    Arc::clone(&secrets_manager),
                    ext_llm_port.clone(),
                    pack_runtimes.clone(),
                    agent_audit_sink.clone(),
                    stream_observers.clone(),
                )
                .await
            } else {
                #[cfg(feature = "desktop-agent-ephemeral")]
                {
                    crate::runner::agent_node::build_agent_node_handler_ephemeral(
                        merged_agents,
                        config.tenant.clone(),
                        Arc::clone(&secrets_manager),
                        ext_llm_port.clone(),
                        pack_runtimes.clone(),
                        agent_audit_sink.clone(),
                        stream_observers.clone(),
                    )
                    .await
                }
                #[cfg(not(feature = "desktop-agent-ephemeral"))]
                {
                    crate::runner::agent_node::build_agent_node_handler(
                        merged_agents,
                        config.tenant.clone(),
                        Arc::clone(&secrets_manager),
                        ext_llm_port.clone(),
                        pack_runtimes.clone(),
                        agent_audit_sink.clone(),
                        stream_observers.clone(),
                    )
                    .await
                }
            };
            if let Some(handler) = agent_handler {
                engine.set_agent_node_handler(handler);
                tracing::info!("DwAgent runtime wired into FlowEngine");
            }

            // In-process deep-worker runtime for `operala.call` nodes
            // (desktop-agent-ephemeral only, e.g. the designer's offline
            // Test-chat sidecar — same feature the ephemeral DwAgent handler
            // above uses). Reuses the exact key-resolution policy the
            // in-process dw.agent LLM backend uses (env key wins; otherwise
            // the first agent's `llm.credential_ref` resolved from the
            // per-tenant secrets store), then builds a
            // `greentic_llm::RigBackend` — the same OpenAI-compatible,
            // multi-provider client `GreenticLlmBackend` uses for dw.agent —
            // and wraps it in `DeepWorkerInvoker`. No handler is wired (and
            // `operala.call` falls back to the NATS `RemoteDispatchHandler`,
            // failing without it) when no LLM key resolves or the provider
            // fails to build.
            #[cfg(feature = "desktop-agent-ephemeral")]
            {
                let env_key = std::env::var("GREENTIC_LLM_API_KEY")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| {
                        std::env::var("OPENAI_API_KEY")
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                    });
                let api_key = match env_key {
                    Some(key) => Some(key),
                    None => {
                        crate::runner::agent_node::resolve_in_process_llm_key(
                            &secrets_manager,
                            &config.tenant,
                            &operala_agents,
                        )
                        .await
                    }
                };
                match api_key {
                    Some(api_key) => {
                        // Provider/model are resolved PER WORKER from each
                        // `operala.call` node's `input.llm` binding (stamped by
                        // greentic-dw-authoring for authored deep-worker packs);
                        // the handler builds the LLM per dispatch. For a node
                        // that carries NO `input.llm` — e.g. an operala.call
                        // synthesized at runtime for a dw.agent's chronicle
                        // knowledge/memory retrieval — fall back to the agent's
                        // OWN configured provider/model (the same `operala_agents`
                        // the api_key is resolved from), which is NOT a guess: it
                        // is this worker's declared LLM, consistent with its key.
                        // The process-level env still OVERRIDES both. Only when
                        // neither the node, the agent config, nor the env carries
                        // a provider/model does the dispatch error explicitly.
                        let agent_llm =
                            operala_agents.values().map(|agent| &agent.llm).find(|llm| {
                                !llm.provider.trim().is_empty() && !llm.model.trim().is_empty()
                            });
                        let fallback_provider = std::env::var("GREENTIC_LLM_PROVIDER")
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                            .or_else(|| agent_llm.map(|llm| llm.provider.clone()));
                        let fallback_model = std::env::var("GREENTIC_LLM_MODEL")
                            .ok()
                            .filter(|value| !value.trim().is_empty())
                            .or_else(|| agent_llm.map(|llm| llm.model.clone()));
                        let base_url = std::env::var("GREENTIC_LLM_BASE_URL")
                            .ok()
                            .filter(|value| !value.trim().is_empty());
                        engine.set_operala_node_handler(Arc::new(
                            crate::runner::operala_node::RuntimeOperalaNodeHandler::new(
                                api_key,
                                base_url,
                                fallback_provider,
                                fallback_model,
                            ),
                        ));
                        tracing::info!(
                            "operala.call in-process deep-worker runtime wired into FlowEngine \
                             (provider/model resolved per-worker from node input.llm)"
                        );
                    }
                    None => {
                        tracing::warn!(
                            "no LLM API key resolved (env or store) for operala.call \
                             in-process wiring; operala.call nodes will fall back to NATS \
                             (and fail without it)"
                        );
                    }
                }
            }

            // Collect agent-graph sidecars from each pack. Unlike agents (a
            // manifest.cbor map), graphs arrive as a pack FILE (`agent-graph.json`)
            // — one sidecar per pack — so the graph is keyed by `pack_id`. A
            // sidecar that fails UTF-8 / JSON / schema validation is logged and
            // skipped (lenient, mirroring `agent_configs_from_manifest`) so a bad
            // graph never blocks the rest of pack loading. When the same pack_id
            // appears twice (overlay), the last pack wins.
            let mut graphs: HashMap<String, greentic_aw_runtime::graph::GraphConfig> =
                HashMap::new();
            for pack in &pack_runtimes {
                let Some(bytes) = pack.read_agent_graph_sidecar() else {
                    continue;
                };
                let pack_id = pack.metadata().pack_id.clone();
                if let Some(config) =
                    crate::runner::graph_node::graph_config_from_sidecar(&pack_id, &bytes)
                    && graphs.insert(pack_id.clone(), config).is_some()
                {
                    tracing::warn!(
                        pack_id = pack_id.as_str(),
                        "agent-graph sidecar for pack_id seen twice; last pack wins"
                    );
                }
            }

            // Producer/operator-declared graphs (HostConfig.graphs) override
            // pack-sidecar graphs on `graph_id` collision — mirroring the
            // operator-wins merge for agents.
            for (graph_id, graph_config) in config.graphs.clone() {
                graphs.insert(graph_id, graph_config);
            }

            if let Some(handler) = crate::runner::graph_node::build_graph_node_handler(
                graphs,
                agent_audit_sink.clone(),
                Arc::new(pack_runtimes.clone()),
                graph_agents,
            )
            .await
            {
                engine.set_graph_node_handler(handler);
                tracing::info!("DwAgentGraph runtime wired into FlowEngine");
            }
        }

        // Resolve how `dw.agent` nodes dispatch. When `GREENTIC_AW_DISPATCH=nats`
        // is set, the node is rerouted over the durable agentic NATS path instead
        // of the in-process handler. Must be wired while `engine` is still owned.
        #[cfg(feature = "agentic-worker")]
        {
            let dw_dispatch =
                crate::runner::agent_node::dw_agent_dispatch_mode(|k| std::env::var(k).ok());
            engine.set_dw_agent_dispatch(dw_dispatch);
            if matches!(
                dw_dispatch,
                crate::runner::agent_node::DwAgentDispatch::Nats
            ) && std::env::var("GREENTIC_EVENTS_NATS_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .is_none()
            {
                tracing::warn!(
                    "GREENTIC_AW_DISPATCH=nats but GREENTIC_EVENTS_NATS_URL is unset; \
                     dw.agent nodes will fail (no remote dispatch handler). \
                     Set the NATS URL or unset the flag."
                );
            }
        }

        let engine = Arc::new(engine);
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
                audit_nats_client,
            )
            .context("failed to initialise state machine runtime")?,
        );

        // Spawn the response listeners now that the ingress handle
        // (`state_machine`) exists. Each listener resumes paused flows by feeding
        // a synthesized ingress envelope through `StateMachineRuntime::handle`
        // (see `RuntimeSessionResumer`). The resumer is runtime-agnostic (it
        // resumes by correlation id), so one shared resumer serves all runtimes;
        // we run one listener per runtime so every `*.call` node's responses
        // (`greentic.<runtime>.response.v1`) are consumed. Only started when the
        // dispatcher above connected successfully.
        if let Some(client) = dispatch_nats_client {
            let resumer = Arc::new(
                crate::runner::runtime_session_resumer::RuntimeSessionResumer::new(Arc::clone(
                    &state_machine,
                )),
            );
            for runtime_name in ["sorla", "operala", "agentic", "telco-x", "approval"] {
                tokio::spawn(crate::runner::dispatch_listener::run_response_listener(
                    client.clone(),
                    runtime_name.to_string(),
                    Arc::clone(&resumer)
                        as Arc<dyn crate::runner::dispatch_listener::SessionResumer>,
                ));
            }
            // Eagerly pre-cache local-wasm MCP components when the admin publishes
            // a warm event. Feature-gated behind `agentic-worker` because
            // `mcp_store_pull` lives in `greentic-aw-runtime` which is only
            // present when that feature is enabled.
            #[cfg(feature = "agentic-worker")]
            tokio::spawn(crate::runner::mcp_warm_listener::run_mcp_warm_listener(
                client.clone(),
            ));
            tracing::info!(
                "Remote-dispatch (NATS) wired into runtime: sorla.call / operala.call / agentic.call / telco-x.call"
            );
        }
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
