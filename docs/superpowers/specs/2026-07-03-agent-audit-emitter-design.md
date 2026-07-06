# EPIC-B v2 (slice B-3) — Agentic-Worker Step Audit Emitter — Design Spec

**Status:** Draft — 2026-07-03
**Initiative:** Agentic platform coverage PRD, EPIC-B "durable queryable audit". Completes the runtime-audit story: B-2 emits per-flow-node activity; B-3 emits per **agentic-worker step** (tool calls/results) so `dw.agent` reasoning is captured in `audit_activity` too.

## 1. Problem & goal

B-2 gave greentic-runner a best-effort NATS `audit.>` emitter for flow **nodes**, ingested by the admin subscriber. But an agentic worker (`dw.agent`) runs an internal Plan-Act-Observe loop whose per-step tool calls/results are invisible to the audit store — the flow only sees one `dw.agent` node end. This slice emits an audit event per agent step (tool_call, tool_result) so operators can see what tools an agent invoked, when, and with what outcome.

**Goal:** a best-effort audit emit per agentic-worker step, reusing B-2's `AuditSink` + envelope helpers, published to `audit.<tenant>.agent.<event>`, ingested by the existing admin subscriber. Off by default; never affects agent execution.

## 2. Reuse of B-2 (already on research)

B-2 (`greentic-runner#518`) landed in `crates/greentic-runner-host/src/trace/`:
- `AuditSink` (bounded-channel, `emit(subject, &EventEnvelope)`, best-effort/drop-on-full) — reused verbatim.
- `build_audit_event`/`audit_subject`/`NodeAuditRecord`/`Outcome` — the flow-node builder. B-3 adds a small sibling `build_agent_audit_event` (type `greentic.runner.agent.<event>`) rather than overloading the node builder.

Admin ingest is unchanged (the B-2b payload fallback already reads `payload.duration_ms`/`payload.outcome`).

## 3. The seam

`greentic-aw-runtime` (a lower-level crate that greentic-runner-host depends on) exposes:
- `trait StepObserver { wants_streaming; on_token_delta; on_tool_call(name, call_id); on_tool_result(name, call_id, result) }` (`src/lib.rs:110`), injected as `Arc<dyn StepObserver>` via `AgentRuntime::step_with_observer` (`lib.rs:351`), defaulting to `NoopStepObserver` on the plain `step()` path.

greentic-runner-host invokes the agent at:
- `src/runner/agent_node.rs:197` — `self.runtime.step(tenant, session_id, agent_id, input)` (the flow `dw.agent` node).
- `src/runner/graph_node.rs:872, 1007` — the agent-graph nodes.

Because `AuditSink` lives in runner-host (the higher crate) and `StepObserver` is defined in aw-runtime (the lower crate), the observer impl **must live in runner-host** (it can depend on aw-runtime's trait + on `AuditSink`). aw-runtime stays NATS-free.

## 4. Architecture

### 4.1 `AgentAuditObserver` (runner-host, `src/trace/agent_audit.rs`)
Implements `aw_runtime::StepObserver`. Holds an `AuditSink`, the flow `TenantCtx`, `agent_id`, `session_id`. `wants_streaming()` → `false` (no token streaming; audit only cares about tool steps). `on_tool_call(name, call_id)` → emit `agent.tool_call`; `on_tool_result(name, call_id, result)` → emit `agent.tool_result`. Each builds an `EventEnvelope` via `build_agent_audit_event` and calls `sink.emit(audit_subject(tenant, "tool_call"|"tool_result"), &env)`. `on_token_delta` → no-op. Best-effort throughout (sink already never blocks/fails).

### 4.2 `build_agent_audit_event` (runner-host, `src/trace/audit_event.rs`)
`fn build_agent_audit_event(tenant: &TenantCtx, agent_id: &str, session_id: &str, event: &str, payload: serde_json::Value, now: DateTime<Utc>, id: String) -> EventEnvelope`. Builds: `type = "greentic.runner.agent.<event>"`, `source = "runner"`, `subject = "agent:<agent_id>"`, `correlation_id = session_id`, `time = now`, `tenant`, `payload`. For `tool_call`: `payload = { agent_id, tool: name, call_id }`. For `tool_result`: `payload = { agent_id, tool: name, call_id, result }` (the admin subscriber redacts on ingest). Shares the envelope-construction internals with the node builder (extract a small private `base_event(...)` helper in audit_event.rs to DRY the two builders).

### 4.3 Wiring — thread the sink to the agent invocation sites
The `AuditSink`/`audit_nats_client` that B-2 threaded to the `TraceRecorder` site must also reach `agent_node.rs`/`graph_node.rs`. Read the current construction path for the `AgentNode`/graph executor (they carry `self.runtime`); add an `Option<AuditSink>` (or the `Option<async_nats::Client>` to build one) + the tenant/session context along that path. At each `.step(...)` call, when audit is enabled build an `AgentAuditObserver` and call `.step_with_observer(tenant, session_id, agent_id, input, Arc::new(observer))` instead of `.step(...)`. When disabled (no sink), keep the existing `.step(...)` path unchanged.

## 5. Scope boundaries (YAGNI)
**In v1:** emit on `on_tool_call` + `on_tool_result` for the flow `dw.agent` node (`agent_node.rs`) AND the agent-graph nodes (`graph_node.rs`) if the sink threads cleanly to both; otherwise ship `agent_node.rs` first and note graph as a fast follow. Best-effort, off-by-default, `audit.<tenant>.agent.<event>`, the DRY'd envelope builder, unit tests.

**Deferred:**
- Token-delta/streaming audit (high volume, low audit value) — `wants_streaming=false`.
- A per-step "reply"/final-answer audit event (the `AgentStep::Reply` in the returned trail) — could be added, but v1 focuses on tool steps (the operationally interesting part).
- SoRX/OperaX emitters; sampling; the dedicated `GREENTIC_AUDIT_EMIT` gate (rides B-2's gating).
- `duration_ms` per agent step (the `StepObserver` hooks don't carry per-tool timing; omitted — the node-level `dw.agent` duration is already captured by B-2).

## 6. Failure semantics & gating
Identical to B-2: `AuditSink::emit` is non-blocking/drop-on-full/never-errors; a NATS outage cannot stall or fail the agent loop; off unless the NATS client is configured (same gate as B-2). `wants_streaming=false` keeps the observer cheap.

## 7. Testing
- `build_agent_audit_event` round-trips under the admin decode contract (reuse B-2's `admin_decode` test helper: `tenant.tenant`, `type`=`greentic.runner.agent.tool_call`, `source`, `subject`=`agent:<id>`, `correlation_id`, `payload.tool`).
- `AgentAuditObserver`: `on_tool_call` enqueues one `tool_call` event with the right subject/payload; `on_tool_result` enqueues one `tool_result` with the result in payload; `on_token_delta` enqueues nothing; `wants_streaming()==false`. (Build the observer with a `from_sender` test `AuditSink` — the B-2 test ctor — and inspect the channel.)
- Wiring: with no sink, the agent path uses `.step(...)` unchanged (existing agent tests pass); with a sink, `.step_with_observer` is used.

## 8. Rollout
Additive; off unless NATS configured; no admin change (subscriber + B-2b fallback already handle it); reuses B-2 infrastructure. Target `research`.
Note: builds run in this contended, shared machine with `-j2` (memory-frugal) to coexist with concurrent builds; the nested-worktree fixture-build quirk (B-2) applies to `oauth_broker`/`operator_invoke` and is environmental/CI-authoritative.
