# Agent-Graph Node Audit Emit (EPIC-B B-3b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** the agent-graph node path (`RuntimeGraphNodeHandler` → `run_one_agent_turn`/`run_one_supervisor_turn`) emits per-tool-step audit events under the REAL tenant, reusing B-3's `AgentAuditObserver` + `AuditSink`. Off by default; the no-sink path is byte-identical to today.

**Architecture:** Thread `Option<AuditSink>` from the existing `agent_audit_sink` (in scope at `runtime.rs:388` where the graph handler is built) into `build_graph_node_handler` → `RuntimeGraphNodeHandler` → the two turn functions. There, when the sink is present, build a B-3 `AgentAuditObserver` with the REAL tenant/session (which `GraphNodeHandler::execute` already receives) and call `.step_with_observer` instead of `.step`. The synthetic `TenantContext::new("graph","run")` used for per-visit STATE is left unchanged (audit-identity is decoupled from state-identity).

**Tech Stack:** Rust (edition 2024). Reuses `crate::trace::agent_audit::AgentAuditObserver` + `crate::trace::audit_sink::AuditSink` (both on research from B-3).

## Global Constraints

- **Crate:** `crates/greentic-runner-host` only. Reuse B-3's `AgentAuditObserver::new(sink, tenant: TenantCtx, agent_id: String, session_id: String)` + `AuditSink` (Clone) — do NOT modify them or add a new event type. NO admin/aw-runtime/other-repo change.
- **Off by default / no-sink byte-identical:** when the threaded sink is `None` (no `GREENTIC_EVENTS_NATS_URL`), the turn functions call the pre-existing `.step(...)` verbatim. Zero behavior change for the default path.
- **Do NOT change the state tenant:** the `AgentRuntime`'s state store keeps using the synthetic `TenantContext::new("graph","run")` (per-visit durability is intentional). The real tenant is used ONLY to build the audit observer's `TenantCtx`.
- **Best-effort:** the observer only calls `AuditSink::emit` (non-blocking, never errors/panics). Never affects the graph turn.
- **No new deps.** **Conventional commits, NO Claude co-author.** Target `research`.
- **Build discipline (SHARED CONTENDED MACHINE — ~8 concurrent builds, OOM risk):** ALWAYS `-j2` + `CARGO_BUILD_JOBS=2`; FOREGROUND, block+wait. NEVER pkill/kill or delete another worktree's `target/`. The `oauth_broker`/`operator_invoke` fixture-build failures are the known nested-worktree quirk (environmental, CI-root authoritative) — ignore them.

---

### Task 1: thread `Option<AuditSink>` to the graph handler + turn functions

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/graph_node.rs` (`build_graph_node_handler` signature ~:316; `RuntimeGraphNodeHandler` struct ~:595 + its `execute`; `run_one_agent_turn` ~:810 + `run_one_supervisor_turn` ~:943 signatures)
- Modify: `crates/greentic-runner-host/src/runtime.rs` (~:388 call site — pass `agent_audit_sink.clone()`)
- Test: compile-only (behavior added in Task 2)

**Interfaces:**
- Produces: `build_graph_node_handler(graphs, audit_sink: Option<AuditSink>)`; `RuntimeGraphNodeHandler { ..., audit_sink: Option<AuditSink> }`; `run_one_agent_turn(..., audit_sink: Option<&AuditSink>, real_tenant: &TenantCtx)` (+ the same on `run_one_supervisor_turn`) — read the CURRENT signatures first and add the two params threaded from `execute` (which already has the real tenant/env/session).

- [ ] **Step 1: Read the current code.** `graph_node.rs`: `build_graph_node_handler` (~:316), the `RuntimeGraphNodeHandler` struct + `impl GraphNodeHandler for RuntimeGraphNodeHandler` (~:595, its `execute` signature — note it already receives the real `tenant`/`env`/`session`), `run_one_agent_turn` (~:810) + `run_one_supervisor_turn` (~:943). `runtime.rs:315` (`agent_audit_sink`) + `:388` (`build_graph_node_handler(graphs)`).
- [ ] **Step 2: Thread the field + params.** Add `audit_sink: Option<AuditSink>` to `build_graph_node_handler`'s params + store it on `RuntimeGraphNodeHandler`. `execute` passes `self.audit_sink.as_ref()` + the real tenant/env it already has into `run_one_agent_turn`/`run_one_supervisor_turn` (add the two params). Pass `agent_audit_sink.clone()` at `runtime.rs:388`. Do NOT use the params yet (Task 2) — this step is a pure additive thread that must compile with no behavior change.
- [ ] **Step 3: Compile.** `CARGO_BUILD_JOBS=2 cargo build -p greentic-runner-host -j2` — expect clean (the new params are unused; add `let _ = (audit_sink, real_tenant);` or `#[allow(unused)]` on them ONLY for this intermediate commit, to be removed in Task 2).
- [ ] **Step 4: Commit** (`chore(audit): thread Option<AuditSink> + real tenant to graph turn functions`).

