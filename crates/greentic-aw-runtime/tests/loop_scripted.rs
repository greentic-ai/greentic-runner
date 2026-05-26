//! Scripted-LLM unit tests for the Plan-Act-Observe loop.
//! Uses test-mock backends — no Redis, no network.

#![cfg(feature = "test-mock")]

use std::sync::Arc;
use std::time::Duration;

use greentic_aw_runtime::cost::MockTokenMeter;
use greentic_aw_runtime::error::{AgentError, TerminationReason};
use greentic_aw_runtime::llm::LlmResponse;
use greentic_aw_runtime::mock::{
    MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
};
use greentic_aw_runtime::state::ToolCallRecord;
use greentic_aw_runtime::tenant::TenantContext;
use greentic_aw_runtime::{
    AgentConfig, AgentInput, AgentLimits, AgentRuntime, AgentStep, LlmProviderRef, ToolRef,
};

fn cfg(max_iter: u32, timeout_ms: u64, tools: Vec<ToolRef>, cap: Option<u32>) -> AgentConfig {
    AgentConfig {
        agent_id: "a".into(),
        system_prompt: "sys".into(),
        tools,
        llm: LlmProviderRef {
            provider: "mock".into(),
            model: "m".into(),
        },
        limits: AgentLimits {
            max_iter,
            timeout: Duration::from_millis(timeout_ms),
            daily_token_cap_per_tenant: cap,
            ..AgentLimits::default()
        },
    }
}

fn final_reply(text: &str) -> LlmResponse {
    LlmResponse {
        content: Some(text.into()),
        tool_calls: vec![],
        tokens_in: 5,
        tokens_out: 5,
    }
}

fn tool_call(call_id: &str, ext: &str, tool: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCallRecord {
            call_id: call_id.into(),
            extension_id: ext.into(),
            tool_name: tool.into(),
            args: serde_json::json!({}),
        }],
        tokens_in: 5,
        tokens_out: 5,
    }
}

fn build_runtime(
    llm_script: Vec<Result<LlmResponse, greentic_aw_runtime::error::LlmError>>,
    cfg_inner: AgentConfig,
    token_used: u64,
) -> (AgentRuntime, Arc<MockTelemetry>, TenantContext) {
    let llm = Arc::new(MockLlmBackend::new(llm_script));
    let store = Arc::new(MockAgentStateStore::new());
    let telemetry = Arc::new(MockTelemetry::new());
    let cp = MockConfigProvider::new();
    let tc = TenantContext::new("acme", "prod");
    cp.insert(&tc, "a", cfg_inner);
    let cp = Arc::new(cp);
    let token_meter = Arc::new(MockTokenMeter::new(token_used));
    let ledger = Arc::new(NoopToolLedger);
    let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
    let rt = AgentRuntime::new(cp, store, ext, llm, telemetry.clone(), token_meter, ledger);
    (rt, telemetry, tc)
}

