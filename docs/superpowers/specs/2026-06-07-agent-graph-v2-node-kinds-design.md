# Agent-Graph v2 Node Kinds (multi-agent, supervisor, parallel) — Design

- **Date:** 2026-06-07
- **Status:** Approved
- **Scope:** Schema v2 of the agent-graph model in `aw_runtime::graph`:
  first-class **multi-agent** graphs, an LLM-routing **Supervisor** node, and
  **Parallel/Join** fan-out with deterministic, durable execution. This is
  W4-PR1 (engine); W4-PR2 (designer swaps to this shared engine) and W4-PR3
  (canvas/publish) follow in greentic-designer.
- **Builds on:** `docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md`
  (PR #410, merged) — model/executor/checkpoint/redis are extended, not
  replaced.

## Goals

1. `schemaVersion: 2` graphs may contain any number of Agent nodes, plus the
   new `supervisor`, `parallel`, and `join` node kinds.
2. v1 graphs keep working byte-for-byte (validation, execution, checkpoint
   wire format all backward compatible; in-flight v1 runs resume fine).
3. Determinism and durability guarantees carry over: replay-before-invoke per
   node visit, crash-resume without duplicate side effects — now including
   mid-parallel-branch crashes.
4. Admin registry and store hand-off need **zero changes** (the graph doc is
   opaque to them by design).

## Non-goals

- Designer UI / publish changes (W4-PR3).
- Nested parallels in v2.0 (a `parallel` inside a parallel branch is rejected
  by validation — single level keeps the frontier model simple; lift later).
- Cross-branch communication during parallel execution (branches are
  isolated until join).

## Model (v2 additions)

```jsonc
{ "id": "sup",  "kind": "supervisor",
  "systemPrompt": "...", "model": "gpt-4o-mini",
  "routes": [ {"branch": "billing", "description": "Billing questions"},
              {"branch": "tech",    "description": "Technical issues"} ] }

{ "id": "fan",  "kind": "parallel" }   // N>=2 outgoing edges, each with a unique `branch` label
{ "id": "meet", "kind": "join" }       // M>=2 incoming edges from parallel branches, 1 outgoing
```

Validation (v2 adds to the v1 rules):

- `supervisor`: ≥2 routes; every route's `branch` has exactly one matching
  outgoing edge; no duplicate branch labels.
- `parallel`: ≥2 outgoing edges, each carrying a unique `branch` label; every
  branch path must reach the SAME `join` node before reaching `respond` or
  looping (checked by walk); no nested `parallel` on any branch path; branch
  paths are **node-disjoint** until the join (two branches sharing a node
  before `join` is rejected — concurrent branch drives share the visits map,
  so a shared node would collide on attempt numbering).
- `join`: ≥2 incoming edges; exactly 1 outgoing edge; reachable only via a
  `parallel`.
- Multi-agent: any number of Agent nodes was already legal in the model —
  v2 adds explicit test coverage and keeps it legal under v1 too (the v1
  flattening restriction was a designer-publish artifact, not an engine
  rule).
- `SUPPORTED_SCHEMA_VERSIONS` becomes `1..=2`; kind-gating: a v1 document
  containing v2 kinds is rejected (`Invalid("node kind X requires
  schemaVersion 2")`).

## Execution semantics

### Supervisor

A recorded effect, like an agent turn:

1. attempt = visits+1; replay from ledger if recorded.
2. Otherwise invoke a `SupervisorFn` closure (host wires it to
   `AgentRuntime::step` with a routing prompt: the node's `systemPrompt` +
   a generated route menu listing `branch: description` pairs + the
   conversation tail).
3. The reply must contain the sentinel `[[ROUTE:<branch>]]`. Parse →
   validated against the node's routes. Unparseable or unknown branch →
   fall back to the FIRST route with `tracing::warn!` (never fail the run on
   a routing parse miss).
4. The chosen branch is the recorded result (`{"branch": "..."}`), so resume
   replays the same routing decision deterministically.
5. Cursor moves along the chosen branch edge. Supervisor visits do NOT
   increment `iterations` (that stays Router-owned).

### Parallel / Join (frontier execution)

- Reaching `parallel` snapshots the current `GraphRunState` once per branch.
  Branch states are isolated clones; the trunk state is parked.
- Branches execute **concurrently** (`futures::future::join_all` over a
  per-branch drive loop). Each branch has its own cursor and contributes to
  the SHARED visits map (node ids are globally unique, and a node can only be
  on one branch path by validation).
- Per-branch node visits use the same ledger (`record_node_visit`), so a
  crash mid-parallel replays completed branch work.
- **Join merge is deterministic:** branch results merge in branch-label
  lexicographic order (NOT completion order). Merge = append each branch's
  NEW messages (those added after the snapshot) onto the trunk state in that
  order, `resolved = any(branch.resolved)`, `iterations = max(branch
  iterations)`, scratchpad: branch scratchpads stored under
  `scratchpad.branches.<label>`.
- Checkpoint shape: `GraphRunRecord` gains `frontier_json: Option<String>`
  (serialized `Vec<BranchCursor { branch, cursor, state_json }>`), `None`
  outside a parallel region. v1 records deserialize with `frontier_json:
  None` (serde default) — in-flight v1 runs resume unchanged. Redis/in-memory
  stores: the record is a single JSON blob, so no key changes.
- `MAX_NODE_VISITS = 64` is a GLOBAL cap across all branches (shared visits
  map makes this natural).
- A branch reaching `join` parks; when ALL branches have parked, the merged
  trunk resumes after `join`. `respond` inside a parallel branch is rejected
  by validation (only the trunk responds).
- Crash mid-parallel: resume reloads the frontier, re-drives only branches
  whose cursor isn't parked, replaying ledgered visits.

### Effects surface (host wiring)

`GraphExecutor::new` gains a `SupervisorFn` (same Arc-closure shape as
`AgentTurnFn`). `RuntimeGraphNodeHandler` wires it to the same per-visit
`AgentRuntime` construction as agent turns, with the generated routing
prompt. No new env vars; no checkpoint-store trait changes
(`record_node_visit` already covers supervisor decisions).

## Backward compatibility matrix

| Artifact | v1 behavior after this change |
| --- | --- |
| v1 sidecar/doc | identical validation + execution |
| In-flight v1 run record | resumes (frontier_json defaults None) |
| v1 fixtures/tests | all keep passing unmodified |
| Admin/store | untouched (opaque doc) |
| Designer (pre-swap) | keeps its own engine until W4-PR2 |

## Testing

- Validation matrix for each new rule (supervisor routes, parallel branch
  labels, same-join requirement, nested-parallel rejection, respond-in-branch
  rejection, v2-kind-in-v1-doc rejection).
- Multi-agent: two-agent relay graph (agent A → agent B → router → respond)
  executes with per-node config isolation.
- Supervisor: routing happy path; unknown-branch fallback to first route
  (with warn); replay determinism (recorded decision survives resume).
- Parallel: branch isolation (messages don't bleed mid-flight); deterministic
  merge order regardless of completion order (inject artificial delays);
  global visit cap across branches; crash mid-branch → resume completes
  without duplicate effects (extends `tests/graph_crash_resume.rs`).
- v1 regression: entire existing graph test suite green, untouched.

## Delivery

Single PR to `research` in greentic-runner (this repo), stacked on merged
#410; #411 (HTTP provider) is independent — merge order between them does
not matter (trivial `mod.rs` adjacency at worst). W4-PR2/PR3 land in
greentic-designer afterwards.

## Follow-ups (out of scope)

- Nested parallel regions.
- Streaming supervisor deltas to observers.
- Branch-level timeouts/budgets.
