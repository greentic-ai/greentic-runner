//! NATS-consuming serve mode for the agentic-worker runtime.
//!
//! This is the agentic-side counterpart to the runner's `agentic.call` flow
//! node (the out-of-process dispatch path). It mirrors the proven
//! `greentic-sorx` pattern: a long-lived [`AgentDispatchInvoker`] wraps an
//! [`AgentRuntime`] and the [`aw_event_bridge::run_bridge`] consumer turns
//! `greentic.agentic.request.v1` messages into one `AgentRuntime::step` call,
//! publishing the reply on `greentic.agentic.response.v1`.
//!
//! Gated behind the `serve` feature so the core library has no NATS dependency.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use aw_event_bridge::{AgentDispatchInvoker, InvokeOutcome, run_bridge, run_bridge_jetstream};
use serde_json::{Value, json};

use crate::tenant::TenantContext;
use crate::{AgentInput, AgentRuntime};

/// Production [`AgentDispatchInvoker`] wrapping a shared [`AgentRuntime`].
///
/// Maps the dispatch contract onto one Plan-Act-Observe step:
/// * `target` -> `agent_id`
/// * `input` -> [`AgentInput`] (the `user_text` field is extracted exactly as
///   the in-process `agent_node` handler does)
/// * the correlation/idempotency hint -> session id (so the same logical
///   conversation resumes the same agentic state)
///
/// The successful [`AgentOutput`] is serialised to
/// `{ "reply", "trail", "terminated_by" }`, matching the in-process node output
/// shape so downstream flow nodes see an identical payload regardless of path.
///
/// [`AgentOutput`]: crate::AgentOutput
pub struct RuntimeAgentDispatchInvoker {
    runtime: Arc<AgentRuntime>,
}

impl RuntimeAgentDispatchInvoker {
    /// Wrap a shared [`AgentRuntime`] in a dispatch invoker.
    #[must_use]
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self { runtime }
    }
}

/// Extract the user text from the opaque dispatch input.
///
/// Accepts either `{"user_text": "..."}` (the runner's `agentic.call` node
/// shape) or `{"text": "..."}` (raw [`AgentInput`] shape); a bare JSON string is
/// also accepted. Returns an empty string when no text is present so a tool-only
/// or system-prompt-only step can still run.
fn extract_user_text(input: &Value) -> String {
    input
        .get("user_text")
        .or_else(|| input.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| input.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Resolve the session id for the step.
///
/// Prefers an explicit `session_id` field in the input; otherwise falls back to
/// the dispatch correlation/idempotency hint (which the runner derives from the
/// flow session). A final synthetic default keeps the function total.
fn resolve_session_id(input: &Value, idempotency_key: Option<&str>) -> String {
    input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| idempotency_key.map(str::to_string))
        .filter(|hint| !hint.is_empty())
        .unwrap_or_else(|| "agentic-dispatch".to_string())
}

#[async_trait]
impl AgentDispatchInvoker for RuntimeAgentDispatchInvoker {
    async fn invoke(
        &self,
        tenant: &str,
        env: &str,
        target: &str,
        _operation: &str,
        input: Value,
        idempotency_key: Option<&str>,
    ) -> Result<InvokeOutcome> {
        let user_text = extract_user_text(&input);
        let session_id = resolve_session_id(&input, idempotency_key);
        let tenant_ctx = TenantContext::new(tenant, env);

        let output = self
            .runtime
            .step(
                tenant_ctx,
                &session_id,
                target,
                AgentInput { text: user_text },
            )
            .await
            .with_context(|| format!("agentic step failed for agent '{target}'"))?;

        Ok(InvokeOutcome {
            ok: true,
            output: json!({
                "reply": output.reply,
                "trail": output.trail,
                "terminated_by": output.terminated_by,
            }),
            events: vec![],
        })
    }
}

/// Whether the agentic serve consumer uses JetStream (durable) vs core-NATS.
///
/// Default ON; set `GREENTIC_AW_JETSTREAM=0|false|no|off` to force the legacy
/// core-NATS path.
#[must_use]
pub fn use_jetstream(get_env: impl Fn(&str) -> Option<String>) -> bool {
    match get_env("GREENTIC_AW_JETSTREAM") {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => true,
    }
}

/// Connect to NATS at `nats_url` and serve agentic dispatch requests forever.
///
/// Uses JetStream durable consumer by default; set `GREENTIC_AW_JETSTREAM=0`
/// (or `off`/`false`/`no`) to fall back to the legacy core-NATS consumer.
///
/// Blocks until the subscription stream ends or the process is signalled. The
/// caller supplies the constructed [`AgentRuntime`] (production or test-mock).
pub async fn serve(nats_url: &str, runtime: Arc<AgentRuntime>) -> Result<()> {
    let client = async_nats::connect(nats_url)
        .await
        .with_context(|| format!("connecting to NATS at {nats_url}"))?;
    tracing::info!(
        nats_url,
        subject = aw_event_bridge::request_topic(aw_event_bridge::RUNTIME_NAME),
        "aw event bridge connected; serving agentic dispatch"
    );
    let invoker = Arc::new(RuntimeAgentDispatchInvoker::new(runtime));
    if use_jetstream(|k| std::env::var(k).ok()) {
        tracing::info!(nats_url, "aw serve: JetStream durable consumer");
        run_bridge_jetstream(client, invoker).await
    } else {
        tracing::info!(nats_url, "aw serve: core-NATS consumer (legacy)");
        run_bridge(client, invoker).await
    }
}

/// Build a credit-free, broker-free [`AgentRuntime`] that returns a canned reply
/// for any agent id, using the `test-mock` test doubles.
///
/// This is the key to a live e2e (runner `agentic.call` -> aw serve over NATS)
/// without real LLM credits or Redis: every dispatched step resolves the agent
/// against an in-memory config provider and returns `reply` from a scripted mock
/// LLM. `reply` is repeated so a session can take several steps.
#[cfg(feature = "test-mock")]
#[must_use]
pub fn build_test_mock_runtime(agent_id: &str, reply: &str) -> Arc<AgentRuntime> {
    use crate::cost::MockTokenMeter;
    use crate::llm::LlmResponse;
    use crate::mock::{
        MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
    };
    use crate::tools::ToolLedger;
    use crate::{AgentConfig, AgentLimits, LlmProviderRef};

    // A generous scripted queue so multi-turn sessions don't exhaust it.
    let scripted = (0..64)
        .map(|_| {
            Ok(LlmResponse {
                content: Some(reply.to_string()),
                tool_calls: vec![],
                tokens_in: 1,
                tokens_out: 1,
            })
        })
        .collect();
    let llm = Arc::new(MockLlmBackend::new(scripted));
    let store = Arc::new(MockAgentStateStore::new());
    let telemetry = Arc::new(MockTelemetry::new());

    // Register the agent for every tenant the test might use. The mock keys by
    // `tenant.key_prefix():agent_id`; we register the common defaults so the
    // dispatched tenant/env resolves.
    let config_provider = MockConfigProvider::new();
    let agent_config = AgentConfig {
        agent_id: agent_id.to_string(),
        system_prompt: "test-mock agent".to_string(),
        tools: vec![],
        llm: LlmProviderRef {
            provider: "mock".to_string(),
            model: "mock".to_string(),
            credential_ref: None,
        },
        limits: AgentLimits::default(),
        memory: None,
    };
    for (tenant, env) in [
        ("default", "default"),
        ("acme", "prod"),
        ("t", "e"),
        ("sorx", "default"),
    ] {
        config_provider.insert(
            &TenantContext::new(tenant, env),
            agent_id,
            agent_config.clone(),
        );
    }
    let config_provider = Arc::new(config_provider);

    let token_meter = Arc::new(MockTokenMeter::new(0));
    let ledger: Arc<dyn ToolLedger> = Arc::new(NoopToolLedger);
    let ext_runtime = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());

    Arc::new(AgentRuntime::new(
        config_provider,
        store,
        ext_runtime,
        llm,
        telemetry,
        token_meter,
        ledger,
        None,
    ))
}

