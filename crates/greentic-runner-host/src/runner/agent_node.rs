use anyhow::Result;
use serde_json::Value;

/// Bridges a `DwAgent` flow node into the agentic-worker runtime.
///
/// The concrete impl (constructed in the runner binary, Task 4.3) wraps
/// `greentic_aw_runtime::AgentRuntime`. The engine holds it as a trait
/// object so `engine.rs` stays free of AW-runtime construction details.
#[async_trait::async_trait]
pub trait AgentNodeHandler: Send + Sync {
    /// Execute one agentic step. `flow_input` is the upstream node's
    /// JSON payload (expects at least `{"user_text": "..."}`); returns
    /// the node output JSON (`{"reply", "trail", "terminated_by"}`).
    async fn execute(
        &self,
        tenant_id: &str,
        env_id: &str,
        agent_id: &str,
        session_id: &str,
        flow_input: &Value,
    ) -> Result<Value>;
}

// ---------------------------------------------------------------------------
// agentic-worker feature: full DwAgent / AgentRuntime integration
// ---------------------------------------------------------------------------

#[cfg(feature = "agentic-worker")]
mod aw {
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;

    use anyhow::Result;
    use greentic_aw_runtime::config::AgentConfig;
    use greentic_aw_runtime::config_provider::ConfigProvider;
    use greentic_aw_runtime::error::ConfigError;
    use greentic_aw_runtime::{AgentInput, AgentRuntime, AgentStep, TenantContext};
    use serde_json::{Value, json};

    use super::AgentNodeHandler;

    // -----------------------------------------------------------------------
    // Pack-manifest agent helpers
    // -----------------------------------------------------------------------

