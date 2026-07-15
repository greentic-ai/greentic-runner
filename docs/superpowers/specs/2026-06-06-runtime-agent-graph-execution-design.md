# Runtime Agent-Graph Execution — Design

- **Date:** 2026-06-06
- **Status:** Approved
- **Scope:** Execute the visual agent-graph (`{entry, nodes, edges}`, authored in
  the designer's `/agent-graph` canvas, designer PR #436) **at runtime**, in
  both delivery paths: local pack (`agent-graph.json` sidecar in a `.gtpack`)
  and hosted (admin-registry graph doc, populated by store run-from-store).
  Today a published graph runs as a flattened single-agent worker; the graph
  is archival.
- **Companion sources:**
  - Designer engine slice (port source): `greentic-designer`
    `src/orchestrate/agent_graph/` on `spike/agent-graph-engine-slice`
    (PR #436).
  - This crate's existing single-agent loop: `crates/greentic-aw-runtime`.
  - Store hand-off: `greentic-store-server`
    `crates/greentic-store-api/src/handlers/agentic_workers/run.rs`.

## Goals

1. A published agent-graph worker executes its actual graph (Agent → Tool →
   Router loop → Respond) at runtime — not the first-Agent-node projection.
2. Both delivery paths work: `gtc start` bundles (sidecar) and run-from-store
   (admin registry).
3. Durable execution: a crashed run resumes from its checkpoint without
   re-invoking completed side effects (same guarantee the designer slice
   gives, ported to a Redis store).
4. One engine, two hosts: the executor lives in `aw_runtime::graph` so the
   designer can later replace its in-repo copy (follow-up, not this work).

## Non-goals

- Multi-agent / supervisor node kinds (W4 — the model stays
  Agent/Tool/Router/Respond with the same validation rules as #436).
- Designer-side swap to the shared module (follow-up bundled with W4).
- Signature verification of sidecars (inherits the pack pipeline's posture).
- New memory backends (W6).

## Architecture

```
.gtpack (agent-graph.json sidecar)──┐
                                    ├─→ GraphConfig ─→ aw_runtime::graph::GraphExecutor
admin registry (graph doc, NEW) ────┘         │
                                              ├─ Agent node  → AgentRuntime::step()  (existing)
                                              ├─ Tool node   → ExtensionRuntime + tool ledger (existing)
                                              ├─ Router      → pure branch logic (ported)
                                              ├─ Respond     → terminal reply
                                              └─ CheckpointStore trait
                                                   ├─ RedisCheckpointStore (runner, NEW)
                                                   └─ (designer keeps its SQLite impl until the swap)
```

The graph executor **orchestrates** the existing single-agent runtime; it does
not replace it. Each Agent node turn goes through `AgentRuntime::step()` (or
`step_with_observer` for streaming), so conversation state, session locks,
LLM retry/backoff, the extension LLM bridge, and the tool idempotency ledger
are reused untouched.

## Components

### 1. `aw_runtime::graph` module (this repo, `crates/greentic-aw-runtime/src/graph/`)

Ported from the designer slice with these deltas:

- **`model.rs`** — `Graph { entry, nodes, edges }`, `NodeKind::{Agent, Tool,
  Router, Respond}`, `Edge { from, to, branch }` plus a `schema_version: u32`
  envelope (`GraphConfig`) so the wire format can evolve. Validation rules are
  identical to #436: entry exists, edge endpoints exist, Agent/Tool have
  exactly one outgoing edge, Router has both `loop` and `resolved` branches,
  Respond has none.
- **`router.rs`** — unchanged port: `resolved || iterations >= max_iterations`
  → `resolved` branch, else `loop`.
- **`state.rs`** — `GraphRunState` (rename of `TriageState`): append-only
  `messages`, `resolved` flag, `iterations`, free-form `scratchpad`.
- **`executor.rs`** — the drive loop with `cursor` + per-node `visits`
  counters, `MAX_NODE_VISITS = 64` global cap, replay-before-invoke on
  Agent/Tool nodes, checkpoint write after every step. Effect closures are
  injected (`AgentTurnFn`, `ToolFn`) so the executor stays host-agnostic.
- **`checkpoint.rs`** — NEW trait in place of the designer's direct sqlx:

  ```rust
  pub trait CheckpointStore: Send + Sync {
      async fn load(&self, tenant: &TenantContext, run_id: &str)
          -> Result<Option<GraphRunRecord>, CheckpointError>;
      async fn save(&self, tenant: &TenantContext, rec: &GraphRunRecord)
          -> Result<(), CheckpointError>;
      /// Insert-if-absent; returns the stored value when already present.
      async fn record_node_visit(&self, tenant: &TenantContext, run_id: &str,
          node_id: &str, attempt: u32, result: &serde_json::Value)
          -> Result<NodeVisitOutcome, CheckpointError>;
      async fn load_node_visit(&self, tenant: &TenantContext, run_id: &str,
          node_id: &str, attempt: u32)
          -> Result<Option<serde_json::Value>, CheckpointError>;
  }
  ```

  (Signatures shown `async fn` for readability; the implementation follows the
  crate's `AgentStateStore` convention of manual
  `Pin<Box<dyn Future<...> + Send>>` returns.)

  `GraphRunRecord` mirrors the designer's `RunRecord`: `run_id`, `graph_json`
  (immutable snapshot taken at run creation — a republished graph never
  mutates an in-flight run), `cursor`, `state_json`, `status`
  (`running|succeeded|failed`), `visits_json`.

- **`redis_checkpoint.rs`** — production impl following the
  `state_redis.rs` conventions: keys
  `aw:{tenant}:{env}:{run_id}:graph` (run record, TTL 7 days, refreshed on
  save) and `aw:{tenant}:{env}:{run_id}:graph:visit:{node}:{attempt}`
  (`SET NX` for insert-if-absent + `GET` fallback, same TTL). An
  `InMemoryCheckpointStore` ships for tests.

### 2. `DwAgentGraph` node handler (`crates/greentic-runner-host/src/runner/graph_node.rs`)

Mirrors `agent_node.rs`, gated by the same `agentic-worker` feature:

- Flow contract: input `{"user_text": ...}` (+ node config `graph_id`), output
  `{"reply", "trail", "terminated_by"}` — identical envelope to `DwAgent` so
  flow tooling needs no new shape.
- `run_id` derives deterministically from `{session_id}:{graph_id}` — a flow
  session that re-enters the node resumes its checkpointed run; a `succeeded`
  / `failed` record starts a fresh run (`run_id` gains the completed-run
  count as suffix).
- Session lock: the existing `AgentStateStore::acquire_lock` on the session
  guards the whole graph step, preventing concurrent runs on one session.
- Streaming: per-Agent-node turns pass through the existing `StepObserver`
  opt-in; Router/Tool nodes emit no tokens.

### 3. Graph config resolution (both delivery paths)

Resolution layers mirror the agent-config chain (`LayeredConfigProvider`):

1. **Pack path (local / `gtc start`):** the pack loader reads
   `agent-graph.json` from the `.gtpack` and registers a `GraphConfig` under
   the worker's id in an in-memory `HostGraphProvider` (sibling of
   `HostConfigProvider`).
2. **Admin path (hosted):** `HttpGraphConfigProvider` fetches
   `GET {GREENTIC_AW_ADMIN_ENDPOINT}/api/v1/designer/agent-graphs/{graph_id}`
   with `GREENTIC_AW_ADMIN_TOKEN` — same env vars, timeout (10s), error
   taxonomy (401/403 → Misconfigured, 404 → NotFound) and 60s cache TTL as
   the agent-config HTTP provider.

Missing graph everywhere → the node fails soft with a structured error reply
(flow continues), mirroring `DwAgent`'s missing-agent behavior.

### 4. greentic-designer-admin (separate PR)

- Migration: `agent_graphs` table (`tenant_id`, `graph_id`, `version`,
  `graph_json`, timestamps; last-writer-wins upsert like the agents
  registry).
- Endpoints under the existing designer-agents auth (tenant `gtc_live_*`
  bearer): `PUT /api/v1/designer/agent-graphs/{graph_id}` and
  `GET /api/v1/designer/agent-graphs/{graph_id}`.

### 5. greentic-store-server run hand-off (separate PR)

`run.rs`: when the published pack contains `agent-graph.json`, the hand-off
additionally `PUT`s the graph doc to the admin registry (namespaced
`{worker-name}.{graph-id}` like agents) and the run response gains
`graphs: [{graph_id, admin_version}]` alongside `agents[]`. Workers without a
sidecar behave exactly as today.

## Data flow (hosted, end to end)

1. Designer publishes graph → `.gtpack` carries `agent-graph.json` (#436,
   already implemented).
2. Store `POST .../run` → registers agent configs **and** the graph doc into
   the `store-runs` tenant.
3. Runner flow hits a `DwAgentGraph` node → `HttpGraphConfigProvider` fetches
   the doc → `GraphExecutor` drives it: Agent turns via `AgentRuntime::step`,
   Tool dispatch via `ExtensionRuntime`, checkpoints to Redis each step.
4. Respond node → reply returned to the flow; run record marked `succeeded`.

## Error handling

- **Caps:** `Router.max_iterations` (per-graph, default 4) + global
  `MAX_NODE_VISITS = 64` → run marked `failed`, node returns a structured
  degraded reply. No `unwrap()`/`panic!()` anywhere in the path.
- **At-least-once effects:** node effect runs, then
  `record_node_visit` (SET NX); crash between effect and record re-executes
  the effect on resume — same documented window as #436. Tool calls keep the
  inner idempotency ledger as a second layer.
- **Checkpoint store unavailable:** run fails soft with the degraded reply;
  the session lock prevents half-state interleaving.
- **Graph fetch failures:** layered fallback; `Misconfigured` (bad token)
  propagates without fallback, matching agent-config semantics.

## Testing

- **Unit:** model validation table tests; router branch matrix; executor
  cursor/replay against mock `AgentTurnFn`/`ToolFn` + `InMemoryCheckpointStore`
  (happy path, loop-until-cap, resume-mid-run, replay-skips-effect).
- **Crash-resume integration:** drive N steps, drop the executor, rebuild
  from the store, assert no duplicate effect invocations and identical final
  transcript.
- **Redis impl:** integration test behind the repo's existing Redis test
  pattern (skip when no `REDIS_URL`).
- **Handler:** `DwAgentGraph` contract test mirroring `agent_node.rs` tests;
  feature-gate compile checks (`--no-default-features`).
- **E2E (post-merge, gated on #436):** Triage Agent graph from #436 as the
  canonical fixture through `gtc start` with a sidecar pack;
  `scripts/smoke-agent-deploy.sh` (greentic-start) extended for the registry
  path.

## Delivery (PR sequence, all `research` lines)

1. **greentic-runner** — `aw_runtime::graph` + Redis checkpoint store +
   `DwAgentGraph` handler + pack sidecar loader (this spec's home repo).
2. **greentic-designer-admin** — graph-doc storage + PUT/GET endpoints.
3. **greentic-store-server** — run hand-off graph registration.
4. **greentic-runner** — `HttpGraphConfigProvider` + layered wiring (kept
   separate so PR 1 stays reviewable; can fold into PR 1 if small).

Gates: designer PR #436 must merge for E2E (sidecar publishing); runner work
proceeds against the sidecar format regardless. The greentic-start runner pin
bump to pick all this up is a follow-up chore PR (same mechanism as the
`74dff3f` pin).

## Follow-ups (out of scope)

- Designer swaps its in-repo engine for `aw_runtime::graph` (bundle with W4).
- W4: additional node kinds / multi-agent (supervisor, parallel branches).
- Run inspection/ops surface (list/cancel in-flight graph runs).
- Checkpoint TTL vs long-lived runs policy (7d matches conversation state).
