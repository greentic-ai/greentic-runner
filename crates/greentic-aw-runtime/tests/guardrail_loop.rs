//! Integration tests proving that the guardrail chain is wired into `run_step`.
//!
//! These tests exercise the fail-closed inbound path: when a mandatory
//! guardrail cannot be resolved from the capability registry, `run_step` must
//! return `AgentError::GuardrailDenied` *before* any LLM call is attempted.
//! This is achievable without a populated registry or a live LLM because the
//! `ExtensionRuntime::for_test()` has an empty capability registry — perfect
//! for triggering the fail-closed branch.

#![cfg(feature = "test-mock")]

use std::sync::Arc;
use std::time::Duration;

use greentic_aw_runtime::config::{AgentConfig, AgentLimits, GuardrailRef, LlmProviderRef};
use greentic_aw_runtime::cost::MockTokenMeter;
use greentic_aw_runtime::error::AgentError;
use greentic_aw_runtime::guardrail::{GuardrailDirection, StaticGuardrailPolicy};
use greentic_aw_runtime::llm::LlmResponse;
use greentic_aw_runtime::mock::{
    MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
};
use greentic_aw_runtime::tenant::TenantContext;
use greentic_aw_runtime::{AgentInput, AgentRuntime};

/// Build a minimal `AgentRuntime` with a single LLM script entry (which should
/// never be reached in fail-closed tests) and a supplied mandatory guardrail ref.
fn build_runtime_with_mandatory_guardrail(mandatory_cap_id: &str) -> (AgentRuntime, TenantContext) {
    let mandatory = vec![GuardrailRef {
        cap_id: mandatory_cap_id.to_string(),
        offer_id: None,
        config: serde_json::Value::Null,
    }];

    let llm_script = vec![Ok(LlmResponse {
        content: Some("should never arrive".into()),
        tool_calls: vec![],
        tokens_in: 1,
        tokens_out: 1,
    })];

    let config = AgentConfig {
        agent_id: "a".into(),
        system_prompt: "test".into(),
        tools: vec![],
        guardrails: vec![],
        llm: LlmProviderRef {
            provider: "mock".into(),
            model: "m".into(),
        },
        limits: AgentLimits {
            max_iter: 4,
            timeout: Duration::from_secs(60),
            ..AgentLimits::default()
        },
    };

    let tc = TenantContext::new("acme", "prod");
    let cp = MockConfigProvider::new();
    cp.insert(&tc, "a", config);

    // `ExtensionRuntime::for_test()` initialises an empty capability registry —
    // any mandatory cap_id will fail to resolve, triggering the fail-closed path.
    let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());

    let runtime = AgentRuntime::new(
        Arc::new(cp),
        Arc::new(MockAgentStateStore::new()),
        ext,
        Arc::new(MockLlmBackend::new(llm_script)),
        Arc::new(MockTelemetry::new()),
        Arc::new(MockTokenMeter::new(0)),
        Arc::new(NoopToolLedger),
        None,
    )
    .with_guardrails(
        Arc::new(StaticGuardrailPolicy(mandatory)),
        // AcceptAllEvaluator would never be reached because assemble_chain
        // errors out before run_chain is called.
        Arc::new(greentic_aw_runtime::guardrail::AcceptAllEvaluator),
    );

    (runtime, tc)
}

/// When a mandatory guardrail cap_id has no offering in the (empty)
/// `ExtensionRuntime::for_test()` registry, `assemble_chain` returns `Err` and
/// `run_step` must fail closed with `AgentError::GuardrailDenied { direction:
/// Inbound, code: "internal", .. }` *before* the LLM is invoked.
#[tokio::test]
async fn fail_closed_mandatory_unresolved_returns_guardrail_denied() {
    // Use the namespace:path format required by `CapabilityId::from_str`.
    let (runtime, tc) = build_runtime_with_mandatory_guardrail("greentic:guardrail/required-pii");

    let result = runtime
        .step(
            tc,
            "session-guardrail-1",
            "a",
            AgentInput {
                text: "hello — please process this".into(),
            },
        )
        .await;

    match result {
        Err(AgentError::GuardrailDenied {
            direction,
            code,
            message,
            ..
        }) => {
            assert_eq!(
                direction,
                GuardrailDirection::Inbound,
                "fail-closed error must carry Inbound direction"
            );
            assert_eq!(
                code, "internal",
                "fail-closed error must use the 'internal' code"
            );
            assert!(
                !message.is_empty(),
                "fail-closed error must carry a non-empty message"
            );
        }
        other => panic!("expected AgentError::GuardrailDenied (inbound/internal), got: {other:?}"),
    }
}

