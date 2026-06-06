# Runtime Agent-Graph Execution (PR 1: engine) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `aw_runtime::graph` — a durable agent-graph executor (Agent/Tool/Router/Respond) with a `CheckpointStore` trait (in-memory + Redis impls) and a `DwAgentGraph` flow-node handler, so published agent-graphs execute as graphs at runtime.

**Architecture:** Port the designer's spike engine (greentic-designer branch `spike/agent-graph-engine-slice`, `src/orchestrate/agent_graph/`) into `crates/greentic-aw-runtime/src/graph/`, replacing its sqlx checkpoint layer with a `CheckpointStore` trait and its closure wiring with injectable `AgentTurnFn`/`ToolFn`. The executor orchestrates the existing `AgentRuntime::step()` per Agent node; nothing in the single-agent loop changes.

**Tech Stack:** Rust 1.95, tokio, redis (ConnectionManager, same as `state_redis.rs`), serde, thiserror. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md`

---

## Conventions for every task

- Workspace: this worktree (branch `feat/aw-graph-runtime`, repo greentic-runner).
- Port source of truth: read designer files with
  `git -C /home/bima-pangestu/Works/greentic/greentic-designer show origin/spike/agent-graph-engine-slice:src/orchestrate/agent_graph/<file>.rs`
  (fetch first if missing: `git -C /home/bima-pangestu/Works/greentic/greentic-designer fetch origin`).
- Crate conventions (MUST follow): `#![deny(unsafe_code)]`,
  `#[warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` — no
  `unwrap()`/`expect()`/`panic!()` outside `#[cfg(test)]`. Async traits use
  manual `Pin<Box<dyn Future<Output = …> + Send + 'a>>` returns (copy the
  shape from `crates/greentic-aw-runtime/src/state.rs:118-152`), NOT
  `async_trait`, inside greentic-aw-runtime. (runner-host already uses
  `async_trait` for its node handler traits — keep that there.)
- Errors: `thiserror` enums per module, mirroring `error.rs` style.
- English-only comments/tests; conventional commits.
- Test command base: `cargo test -p greentic-aw-runtime graph:: --all-features`
  (and `-p greentic-runner-host` for Task 7+).
- After EVERY task: `cargo fmt --all` and
  `cargo clippy -p <touched-crate> --all-targets --all-features -- -D warnings`
  must pass before the commit step.

---

### Task 1: `graph/model.rs` — GraphConfig, nodes, validation