    /// Deserialize raw agent blobs from a pack manifest into typed
    /// [`AgentConfig`] structs.
    ///
    /// Malformed blobs are skipped with a [`tracing::warn!`] so a single
    /// bad entry never prevents other agents (or pack-level operators) from
    /// loading. The `pack_id` argument is used only in log messages.
    pub fn agent_configs_from_manifest(
        pack_id: &str,
        blobs: &std::collections::BTreeMap<String, Value>,
    ) -> HashMap<String, AgentConfig> {
        blobs
            .iter()
            .filter_map(|(agent_id, blob)| {
                match serde_json::from_value::<AgentConfig>(blob.clone()) {
                    Ok(config) => Some((agent_id.clone(), config)),
                    Err(deserialize_error) => {
                        tracing::warn!(
                            pack_id,
                            agent_id,
                            error = %deserialize_error,
                            "skipping malformed agent blob in pack manifest"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Merge pack-provided agent configs with operator-declared ones.
    ///
    /// Pack agents form the base layer; operator entries take precedence on
    /// `agent_id` collision (operator always wins). This ensures operators can
    /// override or refine any pack-embedded agent without touching the pack
    /// itself.
    pub fn merge_agent_sources(
        pack_agents: HashMap<String, AgentConfig>,
        operator_agents: HashMap<String, AgentConfig>,
    ) -> HashMap<String, AgentConfig> {
        let mut merged = pack_agents;
        for (agent_id, operator_config) in operator_agents {
            merged.insert(agent_id, operator_config);
        }
        merged
    }

    /// Fixed, user-safe reply returned when an agentic step fails. The detailed
    /// [`greentic_aw_runtime::AgentError`] is logged but never surfaced to the
    /// flow output, so internal failure modes do not leak to end users.
    const SANITISED_ERROR_REPLY: &str = "Something went wrong. Please try again.";

    /// Production [`AgentNodeHandler`] wrapping the agentic-worker runtime.
    ///
    /// Holds a shared [`AgentRuntime`] and translates a `DwAgent` flow node's
    /// JSON payload into an [`AgentInput`], invoking one Plan-Act-Observe step
    /// per call. Construction (Task 4.3b) lives in the runner binary; the engine
    /// only ever sees this through the [`AgentNodeHandler`] trait object.
    pub struct RuntimeAgentNodeHandler {
        runtime: Arc<AgentRuntime>,
    }

    impl RuntimeAgentNodeHandler {
        /// Wrap a shared [`AgentRuntime`] in a flow-node handler.
        pub fn new(runtime: Arc<AgentRuntime>) -> Self {
            Self { runtime }
        }
    }

    #[async_trait::async_trait]
    impl AgentNodeHandler for RuntimeAgentNodeHandler {
        async fn execute(
            &self,
            tenant_id: &str,
            env_id: &str,
            agent_id: &str,
            session_id: &str,
            flow_input: &Value,
        ) -> Result<Value> {
            let user_text = flow_input
                .get("user_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tenant = TenantContext::new(tenant_id, env_id);
            let input = AgentInput { text: user_text };

            match self.runtime.step(tenant, session_id, agent_id, input).await {
                Ok(output) => Ok(json!({
                    "reply": output.reply,
                    "trail": output.trail,
                    "terminated_by": output.terminated_by,
                })),
                Err(error) => {
                    // Never leak the internal AgentError to the flow output. Log
                    // the detail for operators; return a sanitised reply only.
                    tracing::warn!(error = %error, agent_id, session_id, "DwAgent step failed");
                    Ok(json!({
                        "reply": SANITISED_ERROR_REPLY,
                        "trail": Vec::<AgentStep>::new(),
                        "terminated_by": "error",
                    }))
                }
            }
        }
    }

    /// [`ConfigProvider`] backed by the operator's [`HostConfig::agents`] map.
    ///
    /// Agents are operator-global for the MVP: lookup is keyed purely by
    /// `agent_id`; the `tenant`/`env` arguments are accepted (to satisfy the
    /// trait contract) but not used for keying. This avoids a tenant/env
    /// key-matching footgun against the dispatch path, which derives those
    /// values independently. A future per-tenant config source can replace
    /// this implementation without touching callers.
    pub struct HostConfigProvider {
        agents: HashMap<String, AgentConfig>,
    }

    impl HostConfigProvider {
        /// Wrap the operator-declared agents map in a [`ConfigProvider`].
        pub fn new(agents: HashMap<String, AgentConfig>) -> Self {
            Self { agents }
        }
    }

    impl ConfigProvider for HostConfigProvider {
        fn agent_config<'a>(
            &'a self,
            _tenant: &'a TenantContext,
            agent_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<AgentConfig, ConfigError>> + Send + 'a>> {
            let found = self.agents.get(agent_id).cloned();
            let agent_id_owned = agent_id.to_string();
            Box::pin(async move { found.ok_or(ConfigError::AgentNotFound(agent_id_owned)) })
        }
    }

    /// Resolve the extension discovery directory for tool dispatch.
    ///
    /// Honours `GREENTIC_EXTENSIONS_DIR`; otherwise falls back to
    /// `~/.greentic/extensions` (the platform convention), and finally to a
    /// temp-dir path when no home directory can be resolved. A missing or
    /// empty directory is harmless — `list_tools`/`invoke_tool` simply return
    /// empty/NotFound, which is correct for tool-less agents.
    fn extension_discovery_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("GREENTIC_EXTENSIONS_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".greentic").join("extensions");
        }
        std::env::temp_dir().join("greentic").join("extensions")
    }

    /// Resolve the directory scanned for `<agent_id>.json` Digital Worker manifests.
    ///
    /// Honours `GREENTIC_AGENT_MANIFESTS_DIR`; otherwise `~/.greentic/agents`, and
    /// finally a temp-dir path when no home is resolvable (keeps the fn total). A
    /// missing dir is harmless — the overlay provider simply finds no manifest and
    /// returns the YAML base unchanged.
    fn manifests_discovery_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("GREENTIC_AGENT_MANIFESTS_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".greentic").join("agents");
        }
        std::env::temp_dir().join("greentic").join("agents")
    }

    /// Build an [`HttpConfigProvider`] from `GREENTIC_AW_ADMIN_ENDPOINT` +
    /// `GREENTIC_AW_ADMIN_TOKEN`. Returns `None` when either is unset/empty, so
    /// the runtime keeps using the local overlay alone.
    fn registry_from_env() -> Option<greentic_aw_runtime::HttpConfigProvider> {
        let endpoint = std::env::var("GREENTIC_AW_ADMIN_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("GREENTIC_AW_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(greentic_aw_runtime::HttpConfigProvider::new(
            endpoint, token,
        ))
    }

    /// Build an [`McpToolSource`] from the same admin endpoint/token the agent
    /// registry uses (`GREENTIC_AW_ADMIN_ENDPOINT` + `GREENTIC_AW_ADMIN_TOKEN`).
    ///
    /// MCP tools are ON by default whenever the admin credentials are present:
    /// exposure is already authorized twice upstream (the tenant registers the
    /// server with the `agentic_worker` role in admin, and the agent's
    /// allowlist must explicitly reference `mcp:<server_id>`), so a configured
    /// runner participates without extra ceremony. `GREENTIC_AW_MCP=0` is the
    /// operator opt-out escape hatch for environments where outbound calls to
    /// tenant-registered MCP servers must stay disabled. Returns `None` on
    /// opt-out or when either credential is missing/empty.
    ///
    /// [`McpToolSource`]: greentic_aw_runtime::McpToolSource
    fn mcp_source_from_env() -> Option<Arc<greentic_aw_runtime::McpToolSource>> {
        if std::env::var("GREENTIC_AW_MCP").ok().as_deref() == Some("0") {
            tracing::info!("GREENTIC_AW_MCP=0; MCP tool source disabled");
            return None;
        }
        let endpoint = std::env::var("GREENTIC_AW_ADMIN_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("GREENTIC_AW_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        tracing::info!(endpoint = %endpoint, "MCP tool source constructed");
        Some(Arc::new(greentic_aw_runtime::McpToolSource::new(
            endpoint, token,
        )))
    }

    /// Build the production [`greentic_ext_runtime::ExtensionRuntime`] used for
    /// tool dispatch, wrapped in an [`Arc`] for sharing with [`AgentRuntime`].
    ///
    /// On construction failure (e.g. wasmtime engine init), logs the error and
    /// returns `None`; the caller then disables `DwAgent` nodes rather than
    /// panicking.
    ///
    /// Unlike the designer (which installs extensions through an explicit flow),
    /// the runner has no install step — so it performs an initial scan of the
    /// `design/` kind directory under the discovery root and registers each
    /// on-disk extension here. Without this the agentic worker would boot with
    /// an empty tool runtime and every extension tool would be silently dropped.
    /// Per-extension failures (bad signature, malformed describe) are logged and
    /// skipped so one broken extension never aborts boot; the watcher still
    /// hot-reloads later changes.
    pub(crate) fn build_ext_runtime() -> Option<Arc<greentic_ext_runtime::ExtensionRuntime>> {
        use greentic_ext_runtime::{DiscoveryPaths, ExtensionRuntime, RuntimeConfig, discovery};

        let root = extension_discovery_dir();
        let paths = DiscoveryPaths::new(root.clone());
        let mut runtime = match ExtensionRuntime::new(RuntimeConfig::from_paths(paths)) {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(error = %error, "extension runtime init failed; DwAgent nodes disabled");
                return None;
            }
        };

        // Initial load of on-disk design extensions (agentic-worker tools live
        // in `<root>/design/<ext>/`).
        let design_dir = root.join("design");
        match discovery::scan_kind_dir(&design_dir) {
            Ok(ext_dirs) => {
                let mut loaded = 0usize;
                for ext_dir in ext_dirs {
                    match runtime.register_loaded_from_dir(&ext_dir) {
                        Ok(()) => loaded += 1,
                        Err(error) => tracing::warn!(
                            error = %error, dir = %ext_dir.display(),
                            "skipping extension that failed to load"
                        ),
                    }
                }
                tracing::info!(loaded, dir = %design_dir.display(), "loaded design extensions");
            }
            Err(error) => {
                tracing::warn!(error = %error, dir = %design_dir.display(), "scanning design extensions failed")
            }
        }

        Some(Arc::new(runtime))
    }

    /// Resolve the [`LlmBackend`] from the environment.
    ///
    /// Prefers the LLM bridge extension when `GREENTIC_AW_LLM_EXTENSION` is set
    /// (LLM-as-extension); otherwise falls back to the env-keyed in-process
    /// OpenAI client. Shared by the single-agent (`build_agent_node_handler`)
    /// and graph (`graph_node::build_graph_node_handler`) construction paths so
    /// both resolve the backend identically.
    pub(crate) fn build_llm_backend(
        ext_runtime: &Arc<greentic_ext_runtime::ExtensionRuntime>,
    ) -> Arc<dyn greentic_aw_runtime::LlmBackend> {
        use std::time::Duration;

        use greentic_aw_runtime::{ExtensionLlmBackend, OpenAiLlmBackend, RetryingLlmBackend};

        match std::env::var("GREENTIC_AW_LLM_EXTENSION")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            Some(ext_id) => {
                let api_key = std::env::var("GREENTIC_LLM_API_KEY")
                    .or_else(|_| std::env::var("OPENAI_API_KEY"))
                    .unwrap_or_default();
                match bridge_credential(
                    std::env::var("GREENTIC_LLM_PROVIDER").ok(),
                    std::env::var("GREENTIC_LLM_MODEL").ok(),
                    api_key,
                    std::env::var("GREENTIC_LLM_BASE_URL").ok(),
                ) {
                    Some(cred) => {
                        tracing::info!(
                            extension = %ext_id, provider = %cred.provider, model = %cred.model,
                            "AW LLM via bridge extension"
                        );
                        Arc::new(RetryingLlmBackend::new(
                            ExtensionLlmBackend::new(ext_runtime.clone(), ext_id, cred),
                            3,
                            Duration::from_millis(250),
                        ))
                    }
                    None => {
                        tracing::warn!(
                            "GREENTIC_AW_LLM_EXTENSION set but no LLM API key; \
                             falling back to in-process OpenAI client"
                        );
                        Arc::new(RetryingLlmBackend::new(
                            OpenAiLlmBackend::new(String::new()),
                            3,
                            Duration::from_millis(250),
                        ))
                    }
                }
            }
            None => {
                let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
                Arc::new(RetryingLlmBackend::new(
                    OpenAiLlmBackend::new(openai_key),
                    3,
                    Duration::from_millis(250),
                ))
            }
        }
    }

    /// Build a vault-style `BridgeCredential` from resolved parts. `None` when no
    /// API key is present. Defaults: provider "openai", model "gpt-4o". Pure (no
    /// env) so it is unit-testable without global state.
    pub(super) fn bridge_credential(
        provider: Option<String>,
        model: Option<String>,
        api_key: String,
        base_url: Option<String>,
    ) -> Option<greentic_aw_runtime::BridgeCredential> {
        if api_key.trim().is_empty() {
            return None;
        }
        Some(greentic_aw_runtime::BridgeCredential {
            provider: provider
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "openai".into()),
            model: model
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "gpt-4o".into()),
            api_key,
            base_url: base_url.filter(|s| !s.trim().is_empty()),
        })
    }

    /// Build the production `DwAgent` handler if the environment is configured.
    ///
    /// Returns `None` (so `DwAgent` flow dispatch errors clearly) under any of
    /// these graceful-degradation conditions:
    /// - `merged_agents` is empty (no agents from packs or operator config);
    /// - `GREENTIC_AW_REDIS_URL` is unset/empty;
    /// - the AW Redis connection fails;
    /// - the extension runtime fails to initialise.
    ///
    /// `merged_agents` is the result of merging pack-embedded agents (base)
    /// with operator-declared [`HostConfig::agents`] (operator wins on
    /// collision). This merged map replaces the former direct read of
    /// `config.agents` so pack-provided agents are included in the runtime.
    ///
    /// Redis is sourced from the environment because the runner uses an
    /// in-memory flow-state store by default and carries no Redis URL in
    /// [`HostConfig`]; this mirrors the existing env-config convention.
    pub async fn build_agent_node_handler(
        merged_agents: HashMap<String, AgentConfig>,
    ) -> Option<Arc<dyn AgentNodeHandler>> {
        let runtime = build_agent_runtime(merged_agents).await?;
        Some(Arc::new(RuntimeAgentNodeHandler::new(runtime)))
    }

    /// Construct the shared [`AgentRuntime`] from the environment.
    ///
    /// Factored out of [`build_agent_node_handler`] so both the in-process
    /// `dw.agent`/`agentic.call` flow node and the out-of-process NATS serve
    /// mode ([`serve_agentic`]) build an identical runtime (Redis state, env-
    /// resolved LLM backend, design extensions, agent config providers, MCP).
    ///
    /// Returns `None` under the same graceful-degradation conditions as the node
    /// handler: empty agent map, missing/unreachable `GREENTIC_AW_REDIS_URL`, or
    /// extension-runtime init failure.
    pub async fn build_agent_runtime(
        merged_agents: HashMap<String, AgentConfig>,
    ) -> Option<Arc<AgentRuntime>> {
        use greentic_aw_runtime::LayeredConfigProvider;
        use greentic_aw_runtime::ManifestToolOverlayProvider;
        use greentic_aw_runtime::config_provider::CachingConfigProvider;
        use greentic_aw_runtime::cost::RedisTokenMeter;
        use greentic_aw_runtime::tools::RedisToolLedger;
        use greentic_aw_runtime::{OtelTelemetry, RedisAgentStateStore};

        if merged_agents.is_empty() {
            return None; // nothing to serve
        }

        let redis_url = match std::env::var("GREENTIC_AW_REDIS_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                tracing::info!("GREENTIC_AW_REDIS_URL unset; DwAgent nodes disabled");
                return None;
            }
        };

        let state_store = match RedisAgentStateStore::connect(&redis_url).await {
            Ok(store) => Arc::new(store),
            Err(error) => {
                tracing::warn!(error = %error, "AW Redis connect failed; DwAgent nodes disabled");
                return None;
            }
        };

        // The connection manager is cheap to clone (multiplexed, ref-counted);
        // share it with the token meter and idempotency ledger.
        let manager = state_store.manager();
        let token_meter = Arc::new(RedisTokenMeter::new(manager.clone()));
        let ledger = Arc::new(RedisToolLedger::new(manager));

        let ext_runtime = build_ext_runtime()?;

        // Prefer the LLM bridge extension when configured (LLM-as-extension);
        // fall back to the env-keyed in-process OpenAI client otherwise.
        let llm = build_llm_backend(&ext_runtime);

        let agent_count = merged_agents.len();
        // Base config source = the merged agents (pack-embedded ⊕ operator,
        // operator wins). Wrap in the manifest-tool overlay, then layer the
        // admin agent registry on top when configured (registry first, overlay
        // fallback); cache the result either way.
        let overlay = ManifestToolOverlayProvider::new(
            HostConfigProvider::new(merged_agents),
            manifests_discovery_dir(),
        );
        let config_provider: Arc<dyn ConfigProvider> = match registry_from_env() {
            Some(http) => Arc::new(CachingConfigProvider::new(LayeredConfigProvider::new(
                http, overlay,
            ))),
            None => Arc::new(CachingConfigProvider::new(overlay)),
        };
        let telemetry = Arc::new(OtelTelemetry);

        let runtime = Arc::new(AgentRuntime::new(
            config_provider,
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
            mcp_source_from_env(),
        ));

        tracing::info!(agent_count, "AW runtime constructed");
        Some(runtime)
    }

    /// Run the agentic-worker runtime as a NATS-consuming service.
    ///
    /// Builds the production [`AgentRuntime`] via [`build_agent_runtime`] and,
    /// when it could be constructed, serves `greentic.agentic.request.v1`
    /// forever via the shared `aw-event-bridge`. This is the out-of-process
    /// (`agentic.call`) counterpart to the in-process `dw.agent` node.
    ///
    /// Returns `Ok(())` immediately (a no-op) when the runtime cannot be built
    /// (e.g. no agents, no Redis) so the host can call this unconditionally.
    pub async fn serve_agentic(
        nats_url: &str,
        merged_agents: HashMap<String, AgentConfig>,
    ) -> anyhow::Result<()> {
        match build_agent_runtime(merged_agents).await {
            Some(runtime) => {
                tracing::info!(nats_url, "agentic serve mode starting");
                greentic_aw_runtime::serve::serve(nats_url, runtime).await
            }
            None => {
                tracing::info!(
                    "agentic serve mode skipped: no agentic runtime could be constructed"
                );
                Ok(())
            }
        }
    }

    /// Load process-level base agent configs from the manifests directory.
    ///
    /// Reads every `<agent_id>.json` file in [`manifests_discovery_dir`] as a
    /// full [`AgentConfig`] (NOT the tool-only Digital Worker manifest consumed
    /// by [`ManifestToolOverlayProvider`]). This is the ONLY process-level base
    /// agent source: pack-embedded agents and `HostConfig::agents` are both
    /// per-tenant and only materialise inside `TenantRuntime::from_packs`, so an
    /// in-process serve started at process startup cannot see them.
    ///
    /// Returns an empty map when the directory is absent or unreadable. Files
    /// that fail to decode into an [`AgentConfig`], or whose `agent_id` does not
    /// match the file stem, are logged and skipped so one malformed file never
    /// aborts loading. The file stem is the authoritative key (the in-map id is
    /// taken from the stem), mirroring the `<agent_id>.json` convention.
    pub fn load_process_agent_configs() -> HashMap<String, AgentConfig> {
        let dir = manifests_discovery_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::debug!(
                    dir = %dir.display(),
                    error = %error,
                    "agent manifests dir not readable; no process-level agents loaded"
                );
                return HashMap::new();
            }
        };

