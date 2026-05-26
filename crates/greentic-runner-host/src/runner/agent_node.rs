use std::sync::Arc;

use anyhow::Result;
use greentic_aw_runtime::{AgentInput, AgentRuntime, AgentStep, TenantContext};
use serde_json::{Value, json};

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

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_aw_runtime::cost::MockTokenMeter;
    use greentic_aw_runtime::llm::LlmResponse;
    use greentic_aw_runtime::mock::{
        MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
    };
    use greentic_aw_runtime::{AgentConfig, AgentLimits, LlmProviderRef};

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
}
