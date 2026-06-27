use anyhow::Result;
use serde_json::Value;

/// Bridges a `DwAgentGraph` flow node into the graph executor.
///
/// The concrete impl (constructed in the runner binary / pack loader, Task 8)
/// wraps [`greentic_aw_runtime::graph::GraphExecutor`]. The engine holds it as
/// a trait object so `engine.rs` stays free of AW-runtime construction details
/// — mirroring [`super::agent_node::AgentNodeHandler`].
#[async_trait::async_trait]
pub trait GraphNodeHandler: Send + Sync {
    /// Execute (or resume) a graph run for this session. `flow_input`
    /// expects `{"user_text": "..."}`; returns
    /// `{"reply", "trail", "terminated_by"}` — the same envelope as DwAgent.
    async fn execute(
        &self,
        tenant_id: &str,
        env_id: &str,
        graph_id: &str,
        session_id: &str,
        flow_input: &Value,
    ) -> Result<Value>;
}

// ---------------------------------------------------------------------------
// agentic-worker feature: full DwAgentGraph / GraphExecutor integration
// ---------------------------------------------------------------------------

#[cfg(feature = "agentic-worker")]
mod aw {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::Result;
    use greentic_aw_runtime::CachingGraphProvider;
    use greentic_aw_runtime::HttpGraphProvider;
    use greentic_aw_runtime::ToolLedger;
    use greentic_aw_runtime::config_provider::InMemoryConfigProvider;
    use greentic_aw_runtime::error::ConfigError;
    use greentic_aw_runtime::graph::{
        AgentTurnFn, AgentTurnRequest, AgentTurnResult, BoxFut, CheckpointError, CheckpointStore,
        GraphConfig, GraphExecError, GraphExecutor, GraphRole, GraphRunState, RunStatus,
        SupervisorFn, SupervisorRequest, SupervisorResult, ToolCallRequest, ToolFn,
    };
    use greentic_aw_runtime::state::{AgentStateStore, ChatMessage, ConversationState};
    use greentic_aw_runtime::tools::dispatch_tool_call;
    use greentic_aw_runtime::{
        AgentConfig, AgentInput, AgentLimits, AgentRuntime, LlmBackend, LlmProviderRef, Telemetry,
        TenantContext, TokenMeter,
    };
    use greentic_ext_runtime::ExtensionRuntime;
    use serde_json::{Value, json};

    use super::GraphNodeHandler;

    /// Fixed, user-safe reply returned when a graph run fails. The detailed
    /// error is logged but never surfaced to the flow output, so internal
    /// failure modes do not leak to end users. Mirrors
    /// [`super::super::agent_node`]'s `SANITISED_ERROR_REPLY`.
    const SANITISED_ERROR_REPLY: &str = "Something went wrong. Please try again.";

    /// Reply returned when the requested graph is not available (no such graph
    /// in the provider). User-safe; the missing-graph id is logged separately.
    const GRAPH_UNAVAILABLE_REPLY: &str =
        "This worker isn't available right now. Please try again later.";

    /// Resolution sentinel emitted by the agent's reply when it considers the
    /// issue fully resolved. Matched case-insensitively and stripped from the
    /// visible reply. Copied verbatim from the greentic-designer spike
    /// (`src/orchestrate/agent_graph/agent_turn.rs`).
    const RESOLVED_SENTINEL: &str = "[[RESOLVED]]";

    /// Upper bound on the number of fresh-run-id suffixes tried when a session's
    /// run id is already in a terminal state (see [`derive_run_id`]).
    const MAX_RUN_ID_SUFFIX: u32 = 100;

    /// Error returned by [`RuntimeGraphNodeHandler::derive_run_id`].
    ///
    /// Kept local to `runner-host`: `GraphExecError` (from `greentic-aw-runtime`)
    /// has no suffix-exhaustion variant and we should not pollute the upstream
    /// crate with a runner-host-specific concern.
    #[derive(Debug, thiserror::Error)]
    enum RunIdError {
        /// Checkpoint IO failed while scanning for a free slot.
        ///
        /// Note: `checkpoint.load` returns `CheckpointError` directly (not
        /// `GraphExecError`), so we convert both via their respective `From`
        /// impls. `GraphExecError::Checkpoint` wraps `CheckpointError` upstream;
        /// here we keep the original for a tighter error surface.
        #[error(transparent)]
        Checkpoint(#[from] CheckpointError),
        /// All `MAX_RUN_ID_SUFFIX` slots for this session are occupied (terminal).
        /// Distinct from `GraphExecError::IterationCap` (graph visit limit) so
        /// the caller can surface a targeted user reply.
        #[error("all run-id slots exhausted for base run `{base_run_id}`")]
        SuffixExhausted { base_run_id: String },
    }

    /// `true` when `reply` contains the resolution sentinel (case-insensitive).
    fn detect_resolved(reply: &str) -> bool {
        reply
            .to_ascii_lowercase()
            .contains(&RESOLVED_SENTINEL.to_ascii_lowercase())
    }

    /// Strip the sentinel from `reply` (first case-insensitive occurrence) and
    /// trim surrounding whitespace so the user-visible text is clean.
    fn strip_sentinel(reply: &str) -> String {
        let lower = reply.to_ascii_lowercase();
        if let Some(idx) = lower.find(&RESOLVED_SENTINEL.to_ascii_lowercase()) {
            let mut out = reply.to_string();
            out.replace_range(idx..idx + RESOLVED_SENTINEL.len(), "");
            return out.trim().to_string();
        }
        reply.trim().to_string()
    }

    // -----------------------------------------------------------------------
    // GraphConfigSource
    // -----------------------------------------------------------------------

    /// Resolves a [`GraphConfig`] for a `(tenant, graph_id)` pair.
    ///
    /// Object-safe (returns [`BoxFut`]) so it can be held as
    /// `Arc<dyn GraphConfigSource>`. The pack loader (Task 8) fills the
    /// concrete implementation; tests use [`InMemoryGraphProvider`].
    pub trait GraphConfigSource: Send + Sync {
        fn graph_config<'a>(
            &'a self,
            tenant: &'a TenantContext,
            graph_id: &'a str,
        ) -> BoxFut<'a, Result<GraphConfig, ConfigError>>;
    }

    /// [`GraphConfigSource`] backed by an in-memory `graph_id -> GraphConfig`
    /// map. Graphs are operator-global for the MVP: lookup is keyed purely by
    /// `graph_id`; the `tenant` argument is accepted (to satisfy the trait
    /// contract) but not used for keying. Reuses [`ConfigError::AgentNotFound`]
    /// as the crate's canonical "not found" variant.
    pub struct InMemoryGraphProvider {
        graphs: HashMap<String, GraphConfig>,
    }

    impl InMemoryGraphProvider {
        /// Wrap a `graph_id -> GraphConfig` map in a [`GraphConfigSource`].
        pub fn new(graphs: HashMap<String, GraphConfig>) -> Self {
            Self { graphs }
        }
    }