        let mut agents: HashMap<String, AgentConfig> = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_json = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            if !is_json {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "agent config read failed; skipping");
                    continue;
                }
            };
            match serde_json::from_slice::<AgentConfig>(&bytes) {
                Ok(config) => {
                    if config.agent_id != stem {
                        tracing::warn!(
                            file_stem = stem,
                            agent_id = config.agent_id.as_str(),
                            "agent config id does not match filename; keying by filename"
                        );
                    }
                    agents.insert(stem.to_string(), config);
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "agent config decode failed; skipping");
                }
            }
        }
        agents
    }

    #[cfg(test)]
    mod tests {
        use std::collections::HashMap;
        use std::sync::Arc;

        use greentic_aw_runtime::cost::MockTokenMeter;
        use greentic_aw_runtime::llm::LlmResponse;
        use greentic_aw_runtime::mock::{
            MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
        };
        use greentic_aw_runtime::{AgentConfig, AgentLimits, LlmProviderRef};
        use greentic_aw_runtime::{AgentRuntime, TenantContext};
        use serde_json::json;

        use super::*;

        fn sample_agent_config(agent_id: &str) -> AgentConfig {
            AgentConfig {
                agent_id: agent_id.into(),
                system_prompt: "sys".into(),
                tools: vec![],
                llm: LlmProviderRef {
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                },
                limits: AgentLimits::default(),
                memory: None,
            }
        }

        #[tokio::test]
        async fn execute_returns_reply_json() {
            let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
                content: Some("pong".into()),
                tool_calls: vec![],
                tokens_in: 1,
                tokens_out: 1,
            })]));
            let store = Arc::new(MockAgentStateStore::new());
            let telemetry = Arc::new(MockTelemetry::new());

            let config_provider = MockConfigProvider::new();
            let tenant = TenantContext::new("t", "e");
            config_provider.insert(
                &tenant,
                "greeter",
                AgentConfig {
                    agent_id: "greeter".into(),
                    system_prompt: "sys".into(),
                    tools: vec![],
                    llm: LlmProviderRef {
                        provider: "mock".into(),
                        model: "m".into(),
                    },
                    limits: AgentLimits::default(),
                    memory: None,
                },
            );
            let config_provider = Arc::new(config_provider);

            let token_meter = Arc::new(MockTokenMeter::new(0));
            let ledger = Arc::new(NoopToolLedger);
            let ext_runtime = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());

            let runtime = Arc::new(AgentRuntime::new(
                config_provider,
                store,
                ext_runtime,
                llm,
                telemetry,
                token_meter,
                ledger,
                None,
            ));
            let handler = RuntimeAgentNodeHandler::new(runtime);

            let output = handler
                .execute("t", "e", "greeter", "sess-1", &json!({"user_text": "ping"}))
                .await
                .expect("execute should succeed");

            assert_eq!(output["reply"].as_str(), Some("pong"));
        }

        #[tokio::test]
        async fn host_config_provider_returns_config_for_known_agent() {
            let mut agents = HashMap::new();
            agents.insert("greeter".to_string(), sample_agent_config("greeter"));
            let provider = HostConfigProvider::new(agents);

            let tenant = TenantContext::new("acme", "prod");
            let resolved = provider
                .agent_config(&tenant, "greeter")
                .await
                .expect("known agent resolves");

            assert_eq!(resolved.agent_id, "greeter");
        }

        #[tokio::test]
        async fn overlay_provider_replaces_tools_from_manifest() {
            use greentic_aw_runtime::ManifestToolOverlayProvider;
            use greentic_aw_runtime::config::ToolRef;
            use greentic_aw_runtime::config_provider::ConfigProvider;

            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("greeter.json"),
                r#"{"id":"greeter","display_name":"G",
                "tenancy":{"tenant":"t","team_policy":"disabled"},
                "locale":{"worker_default_locale":"en-US","policy":"worker_default",
                          "propagation":"current_task_only","output":"worker_default"},
                "extension_tools":[{"extension_id":"greentic.tavily","extension_version":"1.0.0",
                  "tool_name":"web_search","description":"d","input_schema_json":"{\"type\":\"object\"}",
                  "capabilities":["agentic_worker"],"agentic_worker_metadata":{}}]}"#,
            )
            .unwrap();

            let mut agents = HashMap::new();
            agents.insert("greeter".to_string(), sample_agent_config("greeter"));
            let provider = ManifestToolOverlayProvider::new(
                HostConfigProvider::new(agents),
                tmp.path().to_path_buf(),
            );

            let tenant = TenantContext::new("acme", "prod");
            let cfg = provider.agent_config(&tenant, "greeter").await.unwrap();
            assert_eq!(
                cfg.tools,
                vec![ToolRef {
                    extension_id: "greentic.tavily".into(),
                    tool_name: "web_search".into()
                }]
            );
        }

        #[test]
        fn bridge_credential_defaults_provider_and_model() {
            let c = super::bridge_credential(None, None, "sk-x".into(), None).unwrap();
            assert_eq!(c.provider, "openai");
            assert_eq!(c.model, "gpt-4o");
            assert_eq!(c.api_key, "sk-x");
            assert!(c.base_url.is_none());
        }

        #[test]
        fn bridge_credential_honors_explicit_parts() {
            let c = super::bridge_credential(
                Some("anthropic".into()),
                Some("claude-x".into()),
                "sk-ant".into(),
                Some("https://proxy".into()),
            )
            .unwrap();
            assert_eq!(c.provider, "anthropic");
            assert_eq!(c.model, "claude-x");
            assert_eq!(c.base_url.as_deref(), Some("https://proxy"));
        }

        #[test]
        fn bridge_credential_none_without_key() {
            assert!(
                super::bridge_credential(Some("openai".into()), None, "  ".into(), None).is_none()
            );
        }

        #[tokio::test]
        async fn host_config_provider_returns_not_found_for_unknown_agent() {
            use greentic_aw_runtime::error::ConfigError;

            let provider = HostConfigProvider::new(HashMap::new());

            let tenant = TenantContext::new("acme", "prod");
            let result = provider.agent_config(&tenant, "missing").await;

            assert!(matches!(result, Err(ConfigError::AgentNotFound(_))));
        }

        // -----------------------------------------------------------------------
        // merge_agent_sources tests
        // -----------------------------------------------------------------------

        #[test]
        fn merge_pack_only_agent_resolves() {
            let mut pack_agents = HashMap::new();
            pack_agents.insert("pack-bot".to_string(), sample_agent_config("pack-bot"));

            let merged = super::merge_agent_sources(pack_agents, HashMap::new());

            assert!(merged.contains_key("pack-bot"));
            assert_eq!(merged["pack-bot"].agent_id, "pack-bot");
        }

        #[test]
        fn merge_operator_only_agent_resolves() {
            let mut operator_agents = HashMap::new();
            operator_agents.insert("op-bot".to_string(), sample_agent_config("op-bot"));

            let merged = super::merge_agent_sources(HashMap::new(), operator_agents);

            assert!(merged.contains_key("op-bot"));
            assert_eq!(merged["op-bot"].agent_id, "op-bot");
        }

        #[test]
        fn merge_operator_wins_on_collision() {
            let mut pack_agents = HashMap::new();
            let mut pack_config = sample_agent_config("shared-bot");
            pack_config.system_prompt = "pack prompt".to_string();
            pack_agents.insert("shared-bot".to_string(), pack_config);

            let mut operator_agents = HashMap::new();
            let mut operator_config = sample_agent_config("shared-bot");
            operator_config.system_prompt = "operator prompt".to_string();
            operator_agents.insert("shared-bot".to_string(), operator_config);

            let merged = super::merge_agent_sources(pack_agents, operator_agents);

            assert_eq!(merged.len(), 1);
            assert_eq!(
                merged["shared-bot"].system_prompt, "operator prompt",
                "operator config must override pack config on agent_id collision"
            );
        }

        // -----------------------------------------------------------------------
        // agent_configs_from_manifest tests
        // -----------------------------------------------------------------------

        #[test]
        fn deserialize_agent_blob_produces_correct_config() {
            let blob = serde_json::json!({
                "agent_id": "demo-agent",
                "system_prompt": "You are helpful.",
                "tools": [],
                "llm": {
                    "provider": "openai",
                    "model": "gpt-4o-mini"
                },
                "limits": {
                    "max_iter": 5,
                    "timeout": 30,
                    "max_history_turns": 10,
                    "llm_retry_attempts": 2,
                    "llm_retry_backoff": 500,
                    "provider_failure_message": null,
                    "daily_token_cap_per_tenant": null
                }
            });

            let config: AgentConfig =
                serde_json::from_value(blob).expect("valid blob must deserialize");

            assert_eq!(config.agent_id, "demo-agent");
            assert_eq!(config.system_prompt, "You are helpful.");
            assert_eq!(config.limits.max_iter, 5);
            assert_eq!(config.limits.timeout, std::time::Duration::from_secs(30));
        }

        #[test]
        fn agent_configs_from_manifest_skips_malformed_blobs() {
            use std::collections::BTreeMap;

            let mut blobs: BTreeMap<String, serde_json::Value> = BTreeMap::new();

            // Valid agent blob
            blobs.insert(
                "good-agent".to_string(),
                serde_json::json!({
                    "agent_id": "good-agent",
                    "system_prompt": "Valid.",
                    "tools": [],
                    "llm": { "provider": "openai", "model": "gpt-4o-mini" },
                    "limits": {
                        "max_iter": 8,
                        "timeout": 60,
                        "max_history_turns": 20,
                        "llm_retry_attempts": 3,
                        "llm_retry_backoff": 250,
                        "provider_failure_message": null,
                        "daily_token_cap_per_tenant": null
                    }
                }),
            );

            // Malformed blob (missing required fields)
            blobs.insert(
                "bad-agent".to_string(),
                serde_json::json!({ "broken": true }),
            );

            let configs = super::agent_configs_from_manifest("test-pack", &blobs);

            assert_eq!(configs.len(), 1, "malformed blob must be skipped");
            assert!(configs.contains_key("good-agent"));
            assert!(!configs.contains_key("bad-agent"));
        }

        #[test]
        #[serial_test::serial]
        #[allow(unsafe_code)]
        fn registry_from_env_requires_both_vars() {
            // SAFETY: #[serial] serializes env-mutating tests (crate convention),
            // so no concurrent test observes a torn env; vars cleaned up at the end.
            unsafe {
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
            assert!(super::registry_from_env().is_none());

            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", "http://localhost:9999");
            }
            assert!(
                super::registry_from_env().is_none(),
                "endpoint alone is not enough"
            );

            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_TOKEN", "gtc_live_x");
            }
            assert!(super::registry_from_env().is_some());

            unsafe {
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
        }

        #[test]
        #[serial_test::serial]
        #[allow(unsafe_code)]
        fn mcp_source_from_env_default_on_with_opt_out() {
            // SAFETY: #[serial] serializes env-mutating tests (crate convention),
            // so no concurrent test observes a torn env; vars cleaned up at the end.
            unsafe {
                std::env::remove_var("GREENTIC_AW_MCP");
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }

            // (a) Default-on: endpoint + token present, gate unset → Some.
            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", "http://localhost:9999");
                std::env::set_var("GREENTIC_AW_ADMIN_TOKEN", "gtc_live_x");
            }
            assert!(
                super::mcp_source_from_env().is_some(),
                "MCP is on by default when admin credentials are configured"
            );

            // (b) Explicit opt-out wins even with full credentials.
            unsafe {
                std::env::set_var("GREENTIC_AW_MCP", "0");
            }
            assert!(
                super::mcp_source_from_env().is_none(),
                "GREENTIC_AW_MCP=0 disables MCP regardless of credentials"
            );

            // (b') Legacy opt-in value still enables (any non-"0" value does).
            unsafe {
                std::env::set_var("GREENTIC_AW_MCP", "1");
            }
            assert!(super::mcp_source_from_env().is_some());

            // (c) Missing credential → None even without an opt-out.
            unsafe {
                std::env::remove_var("GREENTIC_AW_MCP");
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
            }
            assert!(
                super::mcp_source_from_env().is_none(),
                "no endpoint → no MCP source"
            );

            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", "http://localhost:9999");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
            assert!(
                super::mcp_source_from_env().is_none(),
                "no token → no MCP source"
            );

            unsafe {
                std::env::remove_var("GREENTIC_AW_MCP");
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
        }

        #[test]
        #[allow(unsafe_code)]
        fn load_process_agent_configs_reads_full_configs_and_skips_bad_files() {
            let dir = tempfile::tempdir().expect("tempdir");

            // Valid full AgentConfig keyed by file stem.
            let good = sample_agent_config("greeter");
            std::fs::write(
                dir.path().join("greeter.json"),
                serde_json::to_vec(&good).expect("serialize"),
            )
            .expect("write good");

            // Malformed JSON — skipped, must not abort the load.
            std::fs::write(dir.path().join("broken.json"), b"{ not json").expect("write broken");

            // Non-JSON file — ignored.
            std::fs::write(dir.path().join("README.md"), b"ignore me").expect("write md");

            let previous = std::env::var("GREENTIC_AGENT_MANIFESTS_DIR").ok();
            unsafe {
                std::env::set_var("GREENTIC_AGENT_MANIFESTS_DIR", dir.path());
            }
            let loaded = super::load_process_agent_configs();
            unsafe {
                match &previous {
                    Some(value) => std::env::set_var("GREENTIC_AGENT_MANIFESTS_DIR", value),
                    None => std::env::remove_var("GREENTIC_AGENT_MANIFESTS_DIR"),
                }
            }

            assert_eq!(loaded.len(), 1, "only the valid config should load");
            assert!(loaded.contains_key("greeter"));
            assert_eq!(loaded["greeter"].agent_id, "greeter");
        }
    }
}