**Files:**
- Create: `crates/greentic-aw-runtime/src/graph/mod.rs`
- Create: `crates/greentic-aw-runtime/src/graph/model.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (add `pub mod graph;`)

- [x] **Step 1: Read the port source**

Run: `git -C /home/bima-pangestu/Works/greentic/greentic-designer show origin/spike/agent-graph-engine-slice:src/orchestrate/agent_graph/model.rs`

The designer types to port verbatim (serde attrs included): `NodeKind`
(tagged enum, `tag = "kind"`, lowercase: `Agent { system_prompt, model,
tools }` camelCase, `Tool { tool_name }`, `Router { max_iterations }` default
4, `Respond`), `Node { id, #[serde(flatten)] kind }`, `Edge { from, to,
branch: Option<String> }`, `Graph { entry, nodes, edges }`, and the
`validate()` rules: entry exists; all edge endpoints exist; Agent/Tool have
exactly one outgoing edge; Router has both a `loop` and a `resolved` branch
edge; Respond has zero outgoing edges.

- [x] **Step 2: Write failing tests** (bottom of `model.rs`, `#[cfg(test)] mod tests`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn triage_graph_json() -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": 1,
            "entry": "agent",
            "nodes": [
                {"id": "agent", "kind": "agent", "systemPrompt": "You triage.", "model": "gpt-4o-mini", "tools": []},
                {"id": "lookup", "kind": "tool", "toolName": "kb.search"},
                {"id": "router", "kind": "router", "maxIterations": 3},
                {"id": "respond", "kind": "respond"}
            ],
            "edges": [
                {"from": "agent", "to": "lookup"},
                {"from": "lookup", "to": "router"},
                {"from": "router", "to": "agent", "branch": "loop"},
                {"from": "router", "to": "respond", "branch": "resolved"}
            ]
        })
    }

    #[test]
    fn parses_and_validates_triage_graph() {
        let cfg = GraphConfig::from_json(&triage_graph_json().to_string())
            .expect("valid graph");
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.graph.entry, "agent");
        assert_eq!(cfg.graph.nodes.len(), 4);
    }

    #[test]
    fn rejects_unknown_entry() {
        let mut v = triage_graph_json();
        v["entry"] = "missing".into();
        let err = GraphConfig::from_json(&v.to_string()).unwrap_err();
        assert!(matches!(err, GraphError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn rejects_router_without_resolved_branch() {
        let mut v = triage_graph_json();
        v["edges"].as_array_mut().unwrap().retain(|e| e["branch"] != "resolved");
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_agent_with_two_outgoing_edges() {
        let mut v = triage_graph_json();
        v["edges"].as_array_mut().unwrap()
            .push(serde_json::json!({"from": "agent", "to": "router"}));
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_respond_with_outgoing_edge() {
        let mut v = triage_graph_json();
        v["edges"].as_array_mut().unwrap()
            .push(serde_json::json!({"from": "respond", "to": "agent"}));
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut v = triage_graph_json();
        v["schemaVersion"] = 2.into();
        let err = GraphConfig::from_json(&v.to_string()).unwrap_err();
        assert!(matches!(err, GraphError::UnsupportedSchemaVersion(2)));
    }
}
```

- [x] **Step 3: Run tests to verify they fail**

Run: `cargo test -p greentic-aw-runtime graph::model --all-features`
Expected: compile error (types not defined yet) — that counts as the failing state for a new module.

- [x] **Step 4: Implement**

`mod.rs`:

```rust
//! Durable agent-graph execution: model, router, executor, checkpointing.
//! Ported from the greentic-designer engine slice (PR #436); see
//! docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md.
pub mod model;

pub use model::{Edge, Graph, GraphConfig, GraphError, Node, NodeKind};
```

`model.rs`: port the designer types verbatim, then add the envelope + error:

```rust
/// Wire envelope for a graph document (`agent-graph.json` sidecar or the
/// admin-registry graph doc). `schema_version` gates forward evolution.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(flatten)]
    pub graph: Graph,
}

fn default_schema_version() -> u32 { 1 }

pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("invalid graph: {0}")]
    Invalid(String),
    #[error("graph JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported graph schemaVersion {0}")]
    UnsupportedSchemaVersion(u32),
}

impl GraphConfig {
    pub fn from_json(raw: &str) -> Result<Self, GraphError> {
        let cfg: GraphConfig = serde_json::from_str(raw)?;
        if cfg.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(GraphError::UnsupportedSchemaVersion(cfg.schema_version));
        }
        cfg.graph.validate().map_err(GraphError::Invalid)?;
        Ok(cfg)
    }
}
```

`Graph` gets `pub fn validate(&self) -> Result<(), String>` implementing the
five rules from Step 1 (port the designer's `validate`, lines 87–140 of the
spike file). `Graph` and `Node`/`Edge`/`NodeKind` also derive `Serialize`
(the checkpoint snapshot serializes the graph back out).

- [x] **Step 5: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime graph::model --all-features`
Expected: 6 passed.

- [x] **Step 6: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy -p greentic-aw-runtime --all-targets --all-features -- -D warnings
git add crates/greentic-aw-runtime/src/graph crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw-runtime): agent-graph model with schema-versioned envelope"
```

---

### Task 2: `graph/state.rs` + `graph/router.rs`

**Files:**
- Create: `crates/greentic-aw-runtime/src/graph/state.rs`
- Create: `crates/greentic-aw-runtime/src/graph/router.rs`
- Modify: `crates/greentic-aw-runtime/src/graph/mod.rs` (add modules + re-exports)

- [x] **Step 1: Read port sources**

`git show` (as in Task 1) for `state.rs` and `router.rs` from the spike.
Designer's `TriageState` is renamed **`GraphRunState`** here:

```rust
/// Run-scoped state threaded through the executor. Append-only `messages`;
/// `resolved` is set by the agent turn; `iterations` counts router loops.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphRunState {
    pub messages: Vec<GraphMessage>,
    pub resolved: bool,
    pub iterations: u32,
    #[serde(default)]
    pub scratchpad: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphMessage {
    pub role: GraphRole, // User | Assistant | Tool
    pub content: String,
}
```

Router port (pure function, no designer deltas):

```rust
/// Resolve the Router node's outgoing edge: `resolved` when the agent set
/// the flag OR the iteration cap is reached; `loop` otherwise.
pub fn route(graph: &Graph, router_id: &str, state: &GraphRunState)
    -> Result<String, GraphError>
```
(returns the target node id; `GraphError::Invalid` if the branch edge is
missing — unreachable after validation, but never panic.)

- [x] **Step 2: Write failing tests**

```rust
// in router.rs #[cfg(test)]
#[test]
fn routes_to_loop_when_unresolved_and_under_cap() {
    let cfg = GraphConfig::from_json(&triage_json()).unwrap(); // test helper shared via graph::model::tests or a test_util fn
    let state = GraphRunState { iterations: 1, ..Default::default() };
    assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "agent");
}

