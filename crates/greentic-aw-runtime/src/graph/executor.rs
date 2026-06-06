//! Durable graph executor — the node-visiting drive loop.
//!
//! Ports the semantics of the greentic-designer spike
//! (`src/orchestrate/agent_graph/executor.rs`, origin/spike/agent-graph-engine-slice)
//! with the following adaptations:
//!
//! - Uses [`GraphConfig`] / [`GraphRunState`] / [`CheckpointStore`] from this
//!   crate rather than the designer's SQLite-backed checkpoint module.
//! - Effect closures are `Arc<dyn Fn(…) -> BoxFut<…>>` (shared, cloneable)
//!   instead of owned `Box<dyn Fn…>`.
//! - `start`/`resume` return `Result<GraphRunOutcome, GraphExecError>` (not
//!   `Result<()>`), carrying the final reply and visit trail.
//!
//! ## Record-then-checkpoint ordering (replayable resume)
//!
//! Each side-effect node (Agent, Tool) follows a strict two-write ordering:
//! the effect's result is recorded into the node-visit store
//! (`CheckpointStore::record_node_visit`) *immediately after the effect
//! returns and before the checkpoint update*. The checkpoint update then
//! commits the new cursor, state, and per-node `visits` counts atomically.
//!
//! On resume at cursor node N, the next attempt is `visits[N] + 1`. If a
//! `(run, N, attempt)` visit row already exists, the effect ran but the
//! process crashed before the checkpoint committed — so the recorded result
//! is **replayed** instead of re-invoking the effect.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::tenant::TenantContext;

use super::checkpoint::{CheckpointError, CheckpointStore, GraphRunRecord, RunStatus};
use super::model::{GraphConfig, GraphError, NodeKind};
use super::router::route;
use super::state::{GraphRole, GraphRunState};

// ---------------------------------------------------------------------------
// BoxFut alias — shared with checkpoint.rs pattern
// ---------------------------------------------------------------------------

/// Owned heap-allocated future, `Send + 'static`.
///
/// Kept `pub` (not `pub(crate)`) because external crates that construct
/// [`AgentTurnFn`] or [`ToolFn`] closures must be able to name this type as
/// their return type.  The Task-7 `DwAgentGraph` handler in `greentic-runner-host`
/// is the primary consumer.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Effect request / result types
// ---------------------------------------------------------------------------

/// Request payload delivered to an injected agent-turn closure.
#[derive(Debug, Clone)]
pub struct AgentTurnRequest {
    /// Graph node id (for telemetry / routing context).
    pub node_id: String,
    /// System prompt from the node's configuration.
    pub system_prompt: String,
    /// Model identifier from the node's configuration.
    pub model: String,
    /// Current run state at the time of the turn.
    pub state: GraphRunState,
}

/// Result returned by an injected agent-turn closure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTurnResult {
    /// The assistant's reply text.
    pub reply: String,
    /// `true` when the agent considers the issue fully resolved.
    pub resolved: bool,
}

/// Request payload delivered to an injected tool closure.
#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    /// Graph node id.
    pub node_id: String,
    /// Tool name from the node's configuration.
    pub tool_name: String,
    /// Current run state at the time of the call.
    pub state: GraphRunState,
}

// ---------------------------------------------------------------------------
// Injected effect types
// ---------------------------------------------------------------------------

/// One agent turn: the host wires this to `AgentRuntime::step`.
pub type AgentTurnFn = Arc<
    dyn Fn(AgentTurnRequest) -> BoxFut<'static, Result<AgentTurnResult, GraphExecError>>
        + Send
        + Sync,
>;

/// One deterministic tool call.
pub type ToolFn = Arc<
    dyn Fn(ToolCallRequest) -> BoxFut<'static, Result<serde_json::Value, GraphExecError>>
        + Send
        + Sync,
>;

// ---------------------------------------------------------------------------
// GraphExecError
// ---------------------------------------------------------------------------