#[allow(clippy::items_after_test_module)] // helper fn + re-exports follow
#[cfg(test)]
mod gating_tests {
    use super::should_serve_agentic_inproc;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn skips_when_opt_in_unset() {
        let env = env_from(&[("GREENTIC_EVENTS_NATS_URL", "nats://127.0.0.1:4222")]);
        assert!(!should_serve_agentic_inproc(env));
    }

    #[test]
    fn skips_when_nats_url_unset() {
        let env = env_from(&[("GREENTIC_AGENTIC_SERVE_INPROC", "1")]);
        assert!(!should_serve_agentic_inproc(env));
    }

    #[test]
    fn skips_when_nats_url_blank() {
        let env = env_from(&[
            ("GREENTIC_AGENTIC_SERVE_INPROC", "1"),
            ("GREENTIC_EVENTS_NATS_URL", "   "),
        ]);
        assert!(!should_serve_agentic_inproc(env));
    }

    #[test]
    fn serves_when_both_set() {
        for truthy in ["1", "true", "TRUE", "yes", "on"] {
            let env = env_from(&[
                ("GREENTIC_AGENTIC_SERVE_INPROC", truthy),
                ("GREENTIC_EVENTS_NATS_URL", "nats://127.0.0.1:4222"),
            ]);
            assert!(should_serve_agentic_inproc(env), "{truthy} should enable");
        }
    }

