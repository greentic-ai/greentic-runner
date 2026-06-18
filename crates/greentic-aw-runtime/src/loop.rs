//! Plan-Act-Observe agent loop. Spec §5.3.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::warn;

use crate::error::{AgentError, LlmError, TerminationReason};
use crate::llm::LlmRequest;
use crate::state::{ChatMessage, ConversationState};
use crate::telemetry::StepTelemetryCtx;
use crate::tenant::TenantContext;
use crate::tools::{dispatch_tool_call, is_tool_allowed, list_tools_for_llm};
use crate::{AgentInput, AgentOutput, AgentRuntime, AgentStep, StepObserver};

pub async fn run_step(
    runtime: &AgentRuntime,
    tenant: TenantContext,
    session_id: &str,
    agent_id: &str,
    message: AgentInput,
    observer: Arc<dyn StepObserver>,
) -> Result<AgentOutput, AgentError> {
    let started = Instant::now();
    let config = runtime
        .config_provider
        .agent_config(&tenant, agent_id)
        .await?;

    // --- Cost budget gate (spec Decision 14) ---
    if let Some(cap) = config.limits.daily_token_cap_per_tenant {
        let used = runtime.token_meter.current(&tenant).await?;
        if used >= u64::from(cap) {
            return Err(AgentError::TokenBudgetExceeded);
        }
    }

    // --- Acquire distributed lock (default wait 5s) ---
    let lock = runtime
        .state_store
        .acquire_lock(&tenant, session_id, Duration::from_secs(5))
        .await
        .map_err(|e| match e {
            crate::error::StateError::LockTimeout(_) => AgentError::LockTimeout,
            other => AgentError::StateLoad(other),
        })?;

    // --- Load state (best-effort; empty on failure) ---
    let mut state = match runtime.state_store.load(&tenant, session_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "state load failed; proceeding with empty state");
            ConversationState::empty(&tenant, session_id)
        }
    };
    // --- Assemble guardrail chain (once per step, before any message push) ---
    // Mandatory refs from the platform policy are resolved first; if any
    // mandatory guardrail cannot be resolved the agent is blocked (fail-closed).
    let guardrail_chain = {
        let registry = runtime.ext_runtime.capability_registry();
        let mandatory = runtime.guardrail_policy.mandatory_guardrails(&tenant);
        match crate::guardrail::assemble_chain(&registry, &mandatory, &config.guardrails) {
            Ok(chain) => chain,
            Err(unresolved) => {
                return Err(AgentError::GuardrailDenied {
                    direction: crate::guardrail::GuardrailDirection::Inbound,
                    code: "internal".to_string(),
                    message: "A required guardrail is unavailable.".to_string(),
                    details: serde_json::to_string(
                        &serde_json::json!({ "unresolved_mandatory": unresolved }),
                    )
                    .ok(),
                });
            }
        }
    };
    let guardrail_ctx = crate::guardrail::GuardrailRunCtx {
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        tenant_id: tenant.tenant_id.clone(),
        env_id: tenant.env_id.clone(),
    };

    // --- Inbound guardrail hook ---
    let user_text = match crate::guardrail::run_chain(
        &guardrail_chain,
        crate::guardrail::GuardrailDirection::Inbound,
        message.text,
        &guardrail_ctx,
        runtime.guardrail_evaluator.as_ref(),
    ) {
        crate::guardrail::ChainOutcome::Pass(text) => text,
        crate::guardrail::ChainOutcome::Denied { info, direction } => {
            return Err(AgentError::GuardrailDenied {
                direction,
                code: info.code,
                message: info.message,
                details: info.details,
            });
        }
    };
    state
        .messages
        .push(ChatMessage::User { content: user_text });

    // Resolve the per-tenant agentic-worker MCP catalog once per step. The
    // source is infallible (degrades to an empty catalog on any admin/server
    // failure) and TTL-cached, so a stable config does not re-hit the network
    // across iterations. `None` source → no MCP tools at all.
    let mcp_catalog = match runtime.mcp.as_ref() {
        Some(src) => Some(src.catalog(&tenant).await),
        None => None,
    };

    let mut total_tokens: u64 = 0;
    let mut trail: Vec<AgentStep> = Vec::new();
    let mut terminated_by = TerminationReason::MaxIterations;
    let mut iterations: u32 = 0;
    let mut reply = String::new();

    for iter in 0..config.limits.max_iter {
        iterations = iter + 1;

        // Extend the lock TTL each iteration; losing the extension is
        // preferable to aborting a partially-complete turn.
        if let Err(e) = lock.refresh().await {
            warn!(error = %e, "lock refresh failed; continuing");
        }

        if started.elapsed() >= config.limits.timeout {
            terminated_by = TerminationReason::Timeout;
            break;
        }

        let tools_schema =
            list_tools_for_llm(&runtime.ext_runtime, mcp_catalog.as_deref(), &config.tools);
        let request = LlmRequest {
            system_prompt: config.system_prompt.clone(),
            history: state.messages.clone(),
            tools: tools_schema,
            provider: config.llm.clone(),
        };

        // Stream only when the observer actually consumes deltas. A
        // non-streaming caller (the default `step` path, `NoopStepObserver`)
        // stays on `complete`, preserving the exact request wire shape — no
        // `stream: true` — that every pre-streaming caller relied on.
        // On a mid-stream error the provider may have billed for partial
        // tokens, but usage only arrives in the stream's final chunk; those
        // partial tokens are NOT metered here. This matches the blocking
        // path, which likewise meters nothing when complete() errors.
        let llm_result = if observer.wants_streaming() {
            let obs = observer.clone();
            let on_delta: crate::llm::OnDelta =
                Box::new(move |chunk: &str| obs.on_token_delta(chunk));
            runtime.llm.complete_streaming(request, on_delta).await
        } else {
            runtime.llm.complete(request).await
        };
        let response = match llm_result {
            Ok(r) => r,
            Err(LlmError::ServiceUnavailable) => {
                let _ = runtime.state_store.save(&tenant, session_id, &state).await;
                return Err(AgentError::LlmProviderUnavailable);
            }
            Err(other) => {
                let _ = runtime.state_store.save(&tenant, session_id, &state).await;
                return Err(AgentError::Llm(other));
            }
        };

        let step_tokens = u64::from(response.tokens_in) + u64::from(response.tokens_out);
        total_tokens += step_tokens;
        if let Err(e) = runtime.token_meter.add(&tenant, step_tokens).await {
            warn!(error = %e, "token meter add failed; continuing");
        }

        // --- Mixed text + tool_calls: tool_calls win (spec Decision 12) ---
        if !response.tool_calls.is_empty() {
            // Record the assistant's tool-call turn BEFORE the tool results.
            // OpenAI requires every `tool` message to follow an assistant
            // message carrying the matching `tool_calls`; without this the next
            // turn 400s ("messages with role 'tool' must be a response to a
            // preceeding message with 'tool_calls'").
            state.messages.push(ChatMessage::Assistant {
                content: response.content.clone().unwrap_or_default(),
                tool_calls: response.tool_calls.clone(),
            });
            for call in response.tool_calls {
                if !is_tool_allowed(&call, &config.tools) {
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: serde_json::json!({ "error": "tool not allowed for this agent" }),
                    });
                    trail.push(AgentStep::ToolCallBlocked {
                        name: call.tool_name.clone(),
                        reason: "not in allow-list".into(),
                    });
                    continue;
                }

                // --- Idempotency: reuse a previously-recorded result ---
                match runtime.ledger.get(&tenant, session_id, &call.call_id).await {
                    Ok(Some(cached)) => {
                        state.messages.push(ChatMessage::Tool {
                            call_id: call.call_id.clone(),
                            content: cached,
                        });
                        trail.push(AgentStep::ToolCallReused {
                            name: call.tool_name.clone(),
                            call_id: call.call_id.clone(),
                        });
                        continue;
                    }
                    Ok(None) => {} // fall through to dispatch
                    Err(e) => {
                        warn!(error = %e, "ledger get failed; dispatching without idempotency");
                    }
                }

                // --- Dispatch (blocking WASM via spawn_blocking) ---
                // Tool dispatch errors are NOT termination (spec §6): surface
                // the error as a Tool observation so the LLM can react, then
                // continue. Failed calls are NOT recorded in the ledger
                // (they should remain retryable on the next turn).
                observer.on_tool_call(&call.tool_name, &call.call_id);
                let result = match dispatch_tool_call(
                    runtime.ext_runtime.clone(),
                    mcp_catalog.clone(),
                    call.clone(),
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(
                            error = %e, tool = %call.tool_name,
                            "tool dispatch failed; recording as observation and continuing"
                        );
                        let err_obs = serde_json::json!({ "error": e.to_string() });
                        state.messages.push(ChatMessage::Tool {
                            call_id: call.call_id.clone(),
                            content: err_obs.clone(),
                        });
                        trail.push(AgentStep::ToolCall {
                            name: call.tool_name.clone(),
                            call_id: call.call_id.clone(),
                            result: err_obs,
                        });
                        continue;
                    }
                };

                observer.on_tool_result(&call.tool_name, &call.call_id, &result);

                // Record successful result in ledger (best-effort).
                if let Err(e) = runtime
                    .ledger
                    .record(&tenant, session_id, &call.call_id, result.clone())
                    .await
                {
                    warn!(error = %e, "ledger record failed; continuing");
                }

                state.messages.push(ChatMessage::Tool {
                    call_id: call.call_id.clone(),
                    content: result.clone(),
                });
                trail.push(AgentStep::ToolCall {
                    name: call.tool_name.clone(),
                    call_id: call.call_id,
                    result,
                });
            }
            continue; // next LLM turn with tool observations
        }

        // --- No tool calls: final reply ---
        reply = response.content.unwrap_or_default();
        // NOTE: the assistant ChatMessage push and trail push are deferred
        // until AFTER the outbound guardrail chain so history reflects the
        // guarded reply, not the raw LLM output.
        terminated_by = TerminationReason::FinalReply;
        break;
    }

    // --- Outbound guardrail hook ---
    // Run on the candidate reply before it leaves the runtime. History is
    // written only after this check so saved state reflects the guarded reply.
    let reply = match crate::guardrail::run_chain(
        &guardrail_chain,
        crate::guardrail::GuardrailDirection::Outbound,
        reply,
        &guardrail_ctx,
        runtime.guardrail_evaluator.as_ref(),
    ) {
        crate::guardrail::ChainOutcome::Pass(text) => text,
        crate::guardrail::ChainOutcome::Denied { info, direction } => {
            return Err(AgentError::GuardrailDenied {
                direction,
                code: info.code,
                message: info.message,
                details: info.details,
            });
        }
    };
    // Push the guarded reply into conversation history and the audit trail
    // now that both inbound and outbound checks have passed.
    if terminated_by == TerminationReason::FinalReply {
        state.messages.push(ChatMessage::Assistant {
            content: reply.clone(),
            tool_calls: vec![],
        });
        trail.push(AgentStep::Reply {
            text: reply.clone(),
        });
    }

    state.truncate_history(config.limits.max_history_turns);
    if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
        warn!(error = %e, "state save failed at end of step");
    }

    runtime.telemetry.record_step(&StepTelemetryCtx {
        tenant_id: tenant.tenant_id.clone(),
        env_id: tenant.env_id.clone(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        terminated_by: terminated_by.clone(),
        iterations,
        total_tokens,
        duration: started.elapsed(),
    });

    Ok(AgentOutput {
        reply,
        trail,
        terminated_by,
    })
}

