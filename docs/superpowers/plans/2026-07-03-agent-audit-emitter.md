# Agentic-Worker Step Audit Emitter (EPIC-B slice B-3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** greentic-runner emits a best-effort audit `EventEnvelope` per agentic-worker step (tool_call, tool_result) to NATS `audit.<tenant>.agent.<event>`, reusing B-2's `AuditSink` + envelope helpers, ingested by the existing admin subscriber. Off by default; never affects agent execution.

**Architecture:** A new `AgentAuditObserver` (impl `aw_runtime::StepObserver`) in `crates/greentic-runner-host/src/trace/agent_audit.rs`, built from the B-2 `AuditSink`, injected at the `dw.agent` invocation sites via `AgentRuntime::step_with_observer`. A DRY'd `build_agent_audit_event` sits beside B-2's `build_audit_event`.

**Tech Stack:** Rust (edition 2024), reuses `AuditSink`/`build_audit_event`/`audit_subject` (already on research from B-2), `aw_runtime::StepObserver`, `greentic-types`, `serde_json`, `chrono`.

## Global Constraints

- **Crate:** `crates/greentic-runner-host`. Reuse the B-2 helpers in `src/trace/{audit_sink.rs, audit_event.rs}` (all `pub`, same crate). NO admin/other-repo change; do NOT modify `greentic-aw-runtime` (only depend on its `StepObserver` trait).
- **Cross-repo envelope contract:** the emitted `EventEnvelope` must decode under admin `activity_from_envelope` (`tenant.tenant`, `type`, `source`, `subject`, `time`, `correlation_id`, `payload`; B-2b added `payload.outcome`/`payload.duration_ms` fallback). Agent events: `type="greentic.runner.agent.tool_call"|"...tool_result"`, `source="runner"`, `subject="agent:<agent_id>"`, `correlation_id=<session_id>`, `payload` per §4.2 of the spec.
- **Best-effort, never blocks/fails the agent loop:** the observer only calls `AuditSink::emit` (already non-blocking, drop-on-full, never errors/panics); `wants_streaming()==false`.
- **Off by default:** with no NATS client (no `GREENTIC_EVENTS_NATS_URL`), no sink → the agent invocation uses the existing `.step(...)` path unchanged. Zero default-path change.
- **No new deps.** **Conventional commits, NO Claude co-author.** Target `research`.
- **Build discipline (SHARED CONTENDED MACHINE — the user runs ~8 concurrent cargo builds; naive builds get OOM-killed):** run every cargo command with `-j2` (e.g. `CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2 <name>`) to cap peak rustc memory so it coexists. FOREGROUND; block and wait (builds are slow under contention). NEVER pkill/kill or delete another worktree's `target/`. The `oauth_broker`/`operator_invoke` fixture-build failures are the known nested-worktree quirk (environmental, CI-root authoritative) — ignore them.

---

### Task 1: `build_agent_audit_event` + `AgentAuditObserver`

**Files:**
- Modify: `crates/greentic-runner-host/src/trace/audit_event.rs` (add `build_agent_audit_event` + extract a private `base_event` helper the two builders share)
- Create: `crates/greentic-runner-host/src/trace/agent_audit.rs` (+ `pub mod agent_audit;` in `src/trace/mod.rs`)
- Test: inline `#[cfg(test)]` in both files

**Interfaces:**
- Consumes: B-2's `AuditSink` (`emit`, `#[cfg(test)] from_sender`), `audit_subject`, `greentic_types::{EventEnvelope, TenantCtx}`, `aw_runtime::StepObserver`.
- Produces:
  - `fn build_agent_audit_event(tenant: &TenantCtx, agent_id: &str, session_id: &str, event: &str, payload: serde_json::Value, now: DateTime<Utc>, id: String) -> EventEnvelope`.
  - `struct AgentAuditObserver { sink: AuditSink, tenant: TenantCtx, agent_id: String, session_id: String }` implementing `StepObserver` (`wants_streaming→false`, `on_tool_call`, `on_tool_result`, `on_token_delta→{}`).

- [ ] **Step 1: Read B-2's `audit_event.rs`** — the existing `build_audit_event` internals (how it constructs `EventEnvelope`: id, topic, type, source, tenant, subject, time, correlation_id, payload, metadata). Identify the shared core to extract as `fn base_event(tenant, ty, source, subject, correlation_id, payload, time, id) -> EventEnvelope`.

- [ ] **Step 2: Write failing tests.**
  - In `audit_event.rs` (reuse the existing `admin_decode` test helper): `build_agent_audit_event(&tenant, "a1", "s1", "tool_call", json!({"tool":"http","call_id":"c1"}), now, "id1")` → serialize → `admin_decode` yields tenant `t1`, `type`=`greentic.runner.agent.tool_call`, `source`=`runner`, `subject`=`agent:a1`, `correlation_id`=`s1`; and `body.payload.tool == "http"`.
  - In `agent_audit.rs`: build an `AgentAuditObserver` with a `from_sender` `AuditSink` (channel exposed); `on_tool_call("http","c1")` → exactly one enqueued msg, subject `audit.t1.agent.tool_call`, `type` ends `agent.tool_call`; `on_tool_result("http","c1",&json!({"ok":true}))` → one `tool_result` with `payload.result`; `on_token_delta(...)` → zero enqueued; `wants_streaming()==false`.