#[test]
fn routes_to_resolved_when_flag_set() {
    let cfg = GraphConfig::from_json(&triage_json()).unwrap();
    let state = GraphRunState { resolved: true, ..Default::default() };
    assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "respond");
}

#[test]
fn routes_to_resolved_at_iteration_cap() {
    let cfg = GraphConfig::from_json(&triage_json()).unwrap(); // maxIterations: 3
    let state = GraphRunState { iterations: 3, ..Default::default() };
    assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "respond");
}
```

Expose the Task 1 `triage_graph_json()` fixture as
`pub(crate) fn triage_json() -> String` in a new
`crates/greentic-aw-runtime/src/graph/test_fixtures.rs` module gated
`#[cfg(test)]` so all graph tests share it (update Task 1's tests to use it).

- [x] **Step 3: Run to verify fail** — `cargo test -p greentic-aw-runtime graph::router --all-features` → compile error.

- [x] **Step 4: Implement** (port `route()` from the spike `router.rs:9-37`: look up the Router node's `max_iterations`, pick branch `"resolved"` if `state.resolved || state.iterations >= max_iterations`, else `"loop"`, then find the edge `from == router_id && branch == Some(chosen)`).

- [x] **Step 5: Run tests** — Expected: 3 passed (plus Task 1's 6 still green).

- [x] **Step 6: fmt + clippy + commit** — `git commit -m "feat(aw-runtime): graph run state and router branching"`

---

### Task 3: `graph/checkpoint.rs` — trait, record, in-memory store

**Files:**
- Create: `crates/greentic-aw-runtime/src/graph/checkpoint.rs`
- Modify: `crates/greentic-aw-runtime/src/graph/mod.rs`

- [x] **Step 1: Write the types + trait** (new code, not a port — the designer used raw sqlx):

```rust
use std::future::Future;
use std::pin::Pin;

use crate::tenant::TenantContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus { Running, Succeeded, Failed }

/// Durable snapshot of one graph run. `graph_json` is immutable for the
/// run's lifetime (snapshot taken at creation — republishing a graph never
/// mutates an in-flight run).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphRunRecord {
    pub run_id: String,
    pub graph_json: String,
    pub cursor: String,
    pub state_json: String,
    pub status: RunStatus,
    pub visits_json: String, // HashMap<String, u32> per node attempts
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("checkpoint backend error: {0}")]
    Backend(String),
    #[error("checkpoint serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Outcome of `record_node_visit`: `Recorded` = first write won;
/// `Replayed(value)` = an identical attempt already had a result.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeVisitOutcome { Recorded, Replayed(serde_json::Value) }

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait CheckpointStore: Send + Sync {
    fn load<'a>(&'a self, tenant: &'a TenantContext, run_id: &'a str)
        -> BoxFut<'a, Result<Option<GraphRunRecord>, CheckpointError>>;
    fn save<'a>(&'a self, tenant: &'a TenantContext, rec: &'a GraphRunRecord)
        -> BoxFut<'a, Result<(), CheckpointError>>;
    /// Insert-if-absent keyed by (run, node, attempt).
    fn record_node_visit<'a>(&'a self, tenant: &'a TenantContext, run_id: &'a str,
        node_id: &'a str, attempt: u32, result: &'a serde_json::Value)
        -> BoxFut<'a, Result<NodeVisitOutcome, CheckpointError>>;
    fn load_node_visit<'a>(&'a self, tenant: &'a TenantContext, run_id: &'a str,
        node_id: &'a str, attempt: u32)
        -> BoxFut<'a, Result<Option<serde_json::Value>, CheckpointError>>;
}
```

`InMemoryCheckpointStore`: `Mutex<HashMap<String, GraphRunRecord>>` +
`Mutex<HashMap<String, serde_json::Value>>` keyed
`format!("{}:{}:{}:{}:{}", tenant.tenant_id, tenant.env_id, run_id, node_id, attempt)`
(runs keyed without node/attempt). Public — the designer swap and host tests
both use it (not behind `test-mock`).

- [x] **Step 2: Write failing tests**

```rust
#[tokio::test]
async fn record_node_visit_is_insert_if_absent() {
    let store = InMemoryCheckpointStore::default();
    let t = TenantContext::new("t1", "dev");
    let first = store.record_node_visit(&t, "r1", "agent", 1, &serde_json::json!({"reply": "a"})).await.unwrap();
    assert_eq!(first, NodeVisitOutcome::Recorded);
    let second = store.record_node_visit(&t, "r1", "agent", 1, &serde_json::json!({"reply": "DIFFERENT"})).await.unwrap();
    assert_eq!(second, NodeVisitOutcome::Replayed(serde_json::json!({"reply": "a"})));
}

#[tokio::test]
async fn save_then_load_round_trips() {
    let store = InMemoryCheckpointStore::default();
    let t = TenantContext::new("t1", "dev");
    assert!(store.load(&t, "r1").await.unwrap().is_none());
    let rec = GraphRunRecord { run_id: "r1".into(), graph_json: "{}".into(),
        cursor: "agent".into(), state_json: "{}".into(),
        status: RunStatus::Running, visits_json: "{}".into() };
    store.save(&t, &rec).await.unwrap();
    let loaded = store.load(&t, "r1").await.unwrap().unwrap();
    assert_eq!(loaded.cursor, "agent");
    assert_eq!(loaded.status, RunStatus::Running);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let store = InMemoryCheckpointStore::default();
    let t1 = TenantContext::new("t1", "dev");
    let t2 = TenantContext::new("t2", "dev");
    let rec = GraphRunRecord { run_id: "r1".into(), graph_json: "{}".into(),
        cursor: "agent".into(), state_json: "{}".into(),
        status: RunStatus::Running, visits_json: "{}".into() };
    store.save(&t1, &rec).await.unwrap();
    assert!(store.load(&t2, "r1").await.unwrap().is_none());
}
```

(Check `TenantContext`'s actual constructor in
`crates/greentic-aw-runtime/src/tenant.rs` and use it — if it's
`TenantContext { tenant_id, env_id }` public fields or a different `new`
signature, adjust the tests to the real API.)

- [x] **Step 3: Run to verify fail** → compile error.
- [x] **Step 4: Implement the in-memory store.**
- [x] **Step 5: Run tests** — Expected: 3 passed.
- [x] **Step 6: fmt + clippy + commit** — `git commit -m "feat(aw-runtime): graph checkpoint trait with in-memory store"`

---

### Task 4: `graph/executor.rs` — the drive loop

**Files:**
- Create: `crates/greentic-aw-runtime/src/graph/executor.rs`
- Modify: `crates/greentic-aw-runtime/src/graph/mod.rs`

- [x] **Step 1: Read the port source** — spike `executor.rs` (drive loop, lines 129–348). Deltas from the designer version:
  - Checkpoint calls go through `Arc<dyn CheckpointStore>` (designer: sqlx).
  - Effects are injected closures:

```rust
/// One agent turn: (node_id, agent system prompt/model/tools, current state)
/// → reply + resolved flag. The host wires this to `AgentRuntime::step`.
pub type AgentTurnFn = Arc<dyn Fn(AgentTurnRequest) -> BoxFut<'static, Result<AgentTurnResult, GraphExecError>> + Send + Sync>;
pub type ToolFn = Arc<dyn Fn(ToolCallRequest) -> BoxFut<'static, Result<serde_json::Value, GraphExecError>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct AgentTurnRequest {
    pub node_id: String,
    pub system_prompt: String,
    pub model: String,
    pub state: GraphRunState,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentTurnResult { pub reply: String, pub resolved: bool }

#[derive(Debug, Clone)]
pub struct ToolCallRequest { pub node_id: String, pub tool_name: String, pub state: GraphRunState }
```

  - `GraphExecutor::new(store: Arc<dyn CheckpointStore>, agent_turn: AgentTurnFn, tool: ToolFn)`.
  - Entry points:

```rust
pub async fn start(&self, tenant: &TenantContext, run_id: &str, cfg: &GraphConfig, user_text: &str) -> Result<GraphRunOutcome, GraphExecError>;
pub async fn resume(&self, tenant: &TenantContext, run_id: &str) -> Result<GraphRunOutcome, GraphExecError>;

#[derive(Debug, Clone)]
pub struct GraphRunOutcome {
    pub status: RunStatus,
    pub reply: String,                 // last assistant message (Respond)
    pub trail: Vec<serde_json::Value>, // one entry per node visit
}
```

  - Loop semantics (identical to spike): per iteration up to
    `MAX_NODE_VISITS = 64`: Agent node → attempt = visits+1 → `load_node_visit`
    → replay or invoke `agent_turn` → `record_node_visit` BEFORE the
    checkpoint `save` → append assistant message, set `resolved`, bump
    `iterations` ONLY when router loops back (KEEP the spike's exact
    placement: the spike increments `iterations` in the agent visit — match
    the spike, verify against the source); Tool node mirrors with Role::Tool
    message; Router calls `route()`; Respond saves `Succeeded` and returns.
    Cap exhaustion saves `Failed` and returns
    `GraphExecError::IterationCap { run_id }` — the caller maps it to a
    degraded reply, the executor still persists the terminal record first.

```rust
#[derive(Debug, thiserror::Error)]
pub enum GraphExecError {
    #[error("graph run {run_id} exceeded the node-visit cap")]
    IterationCap { run_id: String },
    #[error("unknown node `{0}` (cursor corrupt or graph changed)")]
    UnknownNode(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    #[error("agent turn failed: {0}")]
    AgentTurn(String),
    #[error("tool call failed: {0}")]
    Tool(String),
}
```

- [x] **Step 2: Write failing tests** (in `executor.rs` tests mod; use the shared triage fixture, `InMemoryCheckpointStore`, and counting mock closures):

```rust
fn mock_turns(resolve_on_attempt: u32) -> (AgentTurnFn, Arc<AtomicU32>) {
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    let f: AgentTurnFn = Arc::new(move |req| {
        let n = c.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            Ok(AgentTurnResult { reply: format!("turn {n} for {}", req.node_id),
                                 resolved: n >= resolve_on_attempt })
        })
    });
    (f, calls)
}

fn mock_tool() -> (ToolFn, Arc<AtomicU32>) { /* same shape, returns json!({"ok": true}) */ }

#[tokio::test]
async fn happy_path_resolves_first_pass() {
    // agent resolves on attempt 1 → agent → lookup → router → respond
    let store = Arc::new(InMemoryCheckpointStore::default());
    let (turn, turn_calls) = mock_turns(1);
    let (tool, tool_calls) = mock_tool();
    let exec = GraphExecutor::new(store.clone(), turn, tool);
    let t = TenantContext::new("t1", "dev");
    let cfg = GraphConfig::from_json(&triage_json()).unwrap();
    let out = exec.start(&t, "run-1", &cfg, "help me").await.unwrap();
    assert_eq!(out.status, RunStatus::Succeeded);
    assert_eq!(turn_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert!(out.reply.contains("turn 1"));
}

#[tokio::test]
async fn loops_until_router_cap_then_resolves_via_cap() {
    // agent NEVER resolves; router maxIterations=3 forces the resolved branch
    let (turn, turn_calls) = mock_turns(u32::MAX);
    /* ... start ... */
    let out = exec.start(&t, "run-2", &cfg, "hi").await.unwrap();
    assert_eq!(out.status, RunStatus::Succeeded); // cap routes to respond, run still succeeds
    assert_eq!(turn_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn resume_replays_completed_visits_without_reinvoking() {
    // Run to completion with store A retained; then resume the SAME run_id:
    // resume() on a succeeded run returns the stored outcome and the
    // mock counters do not advance.
    let out2 = exec.resume(&t, "run-1").await.unwrap();
    assert_eq!(out2.status, RunStatus::Succeeded);
    assert_eq!(turn_calls.load(Ordering::SeqCst), prev_count);
}

#[tokio::test]
async fn global_visit_cap_fails_run() {
    // Pathological graph: router with huge maxIterations (e.g. 1000) loops
    // forever → MAX_NODE_VISITS trips → Failed status persisted + IterationCap error.
    let err = exec.start(&t, "run-3", &cfg_with_max_iterations_1000, "hi").await.unwrap_err();
    assert!(matches!(err, GraphExecError::IterationCap { .. }));
    let rec = store.load(&t, "run-3").await.unwrap().unwrap();
    assert_eq!(rec.status, RunStatus::Failed);
}
```

- [x] **Step 3: Run to verify fail.**
- [x] **Step 4: Port + implement** per Step 1 deltas. Keep functions small: `drive()` (the loop), `visit_agent()`, `visit_tool()`, `checkpoint()` helpers.
- [x] **Step 5: Run tests** — Expected: 4 new passed, all earlier graph tests green.
- [x] **Step 6: fmt + clippy + commit** — `git commit -m "feat(aw-runtime): durable graph executor with replay-before-invoke"`

---

### Task 5: Crash-resume integration test

**Files:**
- Create: `crates/greentic-aw-runtime/tests/graph_crash_resume.rs`

- [x] **Step 1: Write the test** (failing only if Task 4 got resume wrong — this is the guarantee test):

```rust
//! Simulates a crash mid-run: the first executor drives until the tool node
//! has recorded its visit, then is dropped. A second executor resumes the
//! same run_id against the same store and must finish WITHOUT re-invoking
//! the completed agent/tool effects.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
// ... imports of graph types as exported from greentic_aw_runtime::graph

#[tokio::test]
async fn resume_after_crash_does_not_duplicate_side_effects() {
    let store = Arc::new(InMemoryCheckpointStore::default());
    let t = TenantContext::new("t1", "dev");
    let cfg = GraphConfig::from_json(&fixture_json()).unwrap();

    // Crash simulation: a ToolFn whose FIRST invocation succeeds (and is
    // recorded by the executor), but we abort the drive immediately after by
    // making the SECOND effect (the next agent turn) return an error.
    // The run is now mid-flight: status Running, cursor on the router/agent.
    /* drive 1: agent ok (attempt 1), tool ok (attempt 1), agent attempt 2 -> Err */
    let r1 = exec1.start(&t, "run-x", &cfg, "hello").await;
    assert!(r1.is_err(), "first drive aborts mid-run");
    let mid = store.load(&t, "run-x").await.unwrap().unwrap();
    assert_eq!(mid.status, RunStatus::Running);

    // drive 2: fresh executor, same store; agent turn now resolves.
    // Completed visits (agent attempt 1, tool attempt 1) must be REPLAYED:
    let out = exec2.resume(&t, "run-x").await.unwrap();
    assert_eq!(out.status, RunStatus::Succeeded);
    assert_eq!(agent_calls_exec2.load(Ordering::SeqCst), 1, "only the failed attempt re-runs");
    assert_eq!(tool_calls_exec2.load(Ordering::SeqCst), 0, "tool visit replayed from ledger");
}
```

(The executor's error path must persist the `Running` checkpoint BEFORE
propagating an effect error — assert that explicitly; if Task 4's
implementation saves only after successful visits, fix Task 4: the
checkpoint after the LAST SUCCESSFUL visit is the resume point.)

- [x] **Step 2: Run** — `cargo test -p greentic-aw-runtime --test graph_crash_resume --all-features` Expected: PASS (or fix executor until it does).
- [x] **Step 3: fmt + clippy + commit** — `git commit -m "test(aw-runtime): crash-resume guarantee for graph runs"`

---

### Task 6: `graph/redis_checkpoint.rs`

**Files:**
- Create: `crates/greentic-aw-runtime/src/graph/redis_checkpoint.rs`
- Modify: `crates/greentic-aw-runtime/src/graph/mod.rs`

- [x] **Step 1: Read the conventions source** — `crates/greentic-aw-runtime/src/state_redis.rs` (ConnectionManager wrapper, `STATE_TTL_SECS = 7*24*60*60`, key builders, error mapping into `StateError` — mirror the structure, mapping into `CheckpointError::Backend`).

- [x] **Step 2: Implement `RedisCheckpointStore`**

```rust
pub struct RedisCheckpointStore { manager: ConnectionManager }

const GRAPH_TTL_SECS: u64 = 7 * 24 * 60 * 60; // matches conversation state

fn run_key(t: &TenantContext, run_id: &str) -> String {
    format!("aw:{}:{}:{}:graph", t.tenant_id, t.env_id, run_id)
}
fn visit_key(t: &TenantContext, run_id: &str, node_id: &str, attempt: u32) -> String {
    format!("aw:{}:{}:{}:graph:visit:{}:{}", t.tenant_id, t.env_id, run_id, node_id, attempt)
}
```

- `load`/`save`: GET/SET the `GraphRunRecord` as JSON with `EX GRAPH_TTL_SECS` (save refreshes TTL).
- `record_node_visit`: `SET key value NX EX GRAPH_TTL_SECS`; when NX reports
  not-set, `GET` the existing value and return `Replayed(existing)`.
- `load_node_visit`: GET → Option.
- (Adjust field access if `TenantContext` exposes accessors instead of public
  fields — copy whatever `state_redis.rs`'s key builders do.)

- [x] **Step 3: Write the gated integration test** (same pattern as existing Redis tests in this crate — find one with `grep -rn "REDIS_URL" crates/greentic-aw-runtime/` and mirror its skip mechanism):

```rust
#[tokio::test]
async fn redis_checkpoint_round_trip_and_nx_semantics() {
    let Some(url) = std::env::var("REDIS_URL").ok() else { eprintln!("skipped: no REDIS_URL"); return };
    // connect, then: save/load round-trip; record_node_visit twice →
    // Recorded then Replayed(first value); tenant isolation via distinct env_id.
}
```

- [x] **Step 4: Run** — `cargo test -p greentic-aw-runtime graph::redis --all-features` (passes trivially without REDIS_URL; run with a local Redis if available: `REDIS_URL=redis://127.0.0.1:6379 cargo test ...`).
- [x] **Step 5: fmt + clippy + commit** — `git commit -m "feat(aw-runtime): redis-backed graph checkpoint store"`

---### Task 7: `DwAgentGraph` node handler (runner-host)

**Files:**
- Create: `crates/greentic-runner-host/src/runner/graph_node.rs`
- Modify: `crates/greentic-runner-host/src/runner/mod.rs` (module decl — find where `agent_node` is declared and mirror)

- [x] **Step 1: Read the mirror source** — `crates/greentic-runner-host/src/runner/agent_node.rs` IN FULL (trait at top, `aw` module gated `#[cfg(feature = "agentic-worker")]`, `HostConfigProvider`, runtime construction, env wiring, tests at bottom). The graph handler mirrors its shape exactly.

- [x] **Step 2: Write the trait + failing contract test**

```rust
/// Bridges a `DwAgentGraph` flow node into the graph executor.
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
```

Contract tests (gated `#[cfg(all(test, feature = "agentic-worker"))]`, using
`InMemoryCheckpointStore`, an `InMemoryGraphProvider` (Step 3) and mock
`AgentTurnFn`/`ToolFn` — NO real AgentRuntime in unit tests):

```rust
#[tokio::test]
async fn executes_graph_and_returns_dw_agent_envelope() {
    let h = test_handler(/* graph registered under "triage" */);
    let out = h.execute("t1", "dev", "triage", "sess-1", &json!({"user_text": "hi"})).await.unwrap();
    assert!(out["reply"].is_string());
    assert!(out["trail"].is_array());
    assert_eq!(out["terminated_by"], "respond");
}

#[tokio::test]
async fn same_session_resumes_completed_run_with_fresh_run() {
    // second call on same session/graph starts run with suffix `:2`
    // (completed-run count), does not error.
}

#[tokio::test]
async fn missing_graph_returns_structured_error_reply() {
    let out = h.execute("t1", "dev", "nope", "sess-1", &json!({"user_text": "hi"})).await.unwrap();
    assert_eq!(out["terminated_by"], "error");
    assert!(out["reply"].as_str().unwrap().contains("not available"));
}

#[tokio::test]
async fn missing_user_text_is_input_error() { /* mirrors agent_node behavior — check its test for exact contract */ }
```

- [x] **Step 3: Implement**

- `GraphConfigSource` trait (in `graph_node.rs`’s `aw` module):
  `fn graph_config<'a>(&'a self, tenant: &'a TenantContext, graph_id: &'a str) -> BoxFut<'a, Result<GraphConfig, ConfigError>>;`
  with `InMemoryGraphProvider` (a `HashMap<String, GraphConfig>` — the pack
  loader fills it, Task 8).
- `RuntimeGraphNodeHandler { executor_parts }`: builds run_id
  `format!("{session_id}__{graph_id}")` (double underscore, NOT `:` — colon
  is the checkpoint key-segment separator and must not appear inside a
  run_id; see checkpoint.rs segment constraints); on `load` returning a
  `succeeded|failed` record, retries with `__{n}` suffix (n = 2, 3, …) until
  an absent or `running` record is found (bounded at 100 → structured error);
  `running` → `resume`, absent → `start`.
- AgentTurnFn wiring to the real `AgentRuntime` lives in a constructor
  `RuntimeGraphNodeHandler::from_runtime(runtime: Arc<AgentRuntime>, store, provider)`:
  the closure registers the node's `{system_prompt, model}` into an
  `InMemoryConfigProvider` under agent id `format!("{graph_id}.{node_id}")`
  and calls `runtime.step(...)` with a per-visit session id
  `format!("{session_id}:{graph_id}:{node_id}")` so node turns don't
  cross-contaminate conversation state; `resolved` is parsed from a trailing
  `[[RESOLVED]]` sentinel in the reply (the spike's convention — strip it
  from the user-visible reply).
- ToolFn wiring: dispatch through `greentic_ext_runtime::ExtensionRuntime`
  the same way `agent_node.rs`'s loop does (find its tool dispatch and reuse;
  tool_name parses as `extension_id/tool_name`, reject other shapes with
  `GraphExecError::Tool`).
- Session lock (spec §2): before starting/resuming the run, acquire
  `AgentStateStore::acquire_lock(tenant, session_id, wait)` (the same store +
  wait the DwAgent handler uses — read its call site) and hold the guard for
  the whole drive; concurrent graph runs on one session must serialize. The
  in-memory unit tests use the mock state store's lock (or a no-op lock
  injected via the same seam `agent_node.rs` tests use — mirror them).
- Errors NEVER propagate as Err from `execute` for runtime failures — they
  map to the degraded envelope `{"reply": <sanitised>, "trail": [...],
  "terminated_by": "error"}` exactly like `agent_node.rs` does (read its
  error mapping and copy the approach).

- [x] **Step 4: Run** — `cargo test -p greentic-runner-host graph_node --features agentic-worker` Expected: 4 passed.
- [x] **Step 5: Compile check without the feature** — `cargo check -p greentic-runner-host --no-default-features --features verify` Expected: clean (trait compiles, aw module absent).
- [x] **Step 6: fmt + clippy + commit** — `git commit -m "feat(runner-host): DwAgentGraph node handler over the graph executor"`

---

### Task 8: Pack sidecar loader + engine wiring

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/graph_node.rs` (loader fn)
- Modify: the engine/host-config seam — find it: `grep -rn "agent_configs_from_manifest\|HostConfig" crates/greentic-runner-host/src/ | grep -v test` and mirror how `agents` flow from `HostConfig` into the `AgentNodeHandler` construction.

- [x] **Step 1: Write the loader + failing tests** (in `graph_node.rs`):

```rust
/// Parse `agent-graph.json` sidecar bytes (from a .gtpack) into a GraphConfig.
/// Malformed sidecars are logged and skipped — a bad graph never prevents
/// the rest of the pack from loading (mirrors agent_configs_from_manifest).
pub fn graph_config_from_sidecar(pack_id: &str, bytes: &[u8]) -> Option<GraphConfig>
```

Tests: valid sidecar parses (reuse the triage fixture JSON); malformed JSON →
`None` (and does not panic); wrong schemaVersion → `None`.

- [x] **Step 2: Wire into the host** — add `graphs: HashMap<String, GraphConfig>` alongside wherever `HostConfig.agents` lives (feature-gated identically), populate from pack loading where agent manifests are read (follow the `agents` data path found in Step 1's grep; greentic-start consumes this seam — its env-file variant is OUT of this PR's scope), and pass into `InMemoryGraphProvider` where the runtime handler is constructed. The flow engine's node-kind dispatch for `dw.agent_graph` mirrors `dw.agent`: `grep -rn "dw.agent\|DwAgent" crates/greentic-runner-host/src/runner/engine.rs` and add the graph variant with the same session-id derivation.

- [x] **Step 3: Run the full crate tests** — `cargo test -p greentic-runner-host --all-features` Expected: all green.
- [x] **Step 4: fmt + clippy + commit** — `git commit -m "feat(runner-host): load agent-graph sidecars and route dw.agent_graph nodes"`

---

### Task 9: Exports, feature matrix, local CI, docs

**Files:**
- Modify: `crates/greentic-aw-runtime/src/graph/mod.rs` (final re-export list: `GraphConfig, Graph, GraphError, GraphRunState, GraphExecutor, GraphExecError, GraphRunOutcome, CheckpointStore, GraphRunRecord, RunStatus, NodeVisitOutcome, InMemoryCheckpointStore, RedisCheckpointStore, AgentTurnFn, ToolFn, AgentTurnRequest, AgentTurnResult, ToolCallRequest`)
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (`pub use graph::...` if the crate re-exports at root — match how `state`/`tools` are re-exported)
- Modify: `docs/agentic-worker-tools.md` or the crate-level doc comment — one paragraph pointing at the graph module + spec.

- [x] **Step 1: Feature matrix compile checks**

```bash
cargo check -p greentic-aw-runtime --all-features
cargo check -p greentic-runner-host --no-default-features --features verify
cargo check -p greentic-runner-host --all-features
```
Expected: all clean.

- [x] **Step 2: Full local CI** — `bash ci/local_check.sh` (from repo root). Expected: fmt + clippy + tests green. If failures are OUTSIDE graph/* scope, record them for the PR description; do not paper over.

- [x] **Step 3: Commit** — `git commit -m "chore(aw-runtime): export graph module surface + docs pointer"`

---

### Task 10: Push + PR

- [x] **Step 1: Pre-push verification** — `git log --oneline origin/research..HEAD` (expect the spec + ~8 implementation commits), re-run `bash ci/local_check.sh` one final time.
- [x] **Step 2: Push** — `git push -u origin feat/aw-graph-runtime`
- [x] **Step 3: PR to research**

```bash
gh pr create --repo greenticai/greentic-runner --base research \
  --title "feat: runtime agent-graph execution (engine + Redis checkpoints + DwAgentGraph node)" \
  --body "<summary: spec link, what's in scope (PR 1 of 4 per spec delivery plan), test evidence incl. crash-resume, feature-matrix checks, known follow-ups (admin/store PRs 2-4)>"
```
No Claude attribution trailers. After creation, verify `gh pr view --json headRefOid` matches `git rev-parse HEAD`.