#[tokio::test]
async fn happy_path_one_iteration() {
    let (rt, tel, tc) = build_runtime(vec![Ok(final_reply("hi"))], cfg(8, 60_000, vec![], None), 0);
    let out = rt
        .step(
            tc,
            "s",
            "a",
            AgentInput {
                text: "hello".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(out.reply, "hi");
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(tel.recorded.lock().unwrap()[0].iterations, 1);
}

#[tokio::test]
async fn max_iterations_terminates_loop() {
    // LLM keeps emitting tool calls for a NON-allowed tool (empty allow-list)
    // → each blocked → loop exhausts at max_iter=3.
    let script = vec![
        Ok(tool_call("c1", "http", "fetch")),
        Ok(tool_call("c2", "http", "fetch")),
        Ok(tool_call("c3", "http", "fetch")),
    ];
    let (rt, tel, tc) = build_runtime(script, cfg(3, 60_000, vec![], None), 0);
    let out = rt
        .step(tc, "s", "a", AgentInput { text: "go".into() })
        .await
        .unwrap();
    assert_eq!(out.terminated_by, TerminationReason::MaxIterations);
    assert_eq!(tel.recorded.lock().unwrap()[0].iterations, 3);
}

#[tokio::test]
async fn timeout_terminates_loop() {
    // timeout=0ms → the iteration-start timeout check fires immediately.
    let (rt, tel, tc) = build_runtime(vec![Ok(final_reply("never"))], cfg(8, 0, vec![], None), 0);
    let out = rt
        .step(tc, "s", "a", AgentInput { text: "x".into() })
        .await
        .unwrap();
    assert_eq!(out.terminated_by, TerminationReason::Timeout);
    assert_eq!(tel.recorded.lock().unwrap()[0].iterations, 1);
}

#[tokio::test]
async fn tool_not_allowed_becomes_observation_then_reply() {
    // First response: tool call for a tool NOT in the (empty) allow-list →
    // blocked observation. Second response: final reply.
    let script = vec![
        Ok(tool_call("c1", "http", "fetch")),
        Ok(final_reply("ok done")),
    ];
    let (rt, _tel, tc) = build_runtime(script, cfg(4, 60_000, vec![], None), 0);
    let out = rt
        .step(tc, "s", "a", AgentInput { text: "go".into() })
        .await
        .unwrap();
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(out.reply, "ok done");
    assert!(
        out.trail
            .iter()
            .any(|s| matches!(s, AgentStep::ToolCallBlocked { .. }))
    );
}

#[tokio::test]
async fn tool_dispatch_error_becomes_observation_then_reply() {
    // Tool IS allowed, but for_test ExtensionRuntime has no extensions →
    // invoke_tool returns NotFound → dispatch error becomes an observation
    // (spec §6), loop continues, second response is the final reply.
    let allowed = vec![ToolRef {
        extension_id: "http".into(),
        tool_name: "fetch".into(),
    }];
    let script = vec![
        Ok(tool_call("c1", "http", "fetch")),
        Ok(final_reply("recovered")),
    ];
    let (rt, _tel, tc) = build_runtime(script, cfg(4, 60_000, allowed, None), 0);
    let out = rt
        .step(tc, "s", "a", AgentInput { text: "go".into() })
        .await
        .unwrap();
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(out.reply, "recovered");
    // The failed tool call appears in the trail as a ToolCall with an error result.
    assert!(
        out.trail
            .iter()
            .any(|s| matches!(s, AgentStep::ToolCall { .. }))
    );
}

#[tokio::test]
async fn token_budget_exceeded_returns_error() {
    // Daily cap 10, already-used 100 → gate trips before any LLM call.
    let (rt, _tel, tc) = build_runtime(
        vec![Ok(final_reply("hi"))],
        cfg(8, 60_000, vec![], Some(10)),
        100,
    );
    let err = rt
        .step(tc, "s", "a", AgentInput { text: "x".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::TokenBudgetExceeded));
}

#[tokio::test]
async fn mixed_text_and_tool_calls_executes_tool_discards_text() {
    // First response: BOTH content and a tool_call (allowed). tool_calls win:
    // content is discarded, the tool dispatches (fails → observation), loop
    // continues. Second response: the real final reply.
    let allowed = vec![ToolRef {
        extension_id: "http".into(),
        tool_name: "fetch".into(),
    }];
    let mixed = LlmResponse {
        content: Some("internal reasoning".into()),
        tool_calls: vec![ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            args: serde_json::json!({}),
        }],
        tokens_in: 5,
        tokens_out: 5,
    };
    let script = vec![Ok(mixed), Ok(final_reply("the real answer"))];
    let (rt, _tel, tc) = build_runtime(script, cfg(4, 60_000, allowed, None), 0);
    let out = rt
        .step(tc, "s", "a", AgentInput { text: "go".into() })
        .await
        .unwrap();
    assert_eq!(out.reply, "the real answer");
    assert_ne!(out.reply, "internal reasoning");
}