#[cfg(all(test, feature = "test-mock"))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};
    use crate::llm::LlmResponse;
    use crate::mock::{MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry};
    use crate::tenant::TenantContext;
    use crate::{AgentInput, AgentRuntime};

    fn cfg() -> AgentConfig {
        AgentConfig {
            agent_id: "a".into(),
            system_prompt: "sys".into(),
            tools: vec![],
            guardrails: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "m".into(),
            },
            limits: AgentLimits::default(),
        }
    }

    /// Happy-path loop test: one LLM call → reply, telemetry recorded.
    #[tokio::test]
    async fn happy_path_returns_llm_reply() {
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

        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let token_meter = Arc::new(crate::cost::MockTokenMeter::new(0));
        let ledger = Arc::new(crate::mock::NoopToolLedger);
        let runtime = AgentRuntime::new(
            cp,
            store,
            ext,
            llm,
            telemetry.clone(),
            token_meter,
            ledger,
            None,
        );

        let out = runtime
            .step(
                tc.clone(),
                "sess-1",
                "a",
                AgentInput {
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(out.reply, "hi from llm");
        assert_eq!(telemetry.recorded.lock().unwrap().len(), 1);
    }

    #[derive(Default)]
    struct Collecting {
        deltas: std::sync::Mutex<Vec<String>>,
        tool_calls: std::sync::Mutex<Vec<String>>,
    }
    impl crate::StepObserver for Collecting {
        fn wants_streaming(&self) -> bool {
            true
        }
        fn on_token_delta(&self, chunk: &str) {
            self.deltas.lock().expect("lock").push(chunk.to_string());
        }
        fn on_tool_call(&self, name: &str, _call_id: &str) {
            self.tool_calls.lock().expect("lock").push(name.to_string());
        }
    }

    /// `step_with_observer` mirrors `happy_path_returns_llm_reply` but
    /// drives a collecting observer; the default-impl single-delta path
    /// (MockLlmBackend uses the trait default) must forward the full reply
    /// to `on_token_delta`.
    #[tokio::test]
    async fn step_with_observer_streams_reply_deltas() {
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

        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let token_meter = Arc::new(crate::cost::MockTokenMeter::new(0));
        let ledger = Arc::new(crate::mock::NoopToolLedger);
        let runtime = AgentRuntime::new(
            cp,
            store,
            ext,
            llm,
            telemetry.clone(),
            token_meter,
            ledger,
            None,
        );

        let obs = Arc::new(Collecting::default());
        let out = runtime
            .step_with_observer(
                tc.clone(),
                "sess-2",
                "a",
                AgentInput {
                    text: "hello".into(),
                },
                obs.clone(),
            )
            .await
            .unwrap();
        assert_eq!(out.reply, "hi from llm");
        assert_eq!(*obs.deltas.lock().unwrap(), vec!["hi from llm".to_string()]);
    }

    /// A non-streaming observer (default `wants_streaming() == false`) must
    /// keep `step` on the `complete` path: no token deltas are produced, and
    /// the reply is still returned. This is the regression guard for callers
    /// (and mocks) that speak the non-streaming OpenAI wire shape — turning
    /// streaming on unconditionally would send `stream: true` and break them.
    #[tokio::test]
    async fn non_streaming_observer_emits_no_deltas() {
        #[derive(Default)]
        struct CountOnly {
            deltas: std::sync::Mutex<u32>,
        }
        impl crate::StepObserver for CountOnly {
            fn on_token_delta(&self, _chunk: &str) {
                *self.deltas.lock().expect("lock") += 1;
            }
        }

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
        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let token_meter = Arc::new(crate::cost::MockTokenMeter::new(0));
        let ledger = Arc::new(crate::mock::NoopToolLedger);
        let runtime = AgentRuntime::new(cp, store, ext, llm, telemetry, token_meter, ledger, None);

        let obs = Arc::new(CountOnly::default());
        let out = runtime
            .step_with_observer(
                tc.clone(),
                "sess-3",
                "a",
                AgentInput {
                    text: "hello".into(),
                },
                obs.clone(),
            )
            .await
            .unwrap();
        assert_eq!(out.reply, "hi from llm");
        assert_eq!(
            *obs.deltas.lock().unwrap(),
            0,
            "no deltas on the non-streaming path"
        );
    }
}
