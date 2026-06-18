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
    let mut user_message = message.text.clone();
    state.messages.push(ChatMessage::User {
        content: message.text,
    });

    // --- INPUT guardrail (spec §4.2): scan the user message before it reaches
    // the LLM. Block short-circuits the step; Mask rewrites the persisted text
    // so PII does not re-enter context next turn. `user_message` is updated so
    // recall/ingest use the masked text too.
    if let Some(g) = &runtime.guardrail {
        match crate::guardrail::guard_incoming(
            g.guardrail.as_ref(),
            crate::guardrail::GuardrailStage::Input,
            &user_message,
            g.fail_closed_ingress,
            &g.block_message,
        )
        .await
        {
            crate::guardrail::IncomingDecision::Allow => {}
            crate::guardrail::IncomingDecision::Mask { text } => {
                if let Some(ChatMessage::User { content }) = state.messages.last_mut() {
                    *content = text.clone();
                }
                user_message = text;
            }
            crate::guardrail::IncomingDecision::Block { message } => {
                state.messages.push(ChatMessage::Assistant {
                    content: message.clone(),
                    tool_calls: vec![],
                });
                state.truncate_history(config.limits.max_history_turns);
                if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
                    warn!(error = %e, "state save failed after input block");
                }
                runtime.telemetry.record_step(&StepTelemetryCtx {
                    tenant_id: tenant.tenant_id.clone(),
                    env_id: tenant.env_id.clone(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    terminated_by: TerminationReason::FinalReply,
                    iterations: 0,
                    total_tokens: 0,
                    duration: started.elapsed(),
                });
                return Ok(AgentOutput {
                    reply: message.clone(),
                    trail: vec![AgentStep::Reply { text: message }],
                    terminated_by: TerminationReason::FinalReply,
                });
            }
        }
    }

    // Whether long-term memory is active for this turn (provider wired + the
    // agent's binding enabled). Drives recall-inject, the `recall_memory` tool,
    // and background ingest below.
    let lt_active = crate::long_term::long_term_active(runtime.long_term_memory.is_some(), &config);

    // --- Long-term recall: inject relevant facts into this turn's prompt ---
    let system_prompt = if lt_active {
        let facts = runtime
            .recall_long_term(
                &tenant,
                crate::long_term::RecallQuery {
                    query: user_message.clone(),
                    limit: Some(crate::long_term::AUTO_INJECT_K),
                },
            )
            .await
            .unwrap_or_default();
        crate::long_term::augment_system_prompt(&config.system_prompt, &facts)
    } else {
        config.system_prompt.clone()
    };

    // --- Knowledge retrieval: inject relevant corpus chunks (RAG-as-context,
    // D3). Distinct seam from long-term memory (`cap://dw.knowledge`); both may
    // augment the same turn, the knowledge block appended after the LT facts.
    // Retrieval failures degrade to no injection rather than failing the turn.
    let kn_active = crate::knowledge::knowledge_active(runtime.knowledge.is_some(), &config);
    let system_prompt = if kn_active {
        let chunks = runtime
            .search_knowledge(
                &tenant,
                crate::knowledge::KnowledgeQuery {
                    query: user_message.clone(),
                    limit: Some(crate::knowledge::auto_top_k(&config)),
                },
            )
            .await
            .unwrap_or_default();
        crate::knowledge::augment_system_prompt(&system_prompt, &chunks)
    } else {
        system_prompt
    };

    // Resolve the per-tenant agentic-worker MCP catalog once per step. The
    // source is infallible (degrades to an empty catalog on any admin/server
    // failure) and TTL-cached, so a stable config does not re-hit the network
    // across iterations. `None` source → no MCP tools at all.
    let mcp_catalog = match runtime.mcp.as_ref() {
        Some(src) => Some(src.catalog(&tenant).await),
        None => None,
    };

    // Resolve the per-tenant component tool catalog once per step (mirrors the
    // MCP catalog above). Infallible + TTL-cached; `None` source → no
    // `component:` tools at all.
    let component_catalog = match runtime.components.as_ref() {
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

        let mut tools_schema = list_tools_for_llm(
            &runtime.ext_runtime,
            mcp_catalog.as_deref(),
            component_catalog.as_deref(),
            &config.tools,
        );
        if lt_active {
            tools_schema.push(crate::long_term::recall_memory_tool_schema());
        }
        let request = LlmRequest {
            system_prompt: system_prompt.clone(),
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
                // --- Host built-in: `recall_memory` (long-term lookup) ---
                // Intercepted before the allow-list + WASM dispatch; routed to
                // the runtime's long-term backend instead of an extension.
                if lt_active && call.tool_name == crate::long_term::RECALL_MEMORY_TOOL {
                    observer.on_tool_call(&call.tool_name, &call.call_id);
                    let result = host_recall_memory(runtime, &tenant, &call).await;
                    observer.on_tool_result(&call.tool_name, &call.call_id, &result);
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: result.clone(),
                    });
                    trail.push(AgentStep::ToolCall {
                        name: call.tool_name.clone(),
                        call_id: call.call_id,
                        result,
                    });
                    continue;
                }
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
                    component_catalog.clone(),
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

                // --- Tool-result guardrail (spec §4.2): the external tool
                // output is the top prompt-injection / PII vector. Guard it
                // before it is observed, recorded, or appended to history.
                let result = if let Some(g) = &runtime.guardrail {
                    let text = result.to_string();
                    match crate::guardrail::guard_incoming(
                        g.guardrail.as_ref(),
                        crate::guardrail::GuardrailStage::Input,
                        &text,
                        g.fail_closed_ingress,
                        &g.tool_block_placeholder,
                    )
                    .await
                    {
                        crate::guardrail::IncomingDecision::Allow => result,
                        crate::guardrail::IncomingDecision::Block { .. } => {
                            serde_json::json!({ "error": "blocked by guardrail policy, result withheld" })
                        }
                        crate::guardrail::IncomingDecision::Mask { text } => {
                            serde_json::Value::String(text)
                        }
                    }
                } else {
                    result
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
        state.messages.push(ChatMessage::Assistant {
            content: reply.clone(),
            tool_calls: vec![],
        });
        trail.push(AgentStep::Reply {
            text: reply.clone(),
        });
        terminated_by = TerminationReason::FinalReply;
        break;
    }

    state.truncate_history(config.limits.max_history_turns);
    if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
        warn!(error = %e, "state save failed at end of step");
    }

    // --- Long-term ingest: persist this turn as an episode (fire-and-forget) ---
    if !reply.is_empty()
        && lt_active
        && let Some(memory) = runtime.long_term_memory.clone()
    {
        match crate::long_term::to_types_tenant(&tenant) {
            Ok(ctx) => {
                let episode = crate::long_term::EpisodeIngest {
                    name: format!("{session_id}:turn"),
                    body: format!("{user_message}\n\n{reply}"),
                    source: crate::long_term::EpisodeSource::Message,
                    source_description: Some("agentic-worker turn".into()),
                    reference_time: chrono::Utc::now(),
                };
                tokio::spawn(async move {
                    if let Err(e) = memory.ingest_episode(&ctx, episode).await {
                        warn!(error = %e, "background long-term ingest failed");
                    }
                });
            }
            Err(e) => {
                warn!(error = %e, "long-term ingest skipped: tenant conversion failed");
            }
        }
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

/// Handle a host built-in `recall_memory` tool call: parse `query`/`limit` from
/// the call args, query long-term memory, and return the facts as JSON (or an
/// error object the LLM can observe and react to).
async fn host_recall_memory(
    runtime: &AgentRuntime,
    tenant: &TenantContext,
    call: &crate::state::ToolCallRecord,
) -> serde_json::Value {
    let query = call
        .args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let limit = call
        .args
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(crate::long_term::TOOL_LIMIT);
    match runtime
        .recall_long_term(
            tenant,
            crate::long_term::RecallQuery {
                query,
                limit: Some(limit),
            },
        )
        .await
    {
        Ok(facts) => serde_json::json!({ "facts": facts }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
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
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "m".into(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
            memory: None,
            knowledge: None,
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

    /// 4b: when a knowledge backend is wired and the agent's binding is enabled,
    /// retrieved chunks are injected into the system prompt the LLM sees.
    #[tokio::test]
    async fn knowledge_chunks_inject_into_system_prompt() {
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            tokens_in: 1,
            tokens_out: 1,
        })]));
        let store = Arc::new(MockAgentStateStore::new());
        let telemetry = Arc::new(MockTelemetry::new());
        let cp = MockConfigProvider::new();
        let tc = TenantContext::new("acme", "prod");

        // Config with an enabled knowledge binding (top_k = 3).
        let mut c = cfg();
        c.knowledge = Some(crate::config::KnowledgeSettings {
            knowledge: Some(crate::config::MemoryProviderRef {
                provider: "provider.knowledge.chronicle".into(),
                capability: "cap://dw.knowledge".into(),
                params: serde_json::Map::new(),
                credential_ref: None,
            }),
            embedding: None,
            top_k: 3,
        });
        cp.insert(&tc, "a", c);
        let cp = Arc::new(cp);

        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let token_meter = Arc::new(crate::cost::MockTokenMeter::new(0));
        let ledger = Arc::new(crate::mock::NoopToolLedger);
        let kb = Arc::new(crate::mock::MockKnowledge::new(vec![
            crate::knowledge::RetrievedChunk {
                text: "Refunds are processed within 5 business days.".into(),
                score: 0.9,
                doc_id: None,
                chunk_index: None,
                metadata: serde_json::Map::new(),
            },
        ]));
        let runtime = AgentRuntime::new(
            cp,
            store,
            ext,
            llm.clone(),
            telemetry,
            token_meter,
            ledger,
            None,
        )
        .with_knowledge(kb);

        runtime
            .step(
                tc,
                "sess-k",
                "a",
                AgentInput {
                    text: "do I get refunds?".into(),
                },
            )
            .await
            .unwrap();

        let prompts = llm.seen_system_prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains("<knowledge>"),
            "knowledge block missing from system prompt: {}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("Refunds are processed within 5 business days."),
            "retrieved chunk missing from system prompt: {}",
            prompts[0]
        );
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

    struct BlockAll;
    impl crate::guardrail::Guardrail for BlockAll {
        fn check<'a>(&'a self, _s: crate::guardrail::GuardrailStage, _t: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<crate::guardrail::GuardrailVerdict, crate::guardrail::GuardrailError>> + Send + 'a>> {
            Box::pin(async {
                Ok(crate::guardrail::GuardrailVerdict {
                    action: crate::guardrail::GuardrailAction::Block { message: "policy says no".into() },
                    assessments: serde_json::Value::Null,
                })
            })
        }
    }

    #[tokio::test]
    async fn input_block_short_circuits_step() {
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("should never be returned".into()),
            tool_calls: vec![],
            tokens_in: 1,
            tokens_out: 1,
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
        let runtime = AgentRuntime::new(cp, store, ext, llm.clone(), telemetry, token_meter, ledger, None)
            .with_guardrail(crate::guardrail::GuardrailRuntimeConfig {
                guardrail: Arc::new(BlockAll),
                fail_closed_ingress: true,
                block_message: "policy says no".into(),
                tool_block_placeholder: "withheld".into(),
            });

        let out = runtime
            .step(tc, "sess-block", "a", AgentInput { text: "leak my SSN 123-45-6789".into() })
            .await
            .unwrap();
        assert_eq!(out.reply, "policy says no");
        // Verify LLM was never called: the seeded response must still be in the queue.
        let remaining = llm.responses.lock().expect("mock llm lock poisoned").len();
        assert_eq!(remaining, 1, "LLM must not be called when input is blocked");
    }
}
