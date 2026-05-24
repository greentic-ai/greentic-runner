//! Plan-Act-Observe agent loop. Phase 1 ships a single-iteration stub
//! (LLM call → reply). Phase 3 expands this into the full loop per
//! spec §5.3.

use std::time::{Duration, Instant};

use crate::error::{AgentError, TerminationReason};
use crate::llm::LlmRequest;
use crate::state::ChatMessage;
use crate::telemetry::StepTelemetryCtx;
use crate::tenant::TenantContext;
use crate::tools::list_tools_for_llm;
use crate::{AgentInput, AgentOutput, AgentRuntime, AgentStep};

pub async fn run_step(
    runtime: &AgentRuntime,
    tenant: TenantContext,
    session_id: &str,
    agent_id: &str,
    message: AgentInput,
) -> Result<AgentOutput, AgentError> {
    let started = Instant::now();
    let config = runtime
        .config_provider
        .agent_config(&tenant, agent_id)
        .await?;
    let _lock = runtime
        .state_store
        .acquire_lock(&tenant, session_id, Duration::from_secs(5))
        .await
        .map_err(|e| match e {
            crate::error::StateError::LockTimeout(_) => AgentError::LockTimeout,
            other => AgentError::StateLoad(other),
        })?;
    let mut state = runtime.state_store.load(&tenant, session_id).await?;
    state.messages.push(ChatMessage::User {
        content: message.text,
    });

    let request = LlmRequest {
        system_prompt: config.system_prompt.clone(),
        history: state.messages.clone(),
        tools: list_tools_for_llm(&config.tools),
        provider: config.llm.clone(),
    };
    let response = runtime.llm.complete(request).await?;
    let reply = response.content.unwrap_or_default();
    state.messages.push(ChatMessage::Assistant {
        content: reply.clone(),
        tool_calls: vec![],
    });

    state.truncate_history(config.limits.max_history_turns);
    if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
        tracing::warn!(error = %e, "state save failed at end of stub step");
    }

    let trail = vec![AgentStep::Reply {
        text: reply.clone(),
    }];
    // tokens_in and tokens_out are u32; widening to u64 is lossless.
    #[allow(clippy::cast_lossless)]
    let total_tokens = (response.tokens_in as u64) + (response.tokens_out as u64);
    runtime.telemetry.record_step(&StepTelemetryCtx {
        tenant_id: tenant.tenant_id.clone(),
        env_id: tenant.env_id.clone(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        terminated_by: TerminationReason::FinalReply,
        iterations: 1,
        total_tokens,
        duration: started.elapsed(),
    });

    Ok(AgentOutput {
        reply,
        trail,
        terminated_by: TerminationReason::FinalReply,
    })
}

#[cfg(all(test, feature = "test-mock"))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};
    use crate::llm::LlmResponse;
    use crate::mock::{MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry};
    use crate::tenant::TenantContext;

    fn cfg() -> AgentConfig {
        AgentConfig {
            agent_id: "a".into(),
            system_prompt: "sys".into(),
            tools: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "m".into(),
            },
            limits: AgentLimits::default(),
        }
    }

    /// Happy-path loop test: one LLM call → reply, telemetry recorded.
    ///
    /// Marked `#[ignore]` because `greentic_ext_runtime::ExtensionRuntime::for_test()`
    /// does not yet exist in the pinned `v1.2.8-research` tag.
    /// See <https://github.com/greentic-biz/greentic-designer-extensions/issues/66>.
    /// When the upstream shim lands: remove `#[ignore]`, restore the full body
    /// from the commit message / issue comments, and delete this placeholder.
    #[tokio::test]
    #[ignore = "needs ExtensionRuntime::for_test() shim from greentic-ext-runtime — see https://github.com/greentic-biz/greentic-designer-extensions/issues/66"]
    async fn happy_path_returns_llm_reply() {
        // ExtensionRuntime::for_test() does not yet exist in v1.2.8-research.
        // The full test body (AgentRuntime::new + step + assertions) lives in
        // the issue linked in the #[ignore] attribute above. This placeholder
        // keeps the test visible in `cargo test -- --list` so Phase 3 devs
        // know it exists without needing to hunt the git log.
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("hi from llm".into()),
            tool_calls: vec![],
            tokens_in: 10,
            tokens_out: 20,
        })]));
        let store = Arc::new(MockAgentStateStore::new());
        let telemetry = Arc::new(MockTelemetry::new());
        let cp = MockConfigProvider::new();
        let tc = TenantContext::new("acme", "prod");
        cp.insert(&tc, "a", cfg());
        let cp = Arc::new(cp);
        let _ = (cp, store, llm, telemetry, tc);
    }
}
