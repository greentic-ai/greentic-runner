//! The Plan-Act-Observe loop must hand the billing sink the model id it
//! actually called, so cloud-commerce can group worker spend by `model`.
//! Uses test-mock backends — no Redis, no network.

#![cfg(feature = "test-mock")]

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use greentic_aw_runtime::billing::{BillingError, BillingMeter};
use greentic_aw_runtime::cost::MockTokenMeter;
use greentic_aw_runtime::llm::LlmResponse;
use greentic_aw_runtime::mock::{
    MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
};
use greentic_aw_runtime::tenant::TenantContext;
use greentic_aw_runtime::{
    AgentConfig, AgentInput, AgentLimits, AgentRuntime, LlmProviderRef, ToolRef,
};

/// One recorded `emit` call: (agent_id, model, env_id, user_email).
type EmitCall = (String, String, String, Option<String>);

#[derive(Default)]
struct RecordingBillingMeter {
    calls: Mutex<Vec<EmitCall>>,
}

impl BillingMeter for RecordingBillingMeter {
    fn emit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        _input_tokens: u64,
        _output_tokens: u64,
        agent_id: &'a str,
        model: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BillingError>> + Send + 'a>> {
        #[allow(clippy::expect_used)]
        self.calls
            .lock()
            .expect("recording meter lock poisoned")
            .push((
                agent_id.to_string(),
                model.to_string(),
                tenant.env_id.clone(),
                tenant.user_email.clone(),
            ));
        Box::pin(async { Ok(()) })
    }

    fn over_budget<'a>(
        &'a self,
        _tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(false))
    }
}

fn cfg(model: &str) -> AgentConfig {
    AgentConfig {
        agent_id: "a".into(),
        system_prompt: "sys".into(),
        tools: Vec::<ToolRef>::new(),
        guardrails: vec![],
        llm: LlmProviderRef {
            provider: "mock".into(),
            model: model.into(),
            credential_ref: None,
        },
        limits: AgentLimits {
            max_iter: 8,
            timeout: Duration::from_millis(60_000),
            ..AgentLimits::default()
        },
        memory: None,
        knowledge: None,
    }
}

fn final_reply(text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(text.into()),
        tool_calls: vec![],
        tokens_in: 7,
        tokens_out: 3,
    }
}

#[tokio::test]
async fn loop_emits_billing_with_the_configured_model() {
    let tenant = TenantContext::new("acme", "prod").with_user_email(Some("u@x.com".into()));
    let cp = MockConfigProvider::new();
    cp.insert(&tenant, "a", cfg("claude-3-haiku"));

    let meter = Arc::new(RecordingBillingMeter::default());
    let runtime = AgentRuntime::new(
        Arc::new(cp),
        Arc::new(MockAgentStateStore::new()),
        Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test()),
        Arc::new(MockLlmBackend::new(vec![Ok(final_reply("hi"))])),
        Arc::new(MockTelemetry::new()),
        Arc::new(MockTokenMeter::new(0)),
        Arc::new(NoopToolLedger),
        None,
    )
    .with_billing_meter(meter.clone());

    let out = runtime
        .step(
            tenant,
            "s",
            "a",
            AgentInput {
                text: "hello".into(),
            },
        )
        .await
        .expect("step should succeed");
    assert_eq!(out.reply, "hi");

    let calls = meter.calls.lock().expect("lock poisoned");
    assert_eq!(calls.len(), 1, "one LLM iteration → one billing emit");
    let (agent_id, model, env_id, user_email) = &calls[0];
    assert_eq!(agent_id, "a");
    assert_eq!(model, "claude-3-haiku");
    assert_eq!(env_id, "prod");
    assert_eq!(user_email.as_deref(), Some("u@x.com"));
}