/// Errors that may surface from [`GraphExecutor::start`] or
/// [`GraphExecutor::resume`].
#[derive(Debug, thiserror::Error)]
pub enum GraphExecError {
    #[error("graph run {run_id} exceeded the node-visit cap")]
    IterationCap { run_id: String },

    #[error("unknown node `{0}` (cursor corrupt or graph changed)")]
    UnknownNode(String),

    #[error("unknown run `{0}`")]
    UnknownRun(String),

    #[error("run `{0}` already completed")]
    AlreadyCompleted(String),

    #[error(transparent)]
    Graph(#[from] GraphError),

    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),

    #[error("agent turn failed: {0}")]
    AgentTurn(String),

    #[error("tool call failed: {0}")]
    Tool(String),
}

// ---------------------------------------------------------------------------
// GraphRunOutcome
// ---------------------------------------------------------------------------

/// The final result of a drive-loop execution.
#[derive(Debug, Clone)]
pub struct GraphRunOutcome {
    /// Terminal status (`Succeeded` or `Failed`).
    pub status: RunStatus,
    /// Last assistant message emitted (what the Respond node returns), or an
    /// empty string if the run never produced an assistant reply.
    pub reply: String,
    /// One JSON entry per node visit.
    ///
    /// **Drive-loop shape** (normal execution or active resume):
    /// `{"node": id, "kind": "agent|tool|router|respond", "attempt": n, "replayed": bool}`.
    ///
    /// **Terminal-resume shape** (returned by [`GraphExecutor::resume`] when the run
    /// is already in a terminal state — built by `rebuild_trail_from_state`):
    /// `{"kind": "user|agent|tool", "content": "…"}`.  The node id and attempt
    /// count are not available from the stored message log, so this shape is
    /// intentionally narrower.  Callers that need both shapes must handle both.
    pub trail: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard upper bound on node visits per `drive` call, independent of the
/// per-router `maxIterations` cap. Prevents infinite loops on malformed
/// graphs.
pub const MAX_NODE_VISITS: u32 = 64;

// ---------------------------------------------------------------------------
// GraphExecutor
// ---------------------------------------------------------------------------

/// Drives agent-graph runs to completion, persisting checkpoints after every
/// node so that a killed process can resume mid-loop.
pub struct GraphExecutor {
    store: Arc<dyn CheckpointStore>,
    agent_turn: AgentTurnFn,
    tool: ToolFn,
}

impl GraphExecutor {
    /// Construct a new executor.
    pub fn new(store: Arc<dyn CheckpointStore>, agent_turn: AgentTurnFn, tool: ToolFn) -> Self {
        Self {
            store,
            agent_turn,
            tool,
        }
    }

    /// Start a **new** run.
    ///
    /// Validates that `run_id` is fresh:
    /// - If a record already exists with status `Running` → delegates to
    ///   resume logic.
    /// - If a record exists in a terminal state → returns
    ///   [`GraphExecError::AlreadyCompleted`].
    ///
    /// Otherwise, seeds [`GraphRunState`] with the user message, snapshots the
    /// graph JSON, saves the initial `Running` record, and drives the loop.
    pub async fn start(
        &self,
        tenant: &TenantContext,
        run_id: &str,
        cfg: &GraphConfig,
        user_text: &str,
    ) -> Result<GraphRunOutcome, GraphExecError> {
        // Check if a record already exists.
        if let Some(existing) = self.store.load(tenant, run_id).await? {
            return match existing.status {
                RunStatus::Running => {
                    // Resume the in-flight run.
                    self.drive_from_record(tenant, run_id, existing).await
                }
                RunStatus::Succeeded | RunStatus::Failed => {
                    Err(GraphExecError::AlreadyCompleted(run_id.to_owned()))
                }
            };
        }

        // Fresh run — seed state.
        let mut state = GraphRunState::default();
        state.push_message(GraphRole::User, user_text);

        let graph_json = serde_json::to_string(cfg)
            .map_err(|e| GraphExecError::Checkpoint(CheckpointError::Serde(e)))?;
        let cursor = cfg.graph.entry.clone();
        let visits: HashMap<String, u32> = HashMap::new();

        let rec = build_record(
            run_id,
            &graph_json,
            &cursor,
            &state,
            &visits,
            RunStatus::Running,
        )?;
        self.store.save(tenant, &rec).await?;

        self.drive(tenant, run_id, cfg.clone(), cursor, state, visits)
            .await
    }