- [ ] **Step 3: Run — expect FAIL** (`CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2 agent_audit` + `audit_event`).
- [ ] **Step 4: Implement** `base_event` (refactor `build_audit_event` to use it — keep its tests green), `build_agent_audit_event`, and `AgentAuditObserver`. `on_tool_call`: `emit(audit_subject(&tenant.tenant, "tool_call"), &build_agent_audit_event(&tenant, &agent_id, &session_id, "tool_call", json!({"agent_id":agent_id,"tool":name,"call_id":call_id}), Utc::now(), <uuid>))`; `on_tool_result` similarly with `"result": result` in payload. (Match how B-2 generates its event id — reuse the same id source.)
- [ ] **Step 5: Run — expect PASS + commit** (`CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2 agent_audit audit_event`; then `cargo fmt --all`; `CARGO_BUILD_JOBS=2 cargo clippy -p greentic-runner-host -j2 --lib -- -D warnings`). Commit: `feat(audit): agent-step audit event builder + StepObserver`.

---

### Task 2: thread the sink + wire `step_with_observer` at the agent sites

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs` (the `.step(...)` at ~:197)
- Modify: `crates/greentic-runner-host/src/runner/graph_node.rs` (the `.step(...)` at ~:872, ~:1007) — if the sink threads cleanly; else ship agent_node.rs only + note graph as follow-up
- Modify: whatever constructs `AgentNode`/the graph executor to pass an `Option<AuditSink>` + tenant/session context (read the construction path; it parallels B-2's `audit_nats_client` threading to `PackFlowAdapter`)
- Test: unit/integration for the enable/disable branch

**Interfaces:**
- Consumes: Task 1's `AgentAuditObserver`, B-2's `AuditSink`, the threaded `Option<async_nats::Client>`/`Option<AuditSink>`.

- [ ] **Step 1: Read the agent invocation + construction path.** `agent_node.rs:197` `self.runtime.step(tenant, session_id, agent_id, input)`. Find where `AgentNode` (and the graph executor) is built and whether B-2's `audit_nats_client`/`AuditSink` already reaches there (it was threaded to `PackFlowAdapter`/the `TraceRecorder` site). Extend that threading to carry an `Option<AuditSink>` (or `Option<async_nats::Client>` to build one) to these node executors.
- [ ] **Step 2: Write the failing test** — with a sink present, invoking the agent node routes through `step_with_observer` and (via a test `AuditSink`) enqueues tool events; with no sink, the node uses `.step(...)` and enqueues nothing / behaves exactly as before. (Mirror how existing agent_node tests drive a step; if a full agent step is hard to unit-test, assert at the "observer is constructed and passed when sink present, None path otherwise" seam.)
- [ ] **Step 3: Run — expect FAIL.**
- [ ] **Step 4: Implement.** At each `.step(...)` site: `if let Some(sink) = &self.audit_sink { let obs = Arc::new(AgentAuditObserver::new(sink.clone(), tenant_ctx, agent_id, session_id)); self.runtime.step_with_observer(tenant, session_id, agent_id, input, obs).await } else { self.runtime.step(tenant, session_id, agent_id, input).await }`. (`AuditSink` must be `Clone` — it holds an `mpsc::Sender` which is `Clone`; if it isn't derived `Clone`, add `#[derive(Clone)]` in Task 1.) Keep the no-sink path byte-identical to today.
- [ ] **Step 5: Full gate + commit.** `cargo fmt --all`; `CARGO_BUILD_JOBS=2 cargo clippy -p greentic-runner-host -j2 --all-targets -- -D warnings`; `CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host -j2` (heavy + contended — wait; ignore the known `oauth_broker`/`operator_invoke` nested-worktree fixture failures). Commit: `feat(audit): emit agent-step audit via step_with_observer at dw.agent sites`.

---

## Self-Review

- **Spec coverage:** §4.1 observer → Task 1; §4.2 builder + DRY base_event → Task 1; §4.3 threading/wiring → Task 2; §6 gating/off-by-default → Task 2 Step 4 + Global Constraints; §7 tests → per-task (contract round-trip, observer enqueue, enable/disable). §5 deferred (streaming, reply event, sampling) → out of plan.
- **Placeholder scan:** "read B-2's audit_event.rs / the agent construction path" are deliberate — the exact `EventEnvelope` internals + the `AgentNode` construction/threading path must be read from the repo (they parallel B-2's T4 threading). The `AuditSink: Clone` requirement is called out explicitly. No TBD left as work-defining.
- **Type consistency:** `build_agent_audit_event`/`AgentAuditObserver` defined Task 1, consumed Task 2; `base_event` shared by both builders (Task 1); subject `audit.<tenant>.agent.<event>` consistent Task 1 (`audit_subject`) ↔ Task 2; `AuditSink` (Clone) reused from B-2.
- **Scope:** 1 new small file + 1 modified builder + 2-3 node wiring edits; reuses B-2 wholesale; one plan; runner-host only.