#[cfg(test)]
mod env_gate_tests {
    use super::use_jetstream;

    #[test]
    fn jetstream_default_on_unless_disabled() {
        assert!(use_jetstream(|_| None)); // default ON
        assert!(!use_jetstream(|k| (k == "GREENTIC_AW_JETSTREAM").then(|| "0".to_string())));
        assert!(!use_jetstream(|k| (k == "GREENTIC_AW_JETSTREAM").then(|| "off".to_string())));
        assert!(use_jetstream(|k| (k == "GREENTIC_AW_JETSTREAM").then(|| "on".to_string())));
    }
}

#[cfg(all(test, feature = "test-mock"))]
mod tests {
    use super::*;

    #[test]
    fn extract_user_text_accepts_node_and_raw_shapes() {
        assert_eq!(extract_user_text(&json!({"user_text": "hi"})), "hi");
        assert_eq!(extract_user_text(&json!({"text": "yo"})), "yo");
        assert_eq!(extract_user_text(&json!("bare")), "bare");
        assert_eq!(extract_user_text(&json!({"other": 1})), "");
    }

    #[test]
    fn resolve_session_id_prefers_explicit_then_idempotency() {
        assert_eq!(
            resolve_session_id(&json!({"session_id": "s1"}), Some("corr")),
            "s1"
        );
        assert_eq!(resolve_session_id(&json!({}), Some("corr")), "corr");
        assert_eq!(resolve_session_id(&json!({}), Some("")), "agentic-dispatch");
        assert_eq!(resolve_session_id(&json!({}), None), "agentic-dispatch");
    }

    #[tokio::test]
    #[allow(clippy::expect_used)] // test asserts the mock path succeeds
    async fn mock_invoker_returns_reply_output() {
        let runtime = build_test_mock_runtime("greeter", "pong");
        let invoker = RuntimeAgentDispatchInvoker::new(runtime);

        let outcome = invoker
            .invoke(
                "acme",
                "prod",
                "greeter",
                "",
                json!({"user_text": "ping"}),
                Some("sess-1::pack=p::flow=f"),
            )
            .await
            .expect("mock invoke succeeds");

        assert!(outcome.ok);
        assert_eq!(outcome.output["reply"], json!("pong"));
        assert_eq!(outcome.output["terminated_by"], json!("final_reply"));
    }
}