---

### Task 2: inject `AgentAuditObserver` at the graph `.step` sites + tests

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/graph_node.rs` (`run_one_agent_turn` + `run_one_supervisor_turn` `.step(...)` sites)
- Test: inline `#[cfg(test)]` in graph_node.rs

**Interfaces:**
- Consumes: Task 1's threaded `audit_sink`/`real_tenant`; B-3's `AgentAuditObserver`, `AuditSink` (`from_sender` test ctor).

- [ ] **Step 1: Write the failing tests.** Using B-3's `AuditSink::from_sender` (channel exposed) + a tool-invoking agent turn (mirror how B-3's agent_node tests / existing graph_node tests drive a turn): with `audit_sink: Some`, `run_one_agent_turn` enqueues `audit.<real_tenant>.agent.tool_call`/`tool_result` under the REAL tenant id (NOT `"graph"`); with `audit_sink: None`, nothing is enqueued and the turn still completes (byte-identical). Same for `run_one_supervisor_turn` if it dispatches tools. (If driving a full turn is hard, assert at the seam: the observer is built with the real tenant + passed to `step_with_observer` when `Some`, and `.step` is used when `None`.)
- [ ] **Step 2: Run — expect FAIL** (`CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2 graph_node`).
- [ ] **Step 3: Implement.** In each turn function, build `real_tenant_ctx: TenantCtx` from the threaded real tenant/env (mirror how B-3's agent_node builds the `TenantCtx` at its site — read that), then:
```rust
match audit_sink {
    Some(sink) => {
        let obs = std::sync::Arc::new(AgentAuditObserver::new(
            sink.clone(), real_tenant_ctx, agent_id.clone(), session_id.clone()));
        runtime.step_with_observer(tenant.clone(), &session_id, &agent_id, input, obs).await
    }
    None => runtime.step(tenant.clone(), &session_id, &agent_id, input).await,
}
```
(`tenant` here is the SYNTHETIC state `TenantContext::new("graph","run")` — unchanged; `real_tenant_ctx` is used ONLY for the observer.) Remove the Task-1 `#[allow(unused)]`.
- [ ] **Step 4: Run — expect PASS.**
- [ ] **Step 5: Gate + commit.** `cargo fmt --all`; `CARGO_BUILD_JOBS=2 cargo clippy -p greentic-runner-host -j2 --all-targets -- -D warnings`; `CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2` (ignore the known `oauth_broker`/`operator_invoke` fixture failures). Commit (`feat(audit): emit agent-graph node tool-step audit under real tenant`).

---

## Self-Review

- **Spec coverage:** §3.1 thread → Task 1; §3.2 inject + decoupled state tenant → Task 2; §3.3 identity → Task 2 (real tenant for envelope, graph session for correlation); §4 gating/off-by-default → Task 2 `None` arm + Global Constraints; §6 tests → Task 2. §5 deferred (state tenant, guardrails, graph event type) → out of plan.
- **Placeholder scan:** "read the current signatures / how B-3 builds the TenantCtx at agent_node" are deliberate — the exact turn-function signatures + the `TenantCtx` construction must be read from the repo. The observer + sink are reused verbatim. No TBD as work-defining.
- **Type consistency:** `Option<AuditSink>` threaded Task 1 → used Task 2; `AgentAuditObserver::new(sink, TenantCtx, String, String)` matches B-3; `.step_with_observer` signature matches aw-runtime (as B-3 used it). Real-tenant vs synthetic-state-tenant kept distinct throughout.
- **Scope:** 2 files (graph_node.rs + runtime.rs call site), reuses B-3 wholesale, one plan, runner-host only.