    impl GraphConfigSource for InMemoryGraphProvider {
        fn graph_config<'a>(
            &'a self,
            _tenant: &'a TenantContext,
            graph_id: &'a str,
        ) -> BoxFut<'a, Result<GraphConfig, ConfigError>> {
            let found = self.graphs.get(graph_id).cloned();
            let graph_id_owned = graph_id.to_string();
            Box::pin(async move { found.ok_or(ConfigError::AgentNotFound(graph_id_owned)) })
        }
    }

    // -----------------------------------------------------------------------
    // GraphConfigSource adapter for CachingGraphProvider<HttpGraphProvider>
    // -----------------------------------------------------------------------

    /// Adapts [`CachingGraphProvider<HttpGraphProvider>`] into the
    /// [`GraphConfigSource`] trait so it can be stored as an
    /// `Arc<dyn GraphConfigSource>` alongside [`InMemoryGraphProvider`].
    ///
    /// This is a 5-line adapter that bridges the inherent-method API of
    /// `CachingGraphProvider` (defined in `greentic-aw-runtime`) into the
    /// trait object used by `runner-host`.
    impl GraphConfigSource for CachingGraphProvider<HttpGraphProvider> {
        fn graph_config<'a>(
            &'a self,
            tenant: &'a TenantContext,
            graph_id: &'a str,
        ) -> BoxFut<'a, Result<GraphConfig, ConfigError>> {
            Box::pin(async move { self.graph_config(tenant, graph_id).await })
        }
    }

    // -----------------------------------------------------------------------
    // LayeredGraphProvider
    // -----------------------------------------------------------------------

    /// Tries the primary [`GraphConfigSource`], falling back to the secondary
    /// when the primary reports the graph missing or has an infrastructure
    /// failure. A `Misconfigured` error is propagated, never masked by the
    /// fallback — matching [`greentic_aw_runtime::LayeredConfigProvider`]'s
    /// semantics exactly.
    ///
    /// Fallback triggers:
    /// - `ConfigError::AgentNotFound` — graph absent from the primary (HTTP
    ///   registry doesn't know it yet; local pack may have it).
    /// - `ConfigError::Internal` — transient infrastructure failure (network
    ///   timeout, 5xx) — local pack can serve as a degraded fallback.
    ///
    /// Non-fallback (surfaces immediately):
    /// - `ConfigError::Misconfigured` — corrupt document or bad auth token;
    ///   the operator must fix the document — silently serving stale local
    ///   data would mask the problem.
    pub struct LayeredGraphProvider<P: GraphConfigSource, F: GraphConfigSource> {
        primary: P,
        fallback: F,
    }

    impl<P: GraphConfigSource, F: GraphConfigSource> LayeredGraphProvider<P, F> {
        pub fn new(primary: P, fallback: F) -> Self {
            Self { primary, fallback }
        }
    }

    impl<P: GraphConfigSource, F: GraphConfigSource> GraphConfigSource for LayeredGraphProvider<P, F> {
        fn graph_config<'a>(
            &'a self,
            tenant: &'a TenantContext,
            graph_id: &'a str,
        ) -> BoxFut<'a, Result<GraphConfig, ConfigError>> {
            Box::pin(async move {
                match self.primary.graph_config(tenant, graph_id).await {
                    Ok(cfg) => Ok(cfg),
                    Err(ConfigError::AgentNotFound(_)) | Err(ConfigError::Internal(_)) => {
                        self.fallback.graph_config(tenant, graph_id).await
                    }
                    // Misconfigured + any future ConfigError variant: surface,
                    // never mask. New variants fall through here and propagate
                    // by default; revisit only if a new variant should fall back.
                    Err(other) => Err(other),
                }
            })
        }
    }

    // -----------------------------------------------------------------------
    // Admin graph registry wiring
    // -----------------------------------------------------------------------

    /// Build a cached [`HttpGraphProvider`] from `GREENTIC_AW_ADMIN_ENDPOINT` +
    /// `GREENTIC_AW_ADMIN_TOKEN`. Returns `None` when either is unset/empty,
    /// so the runtime keeps using the local pack provider alone.
    ///
    /// Mirrors [`super::agent_node::registry_from_env`] exactly — same env
    /// vars, same "both must be present" semantics.
    fn graph_registry_from_env() -> Option<CachingGraphProvider<HttpGraphProvider>> {
        let endpoint = std::env::var("GREENTIC_AW_ADMIN_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("GREENTIC_AW_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(CachingGraphProvider::new(HttpGraphProvider::new(
            endpoint, token,
        )))
    }

    // -----------------------------------------------------------------------
    // Pack-sidecar graph loader
    // -----------------------------------------------------------------------

    /// Parse an `agent-graph.json` sidecar (raw bytes from a `.gtpack`) into a
    /// validated [`GraphConfig`].
    ///
    /// Mirrors the lenient posture of
    /// [`super::super::agent_node::agent_configs_from_manifest`]: every failure
    /// mode — invalid UTF-8, malformed JSON, schema-version mismatch, or graph
    /// validation error — is logged via [`tracing::warn!`] (with `pack_id`) and
    /// yields `None`, so a single bad graph never prevents the rest of the pack
    /// from loading. The `pack_id` argument is used only in log messages.
    pub fn graph_config_from_sidecar(pack_id: &str, bytes: &[u8]) -> Option<GraphConfig> {
        let raw = match std::str::from_utf8(bytes) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    pack_id,
                    error = %error,
                    "skipping agent-graph sidecar: not valid UTF-8"
                );
                return None;
            }
        };
        match GraphConfig::from_json(raw) {
            Ok(config) => Some(config),
            Err(error) => {
                tracing::warn!(
                    pack_id,
                    error = %error,
                    "skipping malformed agent-graph sidecar in pack"
                );
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Production graph-handler construction
    // -----------------------------------------------------------------------

    /// Build the production `DwAgentGraph` handler if the environment is
    /// configured. Mirrors
    /// [`super::super::agent_node::build_agent_node_handler`]: it returns `None`
    /// (so `DwAgentGraph` flow dispatch errors clearly) under any of these
    /// graceful-degradation conditions:
    /// - `graphs` is empty (no graphs from any pack sidecar);
    /// - `GREENTIC_AW_REDIS_URL` is unset/empty;
    /// - the AW Redis connection fails;
    /// - the extension runtime fails to initialise.
    ///
    /// The shared runtime Arcs (state store, ext runtime, LLM backend,
    /// telemetry, token meter, ledger) are built exactly as for the single-agent
    /// path; the durable checkpoint store reuses the same multiplexed Redis
    /// connection manager.
    pub async fn build_graph_node_handler(
        graphs: HashMap<String, GraphConfig>,
    ) -> Option<Arc<dyn GraphNodeHandler>> {
        use greentic_aw_runtime::cost::RedisTokenMeter;
        use greentic_aw_runtime::graph::RedisCheckpointStore;
        use greentic_aw_runtime::tools::RedisToolLedger;
        use greentic_aw_runtime::{OtelTelemetry, RedisAgentStateStore};

        if graphs.is_empty() {
            return None; // nothing to serve
        }

        let redis_url = match std::env::var("GREENTIC_AW_REDIS_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                tracing::info!("GREENTIC_AW_REDIS_URL unset; DwAgentGraph nodes disabled");
                return None;
            }
        };

        let state_store = match RedisAgentStateStore::connect(&redis_url).await {
            Ok(store) => Arc::new(store),
            Err(error) => {
                tracing::warn!(error = %error, "AW Redis connect failed; DwAgentGraph nodes disabled");
                return None;
            }
        };

        // Share the multiplexed connection manager across all Redis-backed
        // stores (state, checkpoint, token meter, idempotency ledger).
        let manager = state_store.manager();
        let checkpoint = Arc::new(RedisCheckpointStore::new(manager.clone()));
        let token_meter = Arc::new(RedisTokenMeter::new(manager.clone()));
        let ledger = Arc::new(RedisToolLedger::new(manager));

        // Process-level graph serve path has no per-tenant secrets context;
        // tool secrets resolve from the env only.
        let ext_runtime = super::super::agent_node::build_ext_runtime(std::sync::Arc::new(
            super::super::agent_node::EnvSecretsBackend,
        ))?;
        let llm = super::super::agent_node::build_llm_backend(&ext_runtime);
        let telemetry = Arc::new(OtelTelemetry);

        let graph_count = graphs.len();
        // Build the graph config source: HTTP registry (cached) primary →
        // InMemoryGraphProvider (local packs) fallback, when the admin env
        // vars are configured; otherwise local packs only.
        //
        // Mirrors the agent-config composition in
        // `agent_node::build_agent_node_handler`:
        //   Some(http) → CachingConfigProvider(Layered(http, overlay))
        //   None       → CachingConfigProvider(overlay)
        //
        // The graph path skips the ManifestToolOverlay (no manifest overlay
        // exists for graphs) and the CachingGraphProvider is already embedded
        // inside the `CachingGraphProvider<HttpGraphProvider>` returned by
        // `graph_registry_from_env`. The local InMemoryGraphProvider needs no
        // separate cache (it is in-memory and O(1)).
        let local = InMemoryGraphProvider::new(graphs);
        let provider: Arc<dyn GraphConfigSource> = match graph_registry_from_env() {
            Some(http) => Arc::new(LayeredGraphProvider::new(http, local)),
            None => Arc::new(local),
        };

        let handler = RuntimeGraphNodeHandler::from_parts(
            provider,
            checkpoint,
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
        );

        tracing::info!(graph_count, "AW graph runtime constructed");
        Some(Arc::new(handler))
    }

    // -----------------------------------------------------------------------
    // RuntimeGraphNodeHandler
    // -----------------------------------------------------------------------

    /// Production [`GraphNodeHandler`] wrapping the durable graph executor.
    ///
    /// Holds the constituent runtime Arcs (state store, extension runtime, LLM
    /// backend, telemetry, token meter, ledger) so it can build a lightweight
    /// [`AgentRuntime`] *per agent visit* with a fresh single-entry
    /// [`InMemoryConfigProvider`] — mirroring the greentic-designer spike's
    /// per-visit runtime construction. Tool dispatch goes straight through the
    /// shared [`ExtensionRuntime`] via [`dispatch_tool_call`].
    pub struct RuntimeGraphNodeHandler {
        graphs: Arc<dyn GraphConfigSource>,
        checkpoint: Arc<dyn CheckpointStore>,
        state_store: Arc<dyn AgentStateStore>,
        agent_turn: AgentTurnFn,
        tool: ToolFn,
        supervisor: SupervisorFn,
    }

    impl RuntimeGraphNodeHandler {
        /// Build a handler from the runtime's constituent parts.
        ///
        /// **Deviation from the Task-7 `from_runtime(runtime: Arc<AgentRuntime>, …)`
        /// signature:** `AgentRuntime`'s inner Arcs (`ext_runtime`, `llm`,
        /// `telemetry`, `token_meter`, `ledger`) are `pub(crate)` and cannot be
        /// extracted from an `Arc<AgentRuntime>` outside the `greentic-aw-runtime`
        /// crate, so a per-visit runtime cannot be reconstructed from a shared
        /// `AgentRuntime`. The task explicitly allows this: "an acceptable
        /// alternative is a `from_parts(...)` constructor taking those Arcs
        /// directly." The pack loader (Task 8) already holds these Arcs when it
        /// would otherwise build an `AgentRuntime`, so it wires them here instead.
        #[allow(clippy::too_many_arguments)]
        pub fn from_parts(
            graphs: Arc<dyn GraphConfigSource>,
            checkpoint: Arc<dyn CheckpointStore>,
            state_store: Arc<dyn AgentStateStore>,
            ext_runtime: Arc<ExtensionRuntime>,
            llm: Arc<dyn LlmBackend>,
            telemetry: Arc<dyn Telemetry>,
            token_meter: Arc<dyn TokenMeter>,
            ledger: Arc<dyn ToolLedger>,
        ) -> Self {
            let agent_turn = build_agent_turn(
                state_store.clone(),
                ext_runtime.clone(),
                llm.clone(),
                telemetry.clone(),
                token_meter.clone(),
                ledger.clone(),
            );
            let tool = build_tool(ext_runtime.clone());
            let supervisor = build_supervisor(
                state_store.clone(),
                ext_runtime,
                llm,
                telemetry,
                token_meter,
                ledger,
            );
            Self {
                graphs,
                checkpoint,
                state_store,
                agent_turn,
                tool,
                supervisor,
            }
        }

        /// **Test-only** constructor that injects effect closures directly,
        /// bypassing per-visit [`AgentRuntime`] construction. Not available
        /// outside `#[cfg(test)]`; production code must use [`from_parts`].
        ///
        /// [`from_parts`]: RuntimeGraphNodeHandler::from_parts
        #[cfg(test)]
        pub(crate) fn with_effects(
            graphs: Arc<dyn GraphConfigSource>,
            checkpoint: Arc<dyn CheckpointStore>,
            state_store: Arc<dyn AgentStateStore>,
            agent_turn: AgentTurnFn,
            tool: ToolFn,
            supervisor: SupervisorFn,
        ) -> Self {
            Self {
                graphs,
                checkpoint,
                state_store,
                agent_turn,
                tool,
                supervisor,
            }
        }

        /// Resolve the run id to drive for this `(session, graph)` and whether
        /// the existing record (if any) is mid-flight.
        ///
        /// Base run id is `"{safe_session}__{graph_id}"` where `safe_session` is
        /// the incoming `session_id` with every `':'` replaced by `'_'`.
        ///
        /// **Why sanitize `':'`?**  Flow session ids frequently embed channel
        /// correlation ids that contain colons (e.g. MS Teams thread ids such as
        /// `msteams:thread:19xyz`). The checkpoint store key contract forbids
        /// `':'`, so using the raw session id causes every such graph run to fail
        /// at save-time. The replacement is stable: the same incoming
        /// `session_id` always maps to the same `safe_session`, so checkpoint
        /// resume works correctly across calls.
        ///
        /// When the base slot is already terminal, retry
        /// `"{safe_session}__{graph_id}__{n}"` for `n = 2..` until a slot is
        /// absent (→ start fresh) or Running (→ resume). Bounded at
        /// [`MAX_RUN_ID_SUFFIX`]. Exhausting all suffixes returns
        /// [`RunIdError::SuffixExhausted`] (distinct from
        /// [`GraphExecError::IterationCap`] so the caller can surface a
        /// targeted reply).
        async fn derive_run_id(
            &self,
            tenant: &TenantContext,
            graph_id: &str,
            session_id: &str,
        ) -> Result<RunSlot, RunIdError> {
            // Sanitize colons: flow session ids (e.g. Teams thread correlation
            // ids) may contain ':' which the checkpoint key contract forbids.
            let safe_session = session_id.replace(':', "_");
            let base = format!("{safe_session}__{graph_id}");
            match self.checkpoint.load(tenant, &base).await? {
                None => return Ok(RunSlot::start(base)),
                Some(rec) if rec.status == RunStatus::Running => {
                    return Ok(RunSlot::resume(base));
                }
                Some(_) => {}
            }
            for n in 2..=MAX_RUN_ID_SUFFIX {
                let candidate = format!("{safe_session}__{graph_id}__{n}");
                match self.checkpoint.load(tenant, &candidate).await? {
                    None => return Ok(RunSlot::start(candidate)),
                    Some(rec) if rec.status == RunStatus::Running => {
                        return Ok(RunSlot::resume(candidate));
                    }
                    Some(_) => continue,
                }
            }
            // All suffix slots are occupied (terminal). This is a distinct
            // error from the graph's own IterationCap: it means the session has
            // exhausted the run-id namespace, not that any single run looped.
            Err(RunIdError::SuffixExhausted { base_run_id: base })
        }
    }

    /// Outcome of [`RuntimeGraphNodeHandler::derive_run_id`]: which run id to
    /// drive and whether to start it fresh or resume an in-flight record.
    struct RunSlot {
        run_id: String,
        resume: bool,
    }

    impl RunSlot {
        fn start(run_id: String) -> Self {
            Self {
                run_id,
                resume: false,
            }
        }
        fn resume(run_id: String) -> Self {
            Self {
                run_id,
                resume: true,
            }
        }
    }

    #[async_trait::async_trait]
    impl GraphNodeHandler for RuntimeGraphNodeHandler {
        async fn execute(
            &self,
            tenant_id: &str,
            env_id: &str,
            graph_id: &str,
            session_id: &str,
            flow_input: &Value,
        ) -> Result<Value> {
            // Contract mirror of agent_node.rs: a missing/empty `user_text`
            // resolves to an empty string and the run proceeds (it never returns
            // Err for this case).
            let user_text = flow_input
                .get("user_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            let tenant = TenantContext::new(tenant_id, env_id);

            // Resolve the graph. A missing graph is a user-facing "unavailable"
            // outcome, never an Err.
            let cfg = match self.graphs.graph_config(&tenant, graph_id).await {
                Ok(cfg) => cfg,
                Err(error) => {
                    tracing::warn!(error = %error, graph_id, "graph config not found");
                    return Ok(error_envelope(GRAPH_UNAVAILABLE_REPLY));
                }
            };

            // Acquire the session lock before driving (mirrors the single-agent
            // loop's default 5s wait). Held across the drive via the guard.
            let _lock = match self
                .state_store
                .acquire_lock(&tenant, session_id, Duration::from_secs(5))
                .await
            {
                Ok(guard) => guard,
                Err(error) => {
                    tracing::warn!(error = %error, graph_id, session_id, "session lock failed");
                    return Ok(error_envelope(SANITISED_ERROR_REPLY));
                }
            };

            let slot = match self.derive_run_id(&tenant, graph_id, session_id).await {
                Ok(slot) => slot,
                Err(RunIdError::SuffixExhausted { .. }) => {
                    tracing::warn!(
                        graph_id,
                        session_id,
                        "all run-id slots exhausted for session"
                    );
                    return Ok(error_envelope(
                        "Too many runs for this session. Please start a new conversation.",
                    ));
                }
                Err(RunIdError::Checkpoint(error)) => {
                    tracing::warn!(error = %error, graph_id, session_id, "run-id derivation failed");
                    return Ok(error_envelope(SANITISED_ERROR_REPLY));
                }
            };

            let executor = GraphExecutor::new(
                self.checkpoint.clone(),
                self.agent_turn.clone(),
                self.tool.clone(),
                self.supervisor.clone(),
            );

            let result = if slot.resume {
                executor.resume(&tenant, &slot.run_id).await
            } else {
                executor
                    .start(&tenant, &slot.run_id, &cfg, &user_text)
                    .await
            };

            match result {
                Ok(outcome) => Ok(json!({
                    "reply": outcome.reply,
                    "trail": outcome.trail,
                    "terminated_by": "respond",
                })),
                Err(error) => {
                    tracing::warn!(error = %error, graph_id, session_id, "graph run failed");
                    // The DwAgent envelope only carries "respond" / "error".
                    // IterationCap (graph's own node-visit limit) gets a distinct,
                    // still user-safe reply; every other runtime failure falls back
                    // to the generic sanitised message.
                    let reply = match error {
                        GraphExecError::IterationCap { .. } => {
                            "This is taking longer than expected. Please try again."
                        }
                        _ => SANITISED_ERROR_REPLY,
                    };
                    Ok(error_envelope(reply))
                }
            }
        }
    }

    /// Build the `{"reply", "trail", "terminated_by": "error"}` envelope. The
    /// trail is empty: a failed/unavailable run surfaces no completed-node trail
    /// to the flow output (the detail is logged instead).
    fn error_envelope(reply: &str) -> Value {
        json!({
            "reply": reply,
            "trail": Vec::<Value>::new(),
            "terminated_by": "error",
        })
    }

    /// Seed a fresh [`ConversationState`] from the graph run's message log so the
    /// per-visit agent has full context (including after a checkpoint resume).
    ///
    /// Tool-role graph messages are folded in as a plain user-context line
    /// rather than a `ChatMessage::Tool` (which needs a `call_id` paired to an
    /// assistant `tool_calls[]` entry the graph state does not track) — matching
    /// the designer spike.
    fn seed_state_from_graph(
        tenant: &TenantContext,
        session_id: &str,
        state: &GraphRunState,
    ) -> ConversationState {
        let mut seeded = ConversationState::empty(tenant, session_id);
        for m in &state.messages {
            match m.role {
                GraphRole::User => seeded.messages.push(ChatMessage::User {
                    content: m.content.clone(),
                }),
                GraphRole::Assistant => seeded.messages.push(ChatMessage::Assistant {
                    content: m.content.clone(),
                    tool_calls: vec![],
                }),
                GraphRole::Tool => seeded.messages.push(ChatMessage::User {
                    content: format!("Tool result: {}", m.content),
                }),
            }
        }
        seeded
    }

    /// Build the [`AgentTurnFn`] effect closure.
    ///
    /// Each invocation constructs a lightweight [`AgentRuntime`] with a fresh
    /// single-entry [`InMemoryConfigProvider`] (agent id `"{graph_id}.{node_id}"`,
    /// the node's system prompt, no tools — the graph owns tool dispatch via the
    /// explicit Tool node) reusing the shared runtime Arcs. Conversation context
    /// is re-seeded from the durable [`GraphRunState`] on every visit; the
    /// per-visit session id is `"{session_id}__{graph_id}__{node_id}"`.
    fn build_agent_turn(
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
    ) -> AgentTurnFn {
        Arc::new(move |req: AgentTurnRequest| {
            let state_store = state_store.clone();
            let ext_runtime = ext_runtime.clone();
            let llm = llm.clone();
            let telemetry = telemetry.clone();
            let token_meter = token_meter.clone();
            let ledger = ledger.clone();
            Box::pin(async move {
                run_one_agent_turn(
                    req,
                    state_store,
                    ext_runtime,
                    llm,
                    telemetry,
                    token_meter,
                    ledger,
                )
                .await
            }) as BoxFut<'static, Result<AgentTurnResult, GraphExecError>>
        })
    }

    /// Drive one [`AgentRuntime::step`] for an agent-node visit. Derives the
    /// agent/session ids, seeds conversation context, runs the step, and maps the
    /// reply's resolution sentinel.
    #[allow(clippy::too_many_arguments)]
    async fn run_one_agent_turn(
        req: AgentTurnRequest,
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
    ) -> Result<AgentTurnResult, GraphExecError> {
        // The request carries no tenant/graph/session — they live on the
        // GraphRunState's seeded session. The executor seeds the run state with
        // the tenant-scoped messages, so we reconstruct a turn-scoped tenant +
        // session purely for the per-visit runtime. Conversation durability is
        // owned by the graph checkpoint, not this ephemeral store.
        let tenant = TenantContext::new("graph", "run");
        let session_id = format!("graph__{}", req.node_id);
        let agent_id = format!("graph.{}", req.node_id);

        let seeded = seed_state_from_graph(&tenant, &session_id, &req.state);
        if let Err(error) = state_store.save(&tenant, &session_id, &seeded).await {
            return Err(GraphExecError::AgentTurn(format!(
                "seeding conversation state: {error}"
            )));
        }

        let cfg = AgentConfig {
            agent_id: agent_id.clone(),
            system_prompt: req.system_prompt.clone(),
            tools: vec![],
            guardrails: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: req.model.clone(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
            memory: None,
            knowledge: None,
        };
        let mut provider = InMemoryConfigProvider::new();
        provider.insert(&tenant, &agent_id, cfg);

        // TODO(guardrail): inline graph-node AgentRuntime bypasses guardrails (v1 scope is dw.agent flow nodes only).
        let runtime = AgentRuntime::new(
            Arc::new(provider),
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
            None,
        );

        // The latest user turn is the most recent user message in the seeded log
        // (the executor pushes the initial user message and re-seeds it here);
        // pass an empty input so we do not duplicate it — the step appends the
        // input as a fresh user turn, so an empty string keeps the history intact.
        let input = AgentInput {
            text: String::new(),
        };

        let out = runtime
            .step(tenant.clone(), &session_id, &agent_id, input)
            .await
            .map_err(|e| GraphExecError::AgentTurn(format!("agent step failed: {e}")))?;

        Ok(AgentTurnResult {
            resolved: detect_resolved(&out.reply),
            reply: strip_sentinel(&out.reply),
        })
    }

    /// Routing sentinel that the supervisor LLM must include in its reply to
    /// select a branch. Parsed case-insensitively.
    const ROUTE_SENTINEL_PREFIX: &str = "[[ROUTE:";
    const ROUTE_SENTINEL_SUFFIX: &str = "]]";

    /// Build the [`SupervisorFn`] effect closure.
    ///
    /// Each invocation constructs a lightweight [`AgentRuntime`] (same pattern
    /// as [`build_agent_turn`]) with a system prompt that appends a route menu
    /// to the node's own `systemPrompt`. The reply is scanned for
    /// `[[ROUTE:<branch>]]`; if the sentinel is absent or the branch is unknown
    /// the executor falls back to the FIRST route with a `tracing::warn!`.
    fn build_supervisor(
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
    ) -> SupervisorFn {
        Arc::new(move |req: SupervisorRequest| {
            let state_store = state_store.clone();
            let ext_runtime = ext_runtime.clone();
            let llm = llm.clone();
            let telemetry = telemetry.clone();
            let token_meter = token_meter.clone();
            let ledger = ledger.clone();
            Box::pin(async move {
                run_one_supervisor_turn(
                    req,
                    state_store,
                    ext_runtime,
                    llm,
                    telemetry,
                    token_meter,
                    ledger,
                )
                .await
            }) as BoxFut<'static, Result<SupervisorResult, GraphExecError>>
        })
    }

    /// Drive one supervisor routing turn via [`AgentRuntime::step`].
    ///
    /// The routing prompt appends a route menu to the node's `systemPrompt`:
    ///
    /// ```text
    /// <node system_prompt>
    ///
    /// Available routes:
    /// - billing: Billing and payment questions
    /// - tech: Technical support issues
    ///
    /// Reply with [[ROUTE:<branch>]] to select a route.
    /// ```
    ///
    /// The agent reply is scanned for `[[ROUTE:<branch>]]` (case-insensitive).
    /// If the sentinel is absent or the branch does not match a declared route,
    /// the first route is used as fallback with a `tracing::warn!`.
    #[allow(clippy::too_many_arguments)]
    async fn run_one_supervisor_turn(
        req: SupervisorRequest,
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
    ) -> Result<SupervisorResult, GraphExecError> {
        let tenant = TenantContext::new("graph", "run");
        let session_id = format!("graph__{}_sup", req.node_id);
        let agent_id = format!("graph.{}.supervisor", req.node_id);

        // Build the route menu suffix.
        let mut route_menu = String::from("\n\nAvailable routes:\n");
        for r in &req.routes {
            route_menu.push_str(&format!("- {}: {}\n", r.branch, r.description));
        }
        route_menu.push_str("\nReply with [[ROUTE:<branch>]] to select a route.");

        let routing_system_prompt = format!("{}{}", req.system_prompt, route_menu);

        let seeded = seed_state_from_graph(&tenant, &session_id, &req.state);
        if let Err(error) = state_store.save(&tenant, &session_id, &seeded).await {
            return Err(GraphExecError::Supervisor(format!(
                "seeding conversation state: {error}"
            )));
        }

        let cfg = AgentConfig {
            agent_id: agent_id.clone(),
            system_prompt: routing_system_prompt,
            tools: vec![],
            guardrails: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: req.model.clone(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
            memory: None,
            knowledge: None,
        };
        let mut provider = InMemoryConfigProvider::new();
        provider.insert(&tenant, &agent_id, cfg);

        // TODO(guardrail): inline graph-node AgentRuntime bypasses guardrails (v1 scope is dw.agent flow nodes only).
        let runtime = AgentRuntime::new(
            Arc::new(provider),
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
            // Supervisor routing runs with an empty tool list; MCP tools are
            // never offered on this path.
            None,
        );

        let input = AgentInput {
            text: String::new(),
        };

        let out = runtime
            .step(tenant.clone(), &session_id, &agent_id, input)
            .await
            .map_err(|e| GraphExecError::Supervisor(format!("supervisor step failed: {e}")))?;

        // Parse [[ROUTE:<branch>]] from the reply (case-insensitive).
        // Do this BEFORE stripping so we read from the unmodified reply.
        let branch = parse_route_branch(&out.reply, &req.routes).unwrap_or_else(|| {
            let fallback = req
                .routes
                .first()
                .map(|r| r.branch.clone())
                .unwrap_or_default();
            tracing::warn!(
                node_id = req.node_id,
                reply = %out.reply,
                fallback = %fallback,
                "supervisor reply missing or unknown [[ROUTE:]] sentinel; falling back to first route"
            );
            fallback
        });

        // Strip the [[ROUTE:<branch>]] sentinel from the stored assistant message
        // so the message log does not expose routing instructions to downstream nodes.
        let raw_reply = strip_route_sentinel(&out.reply);

        Ok(SupervisorResult { branch, raw_reply })
    }

    /// Parse `[[ROUTE:<branch>]]` from the reply and return the branch label if
    /// it matches one of the declared routes. Returns `None` if the sentinel is
    /// absent or the extracted branch is not in the route list.
    fn parse_route_branch(
        reply: &str,
        routes: &[greentic_aw_runtime::graph::SupervisorRoute],
    ) -> Option<String> {
        let lower = reply.to_ascii_lowercase();
        let prefix_lower = ROUTE_SENTINEL_PREFIX.to_ascii_lowercase();
        let suffix_lower = ROUTE_SENTINEL_SUFFIX.to_ascii_lowercase();
        let start = lower.find(&prefix_lower)?;
        let after_prefix = start + prefix_lower.len();
        let end = lower[after_prefix..].find(&suffix_lower)?;
        let branch = reply[after_prefix..after_prefix + end].trim().to_string();
        // Validate against declared routes (case-sensitive match, per spec).
        if routes.iter().any(|r| r.branch == branch) {
            Some(branch)
        } else {
            None
        }
    }

    /// Strip the `[[ROUTE:<branch>]]` sentinel (case-insensitive, first occurrence)
    /// from a supervisor reply, trimming surrounding whitespace.
    ///
    /// If no sentinel is present the reply is returned trimmed. Used to clean
    /// up the text before it is pushed into the message log as an assistant
    /// message.
    fn strip_route_sentinel(reply: &str) -> String {
        let lower = reply.to_ascii_lowercase();
        let prefix_lower = ROUTE_SENTINEL_PREFIX.to_ascii_lowercase();
        let suffix_lower = ROUTE_SENTINEL_SUFFIX.to_ascii_lowercase();
        if let Some(start) = lower.find(&prefix_lower) {
            let after_prefix = start + prefix_lower.len();
            if let Some(end) = lower[after_prefix..].find(&suffix_lower) {
                let sentinel_end = after_prefix + end + suffix_lower.len();
                let mut out = reply.to_string();
                out.replace_range(start..sentinel_end, "");
                return out.trim().to_string();
            }
        }
        reply.trim().to_string()
    }

    /// Build the [`ToolFn`] effect closure.
    ///
    /// `tool_name` is parsed as `"extension_id/tool_name"` (split on the FIRST
    /// `'/'`). Anything without a `'/'`, or an empty side, is a
    /// [`GraphExecError::Tool`]. Dispatch goes through the shared
    /// [`ExtensionRuntime`] via [`dispatch_tool_call`], which wraps the blocking
    /// `invoke_tool` in `spawn_blocking`.
    fn build_tool(ext_runtime: Arc<ExtensionRuntime>) -> ToolFn {
        Arc::new(move |req: ToolCallRequest| {
            let ext_runtime = ext_runtime.clone();
            Box::pin(async move { run_one_tool(req, ext_runtime).await })
                as BoxFut<'static, Result<Value, GraphExecError>>
        })
    }

    /// Parse + dispatch a single Tool-node call. See [`build_tool`].
    async fn run_one_tool(
        req: ToolCallRequest,
        ext_runtime: Arc<ExtensionRuntime>,
    ) -> Result<Value, GraphExecError> {
        let (extension_id, tool_name) = req.tool_name.split_once('/').ok_or_else(|| {
            GraphExecError::Tool(format!(
                "tool name '{}' must be 'extension_id/tool_name'",
                req.tool_name
            ))
        })?;
        if extension_id.is_empty() || tool_name.is_empty() {
            return Err(GraphExecError::Tool(format!(
                "tool name '{}' must be 'extension_id/tool_name' (non-empty parts)",
                req.tool_name
            )));
        }

        // The graph Tool node carries no arguments today: dispatch with an empty
        // object. A future schema can thread structured args from node config.
        let call = greentic_aw_runtime::state::ToolCallRecord {
            call_id: format!("{}__{}", req.node_id, tool_name),
            extension_id: extension_id.to_string(),
            tool_name: tool_name.to_string(),
            args: json!({}),
        };

        // Graph Tool nodes never carry mcp: or component: ids (they use
        // 'extension_id/tool' syntax over the WASM runtime), so neither the MCP
        // nor the component catalog is threaded here.
        dispatch_tool_call(ext_runtime, None, None, call)
            .await
            .map_err(|e| GraphExecError::Tool(format!("dispatch '{}': {e}", req.tool_name)))
    }

    #[cfg(all(test, feature = "agentic-worker"))]
    mod tests {
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        use greentic_aw_runtime::graph::model::SupervisorRoute;
        use greentic_aw_runtime::graph::{
            AgentTurnFn, AgentTurnRequest, AgentTurnResult, GraphConfig, InMemoryCheckpointStore,
            SupervisorFn, SupervisorRequest, SupervisorResult, ToolCallRequest, ToolFn,
        };
        use greentic_aw_runtime::mock::MockAgentStateStore;
        use serde_json::json;

        use super::*;

        // -------------------------------------------------------------------
        // Fixtures
        // -------------------------------------------------------------------

        /// Minimal triage-style graph: agent → lookup tool → router → respond,
        /// router maxIterations=3. Mirrors the aw-runtime test fixture (which is
        /// `pub(crate)` to that crate and thus not importable here).
        fn triage_graph_json(max_iterations: u32) -> String {
            json!({
                "schemaVersion": 1,
                "entry": "agent",
                "nodes": [
                    {"id": "agent", "kind": "agent", "systemPrompt": "You triage.", "model": "gpt-4o-mini", "tools": []},
                    {"id": "lookup", "kind": "tool", "toolName": "kb/search"},
                    {"id": "router", "kind": "router", "maxIterations": max_iterations},
                    {"id": "respond", "kind": "respond"}
                ],
                "edges": [
                    {"from": "agent", "to": "lookup"},
                    {"from": "lookup", "to": "router"},
                    {"from": "router", "to": "agent", "branch": "loop"},
                    {"from": "router", "to": "respond", "branch": "resolved"}
                ]
            })
            .to_string()
        }

        fn triage_cfg(max_iterations: u32) -> GraphConfig {
            GraphConfig::from_json(&triage_graph_json(max_iterations)).expect("fixture valid")
        }

        fn provider_with(graph_id: &str, cfg: GraphConfig) -> Arc<InMemoryGraphProvider> {
            let mut graphs = HashMap::new();
            graphs.insert(graph_id.to_string(), cfg);
            Arc::new(InMemoryGraphProvider::new(graphs))
        }

        /// Agent closure that resolves on the n-th (non-replayed) call.
        fn agent_fn_resolves_on(counter: Arc<AtomicU32>, resolve_on_call: u32) -> AgentTurnFn {
            Arc::new(move |req: AgentTurnRequest| {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let resolved = n >= resolve_on_call;
                let reply = format!("reply-{n} from {}", req.node_id);
                Box::pin(async move { Ok(AgentTurnResult { reply, resolved }) })
            })
        }

        fn tool_fn_ok() -> ToolFn {
            Arc::new(move |_req: ToolCallRequest| {
                Box::pin(async move { Ok(json!({"found": true})) })
            })
        }

        /// A no-op supervisor fn for tests that do not exercise supervisor nodes.
        fn supervisor_fn_noop() -> SupervisorFn {
            Arc::new(|_req: SupervisorRequest| {
                Box::pin(async move {
                    Err(greentic_aw_runtime::graph::GraphExecError::Supervisor(
                        "supervisor fn should not be called in this test".into(),
                    ))
                })
            })
        }

        fn handler_with(
            graphs: Arc<dyn GraphConfigSource>,
            agent_turn: AgentTurnFn,
            tool: ToolFn,
        ) -> RuntimeGraphNodeHandler {
            RuntimeGraphNodeHandler::with_effects(
                graphs,
                Arc::new(InMemoryCheckpointStore::default()),
                Arc::new(MockAgentStateStore::new()),
                agent_turn,
                tool,
                supervisor_fn_noop(),
            )
        }

        // -------------------------------------------------------------------
        // graph_config_from_sidecar tests
        // -------------------------------------------------------------------

        #[test]
        fn sidecar_valid_triage_parses() {
            let bytes = triage_graph_json(3).into_bytes();
            let cfg = super::graph_config_from_sidecar("test-pack", &bytes);
            assert!(cfg.is_some(), "valid triage sidecar must parse");
        }

        #[test]
        fn sidecar_malformed_json_returns_none() {
            let bytes = b"{ this is not valid json";
            // Must not panic; a malformed sidecar is skipped (None).
            let cfg = super::graph_config_from_sidecar("test-pack", bytes);
            assert!(cfg.is_none(), "malformed JSON sidecar must yield None");
        }

        #[test]
        fn sidecar_unsupported_schema_version_returns_none() {
            let bytes = json!({
                "schemaVersion": 99,
                "entry": "agent",
                "nodes": [
                    {"id": "agent", "kind": "agent", "systemPrompt": "x", "model": "gpt-4o-mini", "tools": []},
                    {"id": "respond", "kind": "respond"}
                ],
                "edges": [{"from": "agent", "to": "respond"}]
            })
            .to_string()
            .into_bytes();
            let cfg = super::graph_config_from_sidecar("test-pack", &bytes);
            assert!(
                cfg.is_none(),
                "unsupported schemaVersion must be rejected (None)"
            );
        }

        // -------------------------------------------------------------------
        // Tests
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn executes_graph_and_returns_dw_agent_envelope() {
            let handler = handler_with(
                provider_with("triage", triage_cfg(3)),
                agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1),
                tool_fn_ok(),
            );

            let out = handler
                .execute("t", "e", "triage", "sess-1", &json!({"user_text": "help"}))
                .await
                .expect("execute should succeed");

            assert_eq!(out["terminated_by"].as_str(), Some("respond"));
            assert!(
                out["reply"].as_str().unwrap_or("").contains("reply-1"),
                "reply should carry agent output: {:?}",
                out["reply"]
            );
            assert!(
                out["trail"]
                    .as_array()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false),
                "trail should be a non-empty array: {:?}",
                out["trail"]
            );
        }

        #[tokio::test]
        async fn same_session_resumes_completed_run_with_fresh_run() {
            // Shared checkpoint + state stores so the second call sees the first
            // run's terminal record and mints a fresh `__2` run id.
            let graphs = provider_with("triage", triage_cfg(3));
            let checkpoint = Arc::new(InMemoryCheckpointStore::default());
            let state_store = Arc::new(MockAgentStateStore::new());
            let counter = Arc::new(AtomicU32::new(0));

            let handler = RuntimeGraphNodeHandler::with_effects(
                graphs,
                checkpoint.clone(),
                state_store,
                agent_fn_resolves_on(counter, 1),
                tool_fn_ok(),
                supervisor_fn_noop(),
            );

            let first = handler
                .execute("t", "e", "triage", "sess-1", &json!({"user_text": "one"}))
                .await
                .expect("first call succeeds");
            assert_eq!(first["terminated_by"].as_str(), Some("respond"));

            let second = handler
                .execute("t", "e", "triage", "sess-1", &json!({"user_text": "two"}))
                .await
                .expect("second call succeeds with fresh run id");
            assert_eq!(second["terminated_by"].as_str(), Some("respond"));

            // The fresh run id (`__2`) must exist in the checkpoint store.
            let tenant = TenantContext::new("t", "e");
            let fresh = checkpoint
                .load(&tenant, "sess-1__triage__2")
                .await
                .expect("store ok");
            assert!(fresh.is_some(), "fresh __2 run id should be recorded");
        }

        #[tokio::test]
        async fn missing_graph_returns_structured_error_reply() {
            let handler = handler_with(
                provider_with("triage", triage_cfg(3)),
                agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1),
                tool_fn_ok(),
            );

            let out = handler
                .execute("t", "e", "unknown", "sess-1", &json!({"user_text": "hi"}))
                .await
                .expect("missing graph must NOT return Err");

            assert_eq!(out["terminated_by"].as_str(), Some("error"));
            assert!(
                out["reply"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .contains("available"),
                "reply should mention availability: {:?}",
                out["reply"]
            );
        }

        #[tokio::test]
        async fn missing_user_text_matches_agent_node_contract() {
            // agent_node.rs treats missing `user_text` as an empty string and
            // proceeds (never Err). Assert the same contract here.
            let handler = handler_with(
                provider_with("triage", triage_cfg(3)),
                agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1),
                tool_fn_ok(),
            );

            let out = handler
                .execute("t", "e", "triage", "sess-1", &json!({}))
                .await
                .expect("missing user_text must NOT return Err (matches agent_node)");

            assert_eq!(out["terminated_by"].as_str(), Some("respond"));
        }

        #[tokio::test]
        async fn iteration_cap_maps_to_error_envelope() {
            // maxIterations=1000 on the router → the executor's global
            // MAX_NODE_VISITS cap (64) trips first → IterationCap → "error".
            let handler = handler_with(
                provider_with("triage", triage_cfg(1000)),
                // never resolves → loops until the global visit cap
                agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), u32::MAX),
                tool_fn_ok(),
            );

            let out = handler
                .execute("t", "e", "triage", "sess-1", &json!({"user_text": "loop"}))
                .await
                .expect("iteration cap must NOT return Err");

            assert_eq!(out["terminated_by"].as_str(), Some("error"));
            assert_ne!(
                out["reply"].as_str(),
                Some(SANITISED_ERROR_REPLY),
                "iteration cap should use a distinct reply"
            );
        }

        /// Regression test: flow session ids can contain `':'` (e.g. MS Teams
        /// channel correlation ids like `"msteams:thread:19xyz"`). The checkpoint
        /// store rejects keys that contain `':'`, so without sanitization every
        /// such graph run fails at save-time. [`derive_run_id`] must replace every
        /// `':'` with `'_'` before composing the checkpoint key.
        #[tokio::test]
        async fn colon_bearing_session_id_executes_successfully() {
            let handler = handler_with(
                provider_with("triage", triage_cfg(3)),
                agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1),
                tool_fn_ok(),
            );

            // Session id with multiple colons — typical of Teams thread ids.
            let out = handler
                .execute(
                    "t",
                    "e",
                    "triage",
                    "msteams:thread:19xyz",
                    &json!({"user_text": "hello"}),
                )
                .await
                .expect("colon-bearing session id must NOT return Err");

            assert_eq!(
                out["terminated_by"].as_str(),
                Some("respond"),
                "colon-bearing session id should complete successfully: {:?}",
                out
            );
        }

        // -------------------------------------------------------------------
        // LayeredGraphProvider tests
        // -------------------------------------------------------------------

        /// Provider that always returns the given error.
        struct ErrGraphProvider(fn() -> ConfigError);

        impl GraphConfigSource for ErrGraphProvider {
            fn graph_config<'a>(
                &'a self,
                _tenant: &'a TenantContext,
                _graph_id: &'a str,
            ) -> BoxFut<'a, Result<GraphConfig, ConfigError>> {
                let e = (self.0)();
                Box::pin(async move { Err(e) })
            }
        }

        /// Fallback that panics if consulted — proves primary short-circuits.
        struct PanicGraphProvider;

        impl GraphConfigSource for PanicGraphProvider {
            fn graph_config<'a>(
                &'a self,
                _tenant: &'a TenantContext,
                _graph_id: &'a str,
            ) -> BoxFut<'a, Result<GraphConfig, ConfigError>> {
                Box::pin(
                    async move { panic!("fallback must not be consulted when primary returns Ok") },
                )
            }
        }

        #[tokio::test]
        async fn layered_primary_ok_short_circuits_fallback() {
            let mut graphs = HashMap::new();
            graphs.insert("g".to_string(), triage_cfg(3));
            let primary = InMemoryGraphProvider::new(graphs);
            let layered = LayeredGraphProvider::new(primary, PanicGraphProvider);
            let tc = TenantContext::new("t", "e");
            // If fallback were consulted, PanicGraphProvider panics and fails this.
            let cfg = layered.graph_config(&tc, "g").await.unwrap();
            assert_eq!(cfg.graph.entry, "agent");
        }

        #[tokio::test]
        async fn layered_falls_back_on_not_found() {
            let mut graphs = HashMap::new();
            graphs.insert("g".to_string(), triage_cfg(3));
            let fb = InMemoryGraphProvider::new(graphs);
            let layered = LayeredGraphProvider::new(
                ErrGraphProvider(|| ConfigError::AgentNotFound("g".into())),
                fb,
            );
            let tc = TenantContext::new("t", "e");
            let cfg = layered.graph_config(&tc, "g").await.unwrap();
            assert_eq!(cfg.graph.entry, "agent");
        }

        #[tokio::test]
        async fn layered_falls_back_on_internal() {
            let mut graphs = HashMap::new();
            graphs.insert("g".to_string(), triage_cfg(3));
            let fb = InMemoryGraphProvider::new(graphs);
            let layered = LayeredGraphProvider::new(
                ErrGraphProvider(|| ConfigError::Internal("down".into())),
                fb,
            );
            let tc = TenantContext::new("t", "e");
            let cfg = layered.graph_config(&tc, "g").await.unwrap();
            assert_eq!(cfg.graph.entry, "agent");
        }

        #[tokio::test]
        async fn layered_propagates_misconfigured_without_fallback() {
            let fb = InMemoryGraphProvider::new(HashMap::new());
            let layered = LayeredGraphProvider::new(
                ErrGraphProvider(|| ConfigError::Misconfigured("bad schema".into())),
                fb,
            );
            let tc = TenantContext::new("t", "e");
            let result = layered.graph_config(&tc, "g").await;
            assert!(
                matches!(result, Err(ConfigError::Misconfigured(_))),
                "Misconfigured must NOT fall back: {result:?}"
            );
        }

        // -------------------------------------------------------------------
        // graph_registry_from_env tests
        // -------------------------------------------------------------------

        #[test]
        #[serial_test::serial]
        #[allow(unsafe_code)]
        fn graph_registry_from_env_requires_both_vars() {
            // SAFETY: #[serial] serializes env-mutating tests; vars cleaned up.
            unsafe {
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
            assert!(super::graph_registry_from_env().is_none());

            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", "http://localhost:9999");
            }
            assert!(
                super::graph_registry_from_env().is_none(),
                "endpoint alone is not enough"
            );

            unsafe {
                std::env::set_var("GREENTIC_AW_ADMIN_TOKEN", "gtc_live_x");
            }
            assert!(super::graph_registry_from_env().is_some());

            // Token-alone (no endpoint) must also be None.
            unsafe {
                std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
            }
            assert!(
                super::graph_registry_from_env().is_none(),
                "token alone is not enough"
            );

            unsafe {
                std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
            }
        }

        // -------------------------------------------------------------------
        // Supervisor fixture
        // -------------------------------------------------------------------

        /// Supervisor graph (schemaVersion 2): sup → agent_billing / agent_tech →
        /// router → respond. Mirrors the aw-runtime `supervisor_json()` fixture
        /// (which is `pub(crate)` to that crate and cannot be imported here).
        ///
        /// Topology:
        ///   sup (supervisor: routes=[billing, tech])
        ///    ├─[billing]─► agent_billing ─► router_billing ─┬─[loop]──► sup
        ///    │                                               └─[resolved]─► respond
        ///    └─[tech]────► agent_tech ───► router_tech    ─┬─[loop]──► sup
        ///                                                   └─[resolved]─► respond
        fn supervisor_graph_json() -> String {
            json!({
                "schemaVersion": 2,
                "entry": "sup",
                "nodes": [
                    {
                        "id": "sup",
                        "kind": "supervisor",
                        "systemPrompt": "Route the request to the correct specialist.",
                        "model": "gpt-4o-mini",
                        "routes": [
                            {"branch": "billing", "description": "Billing and payment questions"},
                            {"branch": "tech",    "description": "Technical support issues"}
                        ]
                    },
                    {"id": "agent_billing", "kind": "agent", "systemPrompt": "Billing.", "model": "gpt-4o-mini", "tools": []},
                    {"id": "router_billing", "kind": "router", "maxIterations": 2},
                    {"id": "agent_tech",    "kind": "agent", "systemPrompt": "Tech.",    "model": "gpt-4o-mini", "tools": []},
                    {"id": "router_tech",   "kind": "router", "maxIterations": 2},
                    {"id": "respond",       "kind": "respond"}
                ],
                "edges": [
                    {"from": "sup",           "to": "agent_billing",  "branch": "billing"},
                    {"from": "sup",           "to": "agent_tech",     "branch": "tech"},
                    {"from": "agent_billing", "to": "router_billing"},
                    {"from": "router_billing","to": "agent_billing",  "branch": "loop"},
                    {"from": "router_billing","to": "respond",        "branch": "resolved"},
                    {"from": "agent_tech",    "to": "router_tech"},
                    {"from": "router_tech",   "to": "agent_tech",     "branch": "loop"},
                    {"from": "router_tech",   "to": "respond",        "branch": "resolved"}
                ]
            })
            .to_string()
        }

        fn supervisor_cfg() -> GraphConfig {
            GraphConfig::from_json(&supervisor_graph_json()).expect("supervisor fixture valid")
        }

        /// Helpers for building [`SupervisorRoute`] values inline.
        fn route(branch: &str, description: &str) -> SupervisorRoute {
            SupervisorRoute {
                branch: branch.to_string(),
                description: description.to_string(),
            }
        }

        /// Build a supervisor fn that always routes to `fixed_branch`.
        fn supervisor_fn_routes_to(fixed_branch: &str) -> SupervisorFn {
            let branch = fixed_branch.to_string();
            Arc::new(move |_req: SupervisorRequest| {
                let branch = branch.clone();
                Box::pin(async move {
                    Ok(SupervisorResult {
                        branch: branch.clone(),
                        raw_reply: format!("I'll route this. [[ROUTE:{branch}]] Done."),
                    })
                })
            })
        }

        // Build a handler wired with the given supervisor fn (and a no-op agent
        // that always resolves immediately).
        fn supervisor_handler(
            graphs: Arc<dyn GraphConfigSource>,
            supervisor: SupervisorFn,
        ) -> RuntimeGraphNodeHandler {
            let agent: AgentTurnFn = agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1);
            RuntimeGraphNodeHandler::with_effects(
                graphs,
                Arc::new(InMemoryCheckpointStore::default()),
                Arc::new(MockAgentStateStore::new()),
                agent,
                tool_fn_ok(),
                supervisor,
            )
        }

        // -------------------------------------------------------------------
        // parse_route_branch unit tests
        // -------------------------------------------------------------------

        /// Two routes for the parse tests.
        fn billing_tech_routes() -> Vec<SupervisorRoute> {
            vec![
                route("billing", "Billing and payment questions"),
                route("tech", "Technical support issues"),
            ]
        }

        #[test]
        fn parse_route_extracts_billing_from_reply() {
            // Standard case: sentinel embedded in a longer reply.
            let reply = "I analysed the message. [[ROUTE:billing]] Go ahead.";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(result, Some("billing".to_string()));
        }

        #[test]
        fn parse_route_case_insensitive_prefix() {
            // The prefix [[ROUTE: is matched case-insensitively; the extracted
            // branch label must still match the declared route exactly
            // (case-sensitive per spec).
            let reply = "[[route:tech]] is the answer";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(result, Some("tech".to_string()));
        }

        #[test]
        fn parse_route_missing_sentinel_returns_none() {
            // No sentinel at all → None (caller's unwrap_or_else picks first route).
            let reply = "I think billing would be best here.";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(result, None);
        }

        #[test]
        fn parse_route_unknown_branch_returns_none() {
            // Sentinel present but branch label does not match any declared route.
            let reply = "[[ROUTE:unknown_branch]] routing";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(result, None);
        }

        #[test]
        fn parse_route_multiple_sentinels_uses_first() {
            // When multiple sentinels are present the function uses the FIRST one
            // (find on the lowercased string returns the first occurrence).
            let reply = "[[ROUTE:billing]] or [[ROUTE:tech]]";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(
                result,
                Some("billing".to_string()),
                "multiple sentinels: first occurrence wins"
            );
        }

        #[test]
        fn parse_route_branch_label_whitespace_trimmed() {
            // Whitespace around the branch label inside the sentinel is trimmed.
            let reply = "[[ROUTE:  billing  ]]";
            let result = parse_route_branch(reply, &billing_tech_routes());
            assert_eq!(result, Some("billing".to_string()));
        }

        // -------------------------------------------------------------------
        // strip_route_sentinel unit tests
        // -------------------------------------------------------------------

        #[test]
        fn strip_sentinel_removes_token_leaves_rest() {
            let reply = "I'll route this. [[ROUTE:billing]] Proceeding.";
            let stripped = strip_route_sentinel(reply);
            assert!(
                !stripped.contains("[[ROUTE:billing]]"),
                "sentinel must be removed: {stripped:?}"
            );
            assert!(
                stripped.contains("Proceeding"),
                "text after sentinel must survive: {stripped:?}"
            );
        }

        #[test]
        fn strip_sentinel_no_sentinel_returns_trimmed() {
            let reply = "  No routing needed here.  ";
            let stripped = strip_route_sentinel(reply);
            assert_eq!(stripped, "No routing needed here.");
        }

        #[test]
        fn strip_sentinel_case_insensitive_removal() {
            let reply = "Decision: [[route:tech]] done.";
            let stripped = strip_route_sentinel(reply);
            assert!(
                !stripped.to_ascii_lowercase().contains("[[route:"),
                "sentinel must be stripped case-insensitively: {stripped:?}"
            );
        }

        #[test]
        fn strip_sentinel_only_first_occurrence_removed() {
            // If two sentinels appear, only the first is stripped.
            let reply = "[[ROUTE:billing]] first [[ROUTE:tech]] second";
            let stripped = strip_route_sentinel(reply);
            assert!(
                !stripped.contains("[[ROUTE:billing]]"),
                "first sentinel must be gone: {stripped:?}"
            );
            // The second sentinel remains (we only strip the first occurrence).
            assert!(
                stripped.contains("[[ROUTE:tech]]"),
                "second sentinel must remain: {stripped:?}"
            );
        }

        // -------------------------------------------------------------------
        // Fallback: parse_route_branch None → first-route fallback
        // -------------------------------------------------------------------

        #[test]
        fn parse_route_none_documents_fallback_to_first_route() {
            // This is a pure-fn test of the fallback path used in
            // `run_one_supervisor_turn` via `unwrap_or_else(|| first_route)`.
            // When parse_route_branch returns None, the build_supervisor closure
            // picks req.routes.first().branch as the fallback.
            let routes = billing_tech_routes();
            let garbage = "absolutely no routing sentinel here";
            let parsed = parse_route_branch(garbage, &routes);
            assert_eq!(parsed, None, "no sentinel → None from parser");

            // Simulate the fallback: first route is "billing".
            let fallback = parsed
                .unwrap_or_else(|| routes.first().map(|r| r.branch.clone()).unwrap_or_default());
            assert_eq!(
                fallback, "billing",
                "None → first route 'billing' as fallback"
            );
        }

        // -------------------------------------------------------------------
        // Handler-level supervisor routing tests
        // -------------------------------------------------------------------

        #[tokio::test]
        async fn supervisor_routes_to_billing_returns_respond_envelope() {
            // The mock supervisor always picks "billing". The billing agent
            // resolves immediately. Expect terminated_by="respond" and a
            // non-empty reply.
            let handler = supervisor_handler(
                provider_with("sup_graph", supervisor_cfg()),
                supervisor_fn_routes_to("billing"),
            );

            let out = handler
                .execute(
                    "t",
                    "e",
                    "sup_graph",
                    "sess-sup-1",
                    &json!({"user_text": "I need billing help"}),
                )
                .await
                .expect("supervisor execute must not Err");

            assert_eq!(
                out["terminated_by"].as_str(),
                Some("respond"),
                "billing path must terminate at respond: {out:?}"
            );
            assert!(
                out["reply"].as_str().is_some_and(|r| !r.is_empty()),
                "reply must be non-empty: {out:?}"
            );
            assert!(
                out["trail"].as_array().is_some_and(|t| !t.is_empty()),
                "trail must be non-empty: {out:?}"
            );
        }

        #[tokio::test]
        async fn supervisor_routes_to_tech_returns_respond_envelope() {
            // Same topology, supervisor picks "tech" branch instead.
            let handler = supervisor_handler(
                provider_with("sup_graph", supervisor_cfg()),
                supervisor_fn_routes_to("tech"),
            );

            let out = handler
                .execute(
                    "t",
                    "e",
                    "sup_graph",
                    "sess-sup-2",
                    &json!({"user_text": "my device is broken"}),
                )
                .await
                .expect("supervisor tech-route execute must not Err");

            assert_eq!(
                out["terminated_by"].as_str(),
                Some("respond"),
                "tech path must terminate at respond: {out:?}"
            );
        }

        #[tokio::test]
        async fn supervisor_fallback_on_missing_sentinel_still_completes() {
            // A supervisor fn that returns a raw_reply with NO [[ROUTE:]] sentinel.
            // The build_supervisor closure maps None→first-route ("billing").
            // The handler-level execute() must still complete (not Err).
            //
            // Because with_effects injects the supervisor fn directly (bypassing
            // build_supervisor's parse), we simulate the fallback by having the
            // fn return the first route explicitly — matching what build_supervisor
            // does in production via parse_route_branch(...)
            //   .unwrap_or_else(|| routes.first().branch).
            // The pure-fn fallback path is fully covered by
            // `parse_route_none_documents_fallback_to_first_route`.
            let supervisor: SupervisorFn = Arc::new(|_req: SupervisorRequest| {
                // No sentinel in raw_reply; we return first-route directly as
                // the mock of the fallback behaviour.
                Box::pin(async move {
                    Ok(SupervisorResult {
                        branch: "billing".to_string(),
                        raw_reply: "I think billing is right.".to_string(),
                    })
                })
            });

            let handler =
                supervisor_handler(provider_with("sup_graph", supervisor_cfg()), supervisor);

            let out = handler
                .execute(
                    "t",
                    "e",
                    "sup_graph",
                    "sess-sup-3",
                    &json!({"user_text": "help me"}),
                )
                .await
                .expect("fallback supervisor must not Err");

            assert_eq!(
                out["terminated_by"].as_str(),
                Some("respond"),
                "fallback path must still reach respond: {out:?}"
            );
        }
    }
}

#[cfg(feature = "agentic-worker")]
pub use aw::{
    GraphConfigSource, InMemoryGraphProvider, LayeredGraphProvider, RuntimeGraphNodeHandler,
    build_graph_node_handler, graph_config_from_sidecar,
};
