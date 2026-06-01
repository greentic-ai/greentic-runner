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
    use crate::config::HostConfig;

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

    /// Build the production [`greentic_ext_runtime::ExtensionRuntime`] used for
    /// tool dispatch, wrapped in an [`Arc`] for sharing with [`AgentRuntime`].
    ///
    /// On construction failure (e.g. wasmtime engine init), logs the error and
    /// returns `None`; the caller then disables `DwAgent` nodes rather than
    /// panicking.
    fn build_ext_runtime() -> Option<Arc<greentic_ext_runtime::ExtensionRuntime>> {
        use greentic_ext_runtime::{DiscoveryPaths, ExtensionRuntime, RuntimeConfig};

        let paths = DiscoveryPaths::new(extension_discovery_dir());
        match ExtensionRuntime::new(RuntimeConfig::from_paths(paths)) {
            Ok(runtime) => Some(Arc::new(runtime)),
            Err(error) => {
                tracing::warn!(error = %error, "extension runtime init failed; DwAgent nodes disabled");
                None
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
    /// - no agents are declared in [`HostConfig::agents`];
    /// - `GREENTIC_AW_REDIS_URL` is unset/empty;
    /// - the AW Redis connection fails;
    /// - the extension runtime fails to initialise.
    ///
    /// Redis is sourced from the environment because the runner uses an
    /// in-memory flow-state store by default and carries no Redis URL in
    /// [`HostConfig`]; this mirrors the existing env-config convention.
    pub async fn build_agent_node_handler(
        config: &HostConfig,
    ) -> Option<Arc<dyn AgentNodeHandler>> {
        use std::time::Duration;

        use greentic_aw_runtime::LayeredConfigProvider;
        use greentic_aw_runtime::ManifestToolOverlayProvider;
        use greentic_aw_runtime::config_provider::CachingConfigProvider;
        use greentic_aw_runtime::cost::RedisTokenMeter;
        use greentic_aw_runtime::tools::RedisToolLedger;
        use greentic_aw_runtime::{
            ExtensionLlmBackend, LlmBackend, OpenAiLlmBackend, OtelTelemetry, RedisAgentStateStore,
            RetryingLlmBackend,
        };

        if config.agents.is_empty() {
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
        let llm: Arc<dyn LlmBackend> = match std::env::var("GREENTIC_AW_LLM_EXTENSION")
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
        };

        let overlay = ManifestToolOverlayProvider::new(
            HostConfigProvider::new(config.agents.clone()),
            manifests_discovery_dir(),
        );
        // When the admin agent registry is configured, pull from it first and
        // fall back to the local overlay on miss/outage; otherwise use the
        // local overlay alone. The result is cached either way.
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
        ));

        tracing::info!(agent_count = config.agents.len(), "AW runtime constructed");
        Some(Arc::new(RuntimeAgentNodeHandler::new(runtime)))
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

        #[test]
        #[allow(unsafe_code)]
        fn registry_from_env_requires_both_vars() {
            // SAFETY: single-threaded test; vars cleaned up at the end.
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
    }
}

#[cfg(feature = "agentic-worker")]
pub use aw::{HostConfigProvider, RuntimeAgentNodeHandler, build_agent_node_handler};
