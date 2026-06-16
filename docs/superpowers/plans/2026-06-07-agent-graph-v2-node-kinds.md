# Agent-Graph v2 Node Kinds Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** schemaVersion 2 of `aw_runtime::graph`: multi-agent coverage, Supervisor (LLM routing) node, Parallel/Join frontier execution — durable and deterministic.

**Architecture:** Extend the merged v1 engine (PR #410) in place. Supervisor is a new recorded effect (mirrors agent turns). Parallel switches the single cursor to a frontier (`Vec<BranchCursor>`) persisted in a new optional `GraphRunRecord.frontier_json` field; branches drive concurrently with isolated state clones and merge deterministically at Join.

**Tech Stack:** as v1 (tokio, serde, thiserror; `futures` may already be a workspace dep — check before adding).

**Spec:** `docs/superpowers/specs/2026-06-07-agent-graph-v2-node-kinds-design.md`

---

## Conventions

Same as the v1 plan (`2026-06-06-runtime-agent-graph-execution.md` Conventions section): worktree `feat/aw-graph-v2`, no unwrap/expect/panic outside tests, manual `Pin<Box>` futures in aw-runtime, thiserror, TDD, fmt+clippy -D warnings per task, conventional commits, no attribution trailers.

---

### Task 1: Model v2 — kinds + validation

**Files:** `crates/greentic-aw-runtime/src/graph/model.rs`, `test_fixtures.rs`

- [ ] Add `NodeKind::Supervisor { system_prompt, model, routes: Vec<SupervisorRoute> }` (`SupervisorRoute { branch: String, description: String }`, camelCase serde), `NodeKind::Parallel`, `NodeKind::Join` (tag values `supervisor`/`parallel`/`join`).
- [ ] `SUPPORTED_SCHEMA_VERSIONS: RangeInclusive<u32> = 1..=2`; `GraphConfig::from_json` accepts both; **kind-gating**: v1 doc containing a v2 kind → `Invalid("node kind `supervisor` requires schemaVersion 2")`.
- [ ] Validation additions (each with a failing test FIRST):
  - supervisor: ≥2 routes, unique branch labels, each route has exactly one matching outgoing branch edge, no extra outgoing edges beyond routes.
  - parallel: ≥2 outgoing edges with unique branch labels; walk every branch: must reach one COMMON join before respond/cycle; branch paths node-disjoint until that join; no nested parallel.
  - join: ≥2 incoming, exactly 1 outgoing, only reachable via parallel branches.
  - respond inside a parallel branch → Invalid.
- [ ] New fixtures in test_fixtures.rs: `supervisor_json()` (supervisor → 2 agent branches → router → respond), `parallel_json()` (parallel → 2 branches [agent / tool] → join → respond), both schemaVersion 2.
- [ ] ~12 validation tests (positive parse for both fixtures + each negative rule + v1-regression: ALL existing v1 tests untouched and green).
- [ ] fmt+clippy+commit `feat(aw-runtime): graph model v2 — supervisor, parallel, join kinds`

### Task 2: Supervisor execution

**Files:** `crates/greentic-aw-runtime/src/graph/executor.rs` (+ mod.rs re-exports)

- [ ] `SupervisorFn` type (Arc closure, mirrors AgentTurnFn) + `SupervisorRequest { node_id, system_prompt, model, routes, state }` + `SupervisorResult { branch: String, raw_reply: String }`. `GraphExecutor::new` gains the third closure (UPDATE all existing constructions incl. tests + runner-host — keep the diff mechanical).
- [ ] Drive-loop arm: replay-or-invoke via `visit_effect` (recorded result = the SupervisorResult JSON); after invoke, validate branch ∈ routes — unknown/unparseable handled in the HOST closure (executor trusts SupervisorResult.branch but re-validates against edges; invalid recorded branch → `GraphExecError::UnknownNode`-class error, can't happen via honest hosts). Cursor follows the chosen branch edge. No iterations increment. Trail entry `{node, kind: "supervisor", attempt, replayed, branch}`.
- [ ] Tests: routing happy path (both routes), replay determinism (resume reuses recorded branch, closure not re-invoked), trail shape.
- [ ] fmt+clippy+commit `feat(aw-runtime): supervisor node execution with recorded routing`

### Task 3: Parallel/Join frontier execution

**Files:** `executor.rs`, `checkpoint.rs` (+ redis/in-memory untouched — record is opaque JSON)

- [ ] `GraphRunRecord.frontier_json: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]` — v1 records keep deserializing (add an explicit test deserializing a v1-shaped record JSON).
- [ ] `BranchCursor { branch: String, cursor: String, state_json: String, parked: bool }`.
- [ ] Drive-loop arm for Parallel: snapshot state per branch (sorted branch labels), write frontier to record, then `futures::future::join_all` over per-branch drives. Branch drive = the existing single-cursor loop parameterized by (cursor, branch state), sharing visits map behind a Mutex (or pre-partitioned since branch paths are node-disjoint — choose the simpler correct one and document). Branch parks at Join.
- [ ] After all branches park: deterministic merge (lexicographic branch order): append branch-new messages, `resolved = any`, `iterations = max`, scratchpads under `scratchpad.branches.<label>`; frontier cleared (None); cursor = join's outgoing edge; checkpoint.
- [ ] Effect error inside a branch: other branches finish their current visit, frontier (with non-parked branch cursors at last successful checkpoint) persists, error propagates; resume re-drives only non-parked branches.
- [ ] Global MAX_NODE_VISITS across branches (shared counter).
- [ ] Tests: branch isolation; deterministic merge order under injected delays (slow branch A, fast branch B → merged order still A,B by label); cap across branches; parallel happy path end-to-end; mid-branch effect error → record keeps frontier; resume completes.
- [ ] fmt+clippy+commit `feat(aw-runtime): parallel/join frontier execution with durable branch cursors`

### Task 4: Crash-resume + determinism integration tests

**Files:** `crates/greentic-aw-runtime/tests/graph_crash_resume.rs` (extend)

- [ ] `parallel_resume_after_crash_completes_without_duplicate_effects`: two executor instances, crash one branch mid-parallel, fresh executor resumes — completed branch visits replayed, only unfinished work re-executes (exact counters).
- [ ] `supervisor_decision_survives_crash`: crash after supervisor visit recorded but before next checkpoint; resume must follow the SAME branch without re-invoking the supervisor closure.
- [ ] fmt+clippy+commit `test(aw-runtime): v2 crash-resume guarantees (parallel + supervisor)`

### Task 5: Handler wiring + matrix + CI + PR

**Files:** `crates/greentic-runner-host/src/runner/graph_node.rs`

- [ ] Wire `SupervisorFn` in `from_parts`: per-visit AgentRuntime (same pattern as agent turns) with a generated routing prompt: node systemPrompt + "Choose exactly one route and end your reply with [[ROUTE:<branch>]]" + the route menu (branch: description lines). Parse `[[ROUTE:x]]` (case-insensitive, last occurrence wins); unknown/missing → first route + `tracing::warn!` (per spec). Strip the sentinel from raw_reply.
- [ ] Handler tests: supervisor routing through the envelope; fallback-to-first-route on garbage reply (mock closure path — keep mocks at the executor seam via with_effects extended for SupervisorFn).
- [ ] Feature matrix (4 checks as v1 plan Task 9) + `bash ci/local_check.sh` (document pre-existing publish-dry-run failure again).
- [ ] Push `feat/aw-graph-v2`, PR to research: title `feat: agent-graph v2 — multi-agent, supervisor, parallel/join`, body covers spec link, semantics summary (deterministic merge, frontier checkpoints, v1 compat matrix), test evidence, relation to #410/#411. No attribution. Verify headRefOid.