/// Regression guard: when there are NO mandatory guardrails (default policy) and
/// the agent config has no guardrail refs, `run_step` must still succeed and
/// return the LLM reply unchanged.
#[tokio::test]
async fn no_mandatory_guardrails_passes_through() {
    let config = AgentConfig {
        agent_id: "a".into(),
        system_prompt: "test".into(),
        tools: vec![],
        guardrails: vec![],
        llm: LlmProviderRef {
            provider: "mock".into(),
            model: "m".into(),
        },
        limits: AgentLimits {
            max_iter: 4,
            timeout: Duration::from_secs(60),
            ..AgentLimits::default()
        },
    };

    let tc = TenantContext::new("acme", "prod");
    let cp = MockConfigProvider::new();
    cp.insert(&tc, "a", config);

    let runtime = AgentRuntime::new(
        Arc::new(cp),
        Arc::new(MockAgentStateStore::new()),
        Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test()),
        Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("all good".into()),
            tool_calls: vec![],
            tokens_in: 3,
            tokens_out: 3,
        })])),
        Arc::new(MockTelemetry::new()),
        Arc::new(MockTokenMeter::new(0)),
        Arc::new(NoopToolLedger),
        None,
    );
    // Uses the default NoMandatoryGuardrails + AcceptAllEvaluator — no
    // injection needed.

    let output = runtime
        .step(
            tc,
            "session-guardrail-2",
            "a",
            AgentInput { text: "hi".into() },
        )
        .await
        .expect("no guardrails configured — step must succeed");

    assert_eq!(output.reply, "all good");
}

/// When a mandatory guardrail cap_id has no offering in the (empty)
/// `ExtensionRuntime::for_test()` registry, `assemble_chain` returns `Err`
/// and `run_step` must fail closed with `AgentError::GuardrailDenied {
/// direction: Inbound, code: "internal", .. }` regardless of which evaluator
/// is supplied — the evaluator is never reached because `assemble_chain` errors
/// out before `run_chain` is called.
///
/// Note: the genuine evaluator-deny-with-resolved-chain path is covered by the
/// Task 10 e2e (real PII component) and the `run_chain` unit tests in
/// `guardrail.rs`. Building a resolved-chain integration test here is deferred
/// to Task 10 because it requires the ExtensionRuntime injection API from
/// Task 7.
#[tokio::test]
async fn mandatory_ref_with_empty_registry_fails_closed() {
    // Use the namespace:path format required by `CapabilityId::from_str`.
    let (runtime, tc) = build_runtime_with_mandatory_guardrail("greentic:guardrail/pii");

    let result = runtime
        .step(
            tc,
            "session-guardrail-3",
            "a",
            AgentInput {
                text: "sensitive input".into(),
            },
        )
        .await;

    match result {
        Err(AgentError::GuardrailDenied {
            direction,
            code,
            message,
            ..
        }) => {
            assert_eq!(
                direction,
                GuardrailDirection::Inbound,
                "fail-closed error must carry Inbound direction"
            );
            assert_eq!(
                code, "internal",
                "fail-closed error must use the 'internal' code"
            );
            assert!(
                !message.is_empty(),
                "fail-closed error must carry a non-empty message"
            );
        }
        other => panic!("expected AgentError::GuardrailDenied (inbound/internal), got: {other:?}"),
    }
}