    #[test]
    fn skips_on_falsey_opt_in() {
        for falsey in ["0", "false", "no", "off", "maybe"] {
            let env = env_from(&[
                ("GREENTIC_AGENTIC_SERVE_INPROC", falsey),
                ("GREENTIC_EVENTS_NATS_URL", "nats://127.0.0.1:4222"),
            ]);
            assert!(!should_serve_agentic_inproc(env), "{falsey} should skip");
        }
    }
}

/// Decide whether the runner process should host the agentic-worker NATS
/// service in-process (the opt-in co-host path).
///
/// Returns `true` only when BOTH gates are satisfied:
/// - `GREENTIC_AGENTIC_SERVE_INPROC` is truthy (`1`/`true`/`yes`/`on`,
///   case-insensitive) — opt-in, default OFF; and
/// - `GREENTIC_EVENTS_NATS_URL` is set to a non-empty value (no NATS bus means
///   nothing to serve on).
///
/// Pure over its `get_env` closure so it is unit-testable without touching the
/// real process environment. Feature-independent (no `agentic-worker` gate) so
/// the gating logic stays trivially testable; the actual spawn is gated at the
/// call site.
pub fn should_serve_agentic_inproc(get_env: impl Fn(&str) -> Option<String>) -> bool {
    let opt_in = get_env("GREENTIC_AGENTIC_SERVE_INPROC")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    let nats_set = get_env("GREENTIC_EVENTS_NATS_URL")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    opt_in && nats_set
}

#[cfg(feature = "agentic-worker")]
pub use aw::{
    HostConfigProvider, RuntimeAgentNodeHandler, agent_configs_from_manifest,
    build_agent_node_handler, build_agent_runtime, load_process_agent_configs, merge_agent_sources,
    serve_agentic,
};

#[cfg(feature = "agentic-worker")]
pub(crate) use aw::{build_ext_runtime, build_llm_backend};