    /// Resume an **existing** run.
    ///
    /// - If the run does not exist → [`GraphExecError::UnknownRun`].
    /// - If the run is already in a terminal state → return the stored outcome
    ///   WITHOUT re-driving.
    pub async fn resume(
        &self,
        tenant: &TenantContext,
        run_id: &str,
    ) -> Result<GraphRunOutcome, GraphExecError> {
        let rec = self
            .store
            .load(tenant, run_id)
            .await?
            .ok_or_else(|| GraphExecError::UnknownRun(run_id.to_owned()))?;

        match rec.status {
            RunStatus::Succeeded | RunStatus::Failed => {
                // Terminal — rebuild outcome from stored state without re-driving.
                let state: GraphRunState =
                    serde_json::from_str(&rec.state_json).map_err(CheckpointError::Serde)?;
                let reply = last_assistant_message(&state);
                let trail = rebuild_trail_from_state(&state);
                Ok(GraphRunOutcome {
                    status: rec.status,
                    reply,
                    trail,
                })
            }
            RunStatus::Running => self.drive_from_record(tenant, run_id, rec).await,
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Deserialise a stored record and call [`drive`].
    async fn drive_from_record(
        &self,
        tenant: &TenantContext,
        run_id: &str,
        rec: GraphRunRecord,
    ) -> Result<GraphRunOutcome, GraphExecError> {
        let cfg: GraphConfig = GraphConfig::from_json(&rec.graph_json)?;
        let state: GraphRunState =
            serde_json::from_str(&rec.state_json).map_err(CheckpointError::Serde)?;
        let visits: HashMap<String, u32> =
            serde_json::from_str(&rec.visits_json).map_err(CheckpointError::Serde)?;
        let cursor = rec.cursor.clone();
        self.drive(tenant, run_id, cfg, cursor, state, visits).await
    }

    /// The core node-visiting loop. Persists a checkpoint after every node.
    ///
    /// Semantics are ported faithfully from the designer spike:
    /// - Record-before-checkpoint ordering for side-effect nodes.
    /// - `iterations` increments AFTER `visits.insert` and message push (same
    ///   placement as the spike).
    /// - Respond node: saves Succeeded, returns immediately.
    /// - Cap exhausted: saves Failed, returns `Err(IterationCap)`.
    /// - Effect error: does NOT mark the run Failed (it stays Running so
    ///   `resume` can retry the failed node).
    async fn drive(
        &self,
        tenant: &TenantContext,
        run_id: &str,
        cfg: GraphConfig,
        mut cursor: String,
        mut state: GraphRunState,
        mut visits: HashMap<String, u32>,
    ) -> Result<GraphRunOutcome, GraphExecError> {
        let mut trail: Vec<serde_json::Value> = Vec::new();

        for _ in 0..MAX_NODE_VISITS {
            let node = cfg
                .graph
                .node(&cursor)
                .ok_or_else(|| GraphExecError::UnknownNode(cursor.clone()))?
                .clone();

            match &node.kind {
                NodeKind::Agent {
                    system_prompt,
                    model,
                    ..
                } => {
                    let attempt = *visits.get(&cursor).unwrap_or(&0) + 1;

                    // Pre-clone so the closure can own the values it needs.
                    let node_id_for_err = cursor.clone();
                    let (raw, replayed) = self
                        .visit_effect(tenant, run_id, &cursor, attempt, || {
                            let req = AgentTurnRequest {
                                node_id: node_id_for_err.clone(),
                                system_prompt: system_prompt.clone(),
                                model: model.clone(),
                                state: state.clone(),
                            };
                            let fut = (self.agent_turn)(req);
                            Box::pin(async move {
                                let r = fut.await.map_err(|e| {
                                    GraphExecError::AgentTurn(format!(
                                        "node '{}' attempt {}: {}",
                                        node_id_for_err, attempt, e
                                    ))
                                })?;
                                serde_json::to_value(&r)
                                    .map_err(CheckpointError::Serde)
                                    .map_err(GraphExecError::Checkpoint)
                            })
                        })
                        .await?;

                    let result: AgentTurnResult =
                        serde_json::from_value(raw).map_err(CheckpointError::Serde)?;

                    trail.push(serde_json::json!({
                        "node": cursor,
                        "kind": "agent",
                        "attempt": attempt,
                        "replayed": replayed,
                    }));

                    // Update state — in the same order as the spike.
                    visits.insert(cursor.clone(), attempt);
                    state.iterations += 1;
                    state.push_message(GraphRole::Assistant, &result.reply);
                    if result.resolved {
                        state.resolved = true;
                    }

                    // Advance cursor (single outgoing edge).
                    cursor = next_linear(&cfg, &cursor)?;

                    // Checkpoint: cursor already advanced; state reflects this visit.
                    let rec = build_record(
                        run_id,
                        &serde_json::to_string(&cfg).map_err(CheckpointError::Serde)?,
                        &cursor,
                        &state,
                        &visits,
                        RunStatus::Running,
                    )?;
                    self.store.save(tenant, &rec).await?;
                }

                NodeKind::Tool { tool_name } => {
                    let attempt = *visits.get(&cursor).unwrap_or(&0) + 1;

                    // Pre-clone so the closure can own the values it needs.
                    let node_id_for_err = cursor.clone();
                    let (result, replayed) = self
                        .visit_effect(tenant, run_id, &cursor, attempt, || {
                            let req = ToolCallRequest {
                                node_id: node_id_for_err.clone(),
                                tool_name: tool_name.clone(),
                                state: state.clone(),
                            };
                            let fut = (self.tool)(req);
                            Box::pin(async move {
                                fut.await.map_err(|e| {
                                    GraphExecError::Tool(format!(
                                        "node '{}' attempt {}: {}",
                                        node_id_for_err, attempt, e
                                    ))
                                })
                            })
                        })
                        .await?;

                    trail.push(serde_json::json!({
                        "node": cursor,
                        "kind": "tool",
                        "attempt": attempt,
                        "replayed": replayed,
                    }));

                    visits.insert(cursor.clone(), attempt);
                    state.push_message(GraphRole::Tool, result.to_string());

                    cursor = next_linear(&cfg, &cursor)?;

                    let rec = build_record(
                        run_id,
                        &serde_json::to_string(&cfg).map_err(CheckpointError::Serde)?,
                        &cursor,
                        &state,
                        &visits,
                        RunStatus::Running,
                    )?;
                    self.store.save(tenant, &rec).await?;
                }

                NodeKind::Router { .. } => {
                    let attempt = *visits.get(&cursor).unwrap_or(&0) + 1;

                    let next = route(&cfg.graph, &cursor, &state)?;

                    trail.push(serde_json::json!({
                        "node": cursor,
                        "kind": "router",
                        "attempt": attempt,
                        "replayed": false,
                    }));

                    visits.insert(cursor.clone(), attempt);
                    cursor = next;

                    let rec = build_record(
                        run_id,
                        &serde_json::to_string(&cfg).map_err(CheckpointError::Serde)?,
                        &cursor,
                        &state,
                        &visits,
                        RunStatus::Running,
                    )?;
                    self.store.save(tenant, &rec).await?;
                }

                NodeKind::Respond => {
                    let attempt = *visits.get(&cursor).unwrap_or(&0) + 1;

                    trail.push(serde_json::json!({
                        "node": cursor,
                        "kind": "respond",
                        "attempt": attempt,
                        "replayed": false,
                    }));

                    visits.insert(cursor.clone(), attempt);

                    // Save terminal checkpoint.
                    let rec = build_record(
                        run_id,
                        &serde_json::to_string(&cfg).map_err(CheckpointError::Serde)?,
                        &cursor,
                        &state,
                        &visits,
                        RunStatus::Succeeded,
                    )?;
                    self.store.save(tenant, &rec).await?;

                    let reply = last_assistant_message(&state);
                    return Ok(GraphRunOutcome {
                        status: RunStatus::Succeeded,
                        reply,
                        trail,
                    });
                }
            }
        }

        // Cap exhausted — mark as Failed first, then propagate the error.
        let graph_json = serde_json::to_string(&cfg).map_err(CheckpointError::Serde)?;
        let rec = build_record(
            run_id,
            &graph_json,
            &cursor,
            &state,
            &visits,
            RunStatus::Failed,
        )?;
        self.store.save(tenant, &rec).await?;

        Err(GraphExecError::IterationCap {
            run_id: run_id.to_owned(),
        })
    }

    /// Load-or-invoke-and-record a single side-effect node visit.
    ///
    /// Returns `(serialised_result, replayed)`:
    /// - `replayed = true` means the result was already stored (crash recovery);
    ///   the `invoke` closure was NOT called.
    /// - `replayed = false` means `invoke` was called and its result has been
    ///   durably recorded before returning.
    ///
    /// The `invoke` closure is responsible for wrapping any effect-level error
    /// with `node_id`/`attempt` context **before** returning it here, so that
    /// `?`-propagation in the caller carries the full diagnostic.
    async fn visit_effect(
        &self,
        tenant: &TenantContext,
        run_id: &str,
        node_id: &str,
        attempt: u32,
        invoke: impl FnOnce() -> BoxFut<'static, Result<serde_json::Value, GraphExecError>>,
    ) -> Result<(serde_json::Value, bool), GraphExecError> {
        if let Some(cached) = self
            .store
            .load_node_visit(tenant, run_id, node_id, attempt)
            .await?
        {
            return Ok((cached, true));
        }

        // Effect not yet recorded — invoke and record before returning.
        let value = invoke().await?;
        // Record BEFORE checkpoint (replayable resume ordering).
        self.store
            .record_node_visit(tenant, run_id, node_id, attempt, &value)
            .await?;
        Ok((value, false))
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Serialise all fields into a [`GraphRunRecord`].
fn build_record(
    run_id: &str,
    graph_json: &str,
    cursor: &str,
    state: &GraphRunState,
    visits: &HashMap<String, u32>,
    status: RunStatus,
) -> Result<GraphRunRecord, GraphExecError> {
    let state_json = serde_json::to_string(state).map_err(CheckpointError::Serde)?;
    let visits_json = serde_json::to_string(visits).map_err(CheckpointError::Serde)?;
    Ok(GraphRunRecord {
        run_id: run_id.to_owned(),
        graph_json: graph_json.to_owned(),
        cursor: cursor.to_owned(),
        state_json,
        status,
        visits_json,
    })
}

/// Return the single outgoing edge target, or an error if none.
fn next_linear(cfg: &GraphConfig, id: &str) -> Result<String, GraphExecError> {
    cfg.graph
        .edges_from(id)
        .next()
        .map(|e| e.to.clone())
        .ok_or_else(|| {
            GraphExecError::Graph(super::model::GraphError::Invalid(format!(
                "node '{id}' has no outgoing edge"
            )))
        })
}

/// The most recent assistant message content, or empty string if none.
fn last_assistant_message(state: &GraphRunState) -> String {
    state
        .messages
        .iter()
        .rev()
        .find(|m| m.role == GraphRole::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Rebuild a minimal trail from the state message log (used when returning
/// a stored terminal outcome without re-driving).
fn rebuild_trail_from_state(state: &GraphRunState) -> Vec<serde_json::Value> {
    state
        .messages
        .iter()
        .map(|m| {
            let kind = match m.role {
                GraphRole::User => "user",
                GraphRole::Assistant => "agent",
                GraphRole::Tool => "tool",
            };
            serde_json::json!({"kind": kind, "content": m.content})
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::graph::test_fixtures::triage_json;
    use crate::graph::{GraphConfig, InMemoryCheckpointStore};
    use crate::tenant::TenantContext;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn tenant() -> TenantContext {
        TenantContext::new("test", "dev")
    }

    fn triage_cfg() -> GraphConfig {
        GraphConfig::from_json(&triage_json()).expect("fixture is valid")
    }

    /// Build an [`AgentTurnFn`] that resolves on the n-th call (1-indexed).
    /// `counter` is incremented on every (non-replayed) invocation.
    fn agent_fn_resolves_on(counter: Arc<AtomicU32>, resolve_on_call: u32) -> AgentTurnFn {
        Arc::new(move |req: AgentTurnRequest| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1; // 1-indexed
            let resolved = n >= resolve_on_call;
            let reply = format!("reply-{n} from {}", req.node_id);
            Box::pin(async move { Ok(AgentTurnResult { reply, resolved }) })
        })
    }

    /// Always-succeed tool fn with a counter.
    fn tool_fn_counting(counter: Arc<AtomicU32>) -> ToolFn {
        Arc::new(move |_req: ToolCallRequest| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(serde_json::json!({"found": true})) })
        })
    }

    // -----------------------------------------------------------------------
    // Test 1: happy path — resolves on first agent pass
    // -----------------------------------------------------------------------

    /// Path: agent → lookup → router → respond
    /// Agent resolves on attempt 1 → router takes "resolved" branch → Succeed.
    #[tokio::test]
    async fn happy_path_resolves_first_pass() {
        let store = Arc::new(InMemoryCheckpointStore::default());
        let agent_count = Arc::new(AtomicU32::new(0));
        let tool_count = Arc::new(AtomicU32::new(0));

        let exec = GraphExecutor::new(
            store.clone(),
            agent_fn_resolves_on(agent_count.clone(), 1),
            tool_fn_counting(tool_count.clone()),
        );

        let outcome = exec
            .start(&tenant(), "run-happy", &triage_cfg(), "help me")
            .await
            .expect("should succeed");

        assert_eq!(outcome.status, RunStatus::Succeeded, "status");
        assert!(
            outcome.reply.contains("reply-1"),
            "reply should contain agent output: {:?}",
            outcome.reply
        );
        assert_eq!(agent_count.load(Ordering::SeqCst), 1, "agent invoked once");
        assert_eq!(tool_count.load(Ordering::SeqCst), 1, "tool invoked once");
        // Trail: agent, tool, router, respond = 4 entries
        assert_eq!(outcome.trail.len(), 4, "trail: {:?}", outcome.trail);
    }

    // -----------------------------------------------------------------------
    // Test 2: loops until router cap, then resolves
    // -----------------------------------------------------------------------

    /// triage_json has maxIterations=3.  Agent never self-resolves.
    /// After 3 iterations, router takes "resolved" branch → Succeeded.
    #[tokio::test]
    async fn loops_until_router_cap_then_resolves_via_cap() {
        let store = Arc::new(InMemoryCheckpointStore::default());
        let agent_count = Arc::new(AtomicU32::new(0));

        // resolve_on_call = u32::MAX → never resolves on its own
        let exec = GraphExecutor::new(
            store.clone(),
            agent_fn_resolves_on(agent_count.clone(), u32::MAX),
            tool_fn_counting(Arc::new(AtomicU32::new(0))),
        );

        let outcome = exec
            .start(&tenant(), "run-cap", &triage_cfg(), "loop me")
            .await
            .expect("should succeed via iteration cap");

        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(
            agent_count.load(Ordering::SeqCst),
            3,
            "agent should be invoked exactly 3 times (maxIterations=3)"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: resume on a succeeded run returns stored outcome, no re-invoke
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn resume_on_succeeded_run_returns_stored_outcome_without_reinvoking() {
        let store = Arc::new(InMemoryCheckpointStore::default());
        let agent_count = Arc::new(AtomicU32::new(0));
        let tool_count = Arc::new(AtomicU32::new(0));

        let exec = GraphExecutor::new(
            store.clone(),
            agent_fn_resolves_on(agent_count.clone(), 1),
            tool_fn_counting(tool_count.clone()),
        );

        // Drive to completion.
        exec.start(&tenant(), "run-resume-done", &triage_cfg(), "hi")
            .await
            .expect("first run succeeds");

        let after_start_agent = agent_count.load(Ordering::SeqCst);
        let after_start_tool = tool_count.load(Ordering::SeqCst);

        // resume should return Succeeded without calling agent or tool again.
        let outcome = exec
            .resume(&tenant(), "run-resume-done")
            .await
            .expect("resume should succeed");

        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(
            agent_count.load(Ordering::SeqCst),
            after_start_agent,
            "agent must NOT be called again on resume of terminal run"
        );
        assert_eq!(
            tool_count.load(Ordering::SeqCst),
            after_start_tool,
            "tool must NOT be called again on resume of terminal run"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: global visit cap fails the run
    // -----------------------------------------------------------------------

    /// Build a variant of triage with maxIterations=1000 (far above MAX_NODE_VISITS).
    /// The global cap of 64 should fire first, saving the record as Failed.
    #[tokio::test]
    async fn global_visit_cap_fails_run() {
        let store = Arc::new(InMemoryCheckpointStore::default());

        // Clone triage fixture and patch maxIterations on the router node.
        let mut v: serde_json::Value = serde_json::from_str(&triage_json()).expect("fixture JSON");
        for node in v["nodes"].as_array_mut().expect("nodes array") {
            if node["kind"] == "router" {
                node["maxIterations"] = serde_json::json!(1000);
            }
        }
        let cfg = GraphConfig::from_json(&v.to_string()).expect("patched graph valid");

        let exec = GraphExecutor::new(
            store.clone(),
            // never resolves
            agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), u32::MAX),
            tool_fn_counting(Arc::new(AtomicU32::new(0))),
        );

        let err = exec
            .start(&tenant(), "run-global-cap", &cfg, "infinite loop")
            .await
            .expect_err("should fail with IterationCap");

        assert!(
            matches!(err, GraphExecError::IterationCap { .. }),
            "expected IterationCap, got {err:?}"
        );

        // The stored record must be Failed.
        let rec = store
            .load(&tenant(), "run-global-cap")
            .await
            .expect("store accessible")
            .expect("record must exist");
        assert_eq!(
            rec.status,
            RunStatus::Failed,
            "stored status must be Failed"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: effect error leaves run resumable; replay prevents re-invoke
    // -----------------------------------------------------------------------

    /// Agent succeeds (unresolved) on attempt 1, then fails on attempt 2.
    /// start() returns Err; record stays Running.
    /// resume() with a closure that resolves on attempt 1 (= attempt 2 of the
    /// node, but attempt 1 of the fresh closure) → Succeeded.
    /// Replay must prevent attempt-1 from being re-executed.
    #[tokio::test]
    async fn effect_error_leaves_run_resumable() {
        let store = Arc::new(InMemoryCheckpointStore::default());
        let t = tenant();

        // Phase 1: agent call #1 → ok/unresolved; call #2 → error.
        let phase1_count = Arc::new(AtomicU32::new(0));
        {
            let pc = phase1_count.clone();
            let agent_phase1: AgentTurnFn = Arc::new(move |req: AgentTurnRequest| {
                let n = pc.fetch_add(1, Ordering::SeqCst) + 1;
                let node = req.node_id.clone();
                Box::pin(async move {
                    if n == 1 {
                        Ok(AgentTurnResult {
                            reply: format!("pass-{n} from {node}"),
                            resolved: false,
                        })
                    } else {
                        Err(GraphExecError::AgentTurn(
                            "simulated failure on attempt 2".into(),
                        ))
                    }
                })
            });

            let tool_count = Arc::new(AtomicU32::new(0));
            let store_ref = store.clone();
            let exec = GraphExecutor::new(
                store_ref,
                agent_phase1,
                tool_fn_counting(tool_count.clone()),
            );

            let err = exec
                .start(&t, "run-resumable", &triage_cfg(), "retry me")
                .await
                .expect_err("should fail on agent attempt 2");

            assert!(
                matches!(err, GraphExecError::AgentTurn(_)),
                "expected AgentTurn error, got {err:?}"
            );

            // Record must still be Running.
            let rec = store
                .load(&t, "run-resumable")
                .await
                .expect("store ok")
                .expect("record exists");
            assert_eq!(
                rec.status,
                RunStatus::Running,
                "run should stay Running after effect error"
            );
        }

        // Phase 2: resume with a closure that resolves on its first call
        // (= attempt 2 of the "agent" node, but the phase2 closure only sees
        // calls that were NOT replayed).
        let phase2_count = Arc::new(AtomicU32::new(0));
        let tool_phase2_count = Arc::new(AtomicU32::new(0));
        {
            let pc2 = phase2_count.clone();
            let agent_phase2: AgentTurnFn = Arc::new(move |_req: AgentTurnRequest| {
                pc2.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    Ok(AgentTurnResult {
                        reply: "resolved!".into(),
                        resolved: true,
                    })
                })
            });

            let exec2 = GraphExecutor::new(
                store.clone(),
                agent_phase2,
                tool_fn_counting(tool_phase2_count.clone()),
            );

            let outcome = exec2
                .resume(&t, "run-resumable")
                .await
                .expect("resume should succeed");

            assert_eq!(outcome.status, RunStatus::Succeeded, "outcome status");
        }

        // Attempt 1 was replayed → phase2 agent called exactly once.
        assert_eq!(
            phase2_count.load(Ordering::SeqCst),
            1,
            "phase2 agent must be called exactly once (attempt-1 was replayed)"
        );
        // Attempt 1 of the tool node was replayed (already recorded in phase 1).
        // Attempt 2 of the tool node (second loop pass) is a new invocation.
        // So phase2 tool is called exactly once — for attempt 2, not for the
        // replayed attempt 1.
        assert_eq!(
            tool_phase2_count.load(Ordering::SeqCst),
            1,
            "phase2 tool must be called once (attempt-1 replayed, attempt-2 is fresh)"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: start twice with same run id after completion → AlreadyCompleted
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn start_twice_with_same_run_id_after_completion_errors() {
        let store = Arc::new(InMemoryCheckpointStore::default());

        let exec = GraphExecutor::new(
            store.clone(),
            agent_fn_resolves_on(Arc::new(AtomicU32::new(0)), 1),
            tool_fn_counting(Arc::new(AtomicU32::new(0))),
        );

        exec.start(&tenant(), "run-dup", &triage_cfg(), "first")
            .await
            .expect("first start succeeds");

        let err = exec
            .start(&tenant(), "run-dup", &triage_cfg(), "second attempt")
            .await
            .expect_err("second start must fail");

        assert!(
            matches!(err, GraphExecError::AlreadyCompleted(_)),
            "expected AlreadyCompleted, got {err:?}"
        );
    }
}
