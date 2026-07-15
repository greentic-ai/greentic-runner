# EPIC-B B-3b — Agent-Graph Node Audit Emit — Design Spec

**Status:** Draft — 2026-07-05
**Initiative:** Agentic platform coverage PRD, EPIC-B audit. Extends B-3 (which audited flow `dw.agent` tool steps) to the **agent-graph** node path (`RuntimeGraphNodeHandler`), which B-3 explicitly deferred because its runtime used a synthetic tenant.

## 1. Problem & goal

B-3 injects an `AgentAuditObserver` at the flow `dw.agent` node so agent tool steps emit `audit.<tenant>.agent.<event>`. The **agent-graph** path (`execute_dw_agent_graph` → `RuntimeGraphNodeHandler` → `run_one_agent_turn`/`run_one_supervisor_turn`) does NOT emit audit — its per-visit `AgentRuntime` uses a synthetic `TenantContext::new("graph","run")` (for per-visit state) and B-3's review deferred wiring it (the request carried no tenant).

**Goal:** the agent-graph node path emits the same per-tool-step audit events as the flow `dw.agent` node, using the **real** tenant/session, reusing B-3's `AgentAuditObserver` + `AuditSink` wholesale. Off by default (no NATS → no sink → unchanged). No new event type, no admin change.

## 2. Why the surgery is small (recon)

- `execute_dw_agent_graph` (`engine.rs:~1238`) already passes the **real** `ctx.tenant` + `default_env` + `ctx.session_id` into `GraphNodeHandler::execute` — so the handler already receives the real tenant/session; only the per-visit runtime rebuilds a synthetic one for **state**.
- The `AuditSink` is already in scope where the graph handler is built: `runtime.rs:315` builds `agent_audit_sink`; `runtime.rs:388` calls `build_graph_node_handler(graphs)` in the **same function** — the sink is a local there.
- B-3 already provides `AgentAuditObserver::new(sink, tenant, agent_id, session_id)` + `AuditSink: Clone`.

So the only gaps are: (a) thread `Option<AuditSink>` into `build_graph_node_handler` → the handler struct → the two turn functions; (b) in the turn functions, when the sink is present, build `AgentAuditObserver` with the **real** tenant/session and call `.step_with_observer` instead of `.step`.

## 3. Architecture (additive, off by default, decoupled from state)

### 3.1 Thread the sink
`build_graph_node_handler(graphs)` → `build_graph_node_handler(graphs, Option<AuditSink>)` (call site `runtime.rs:388` passes `agent_audit_sink.clone()`). `RuntimeGraphNodeHandler` (`graph_node.rs:595`) gains an `audit_sink: Option<AuditSink>` field. The handler's `execute` (which already has the real tenant/env/session) passes the sink + the real tenant/session down to `run_one_agent_turn`/`run_one_supervisor_turn`.

### 3.2 Inject the observer (audit-identity decoupled from state-identity)
`run_one_agent_turn`/`run_one_supervisor_turn` KEEP the synthetic `TenantContext::new("graph","run")` for the `AgentRuntime`'s **state store** (per-visit durability is intentional — do NOT change it). For **audit only**, build a `greentic_types::TenantCtx` from the real tenant/env passed in, and:
```
match &audit_sink {
  Some(sink) => {
    let obs = Arc::new(AgentAuditObserver::new(sink.clone(), real_tenant_ctx, agent_id, real_session_id));
    runtime.step_with_observer(state_tenant, &session_id, &agent_id, input, obs).await
  }
  None => runtime.step(state_tenant, &session_id, &agent_id, input).await   // byte-identical to today
}
```
The observer emits `audit.<real_tenant>.agent.tool_call|tool_result` (B-3's builder, unchanged) — so a graph agent-turn's tool steps land in the audit store under the real tenant, exactly like a flow `dw.agent` node.

### 3.3 Subject/session identity
`agent_id` + the per-visit `session_id` (`graph__<node_id>`) are the audit `subject`/`correlation`. Use the real tenant for the envelope's `tenant` (the queryable column); the `session_id` stays the graph per-visit id (fine — it correlates the tool steps of that visit). Reuse `AgentAuditObserver` exactly (no changes to it).

## 4. Failure semantics & gating
Identical to B-3: `AuditSink::emit` is non-blocking/drop-on-full/never-errors; `wants_streaming()==false`; a NATS outage cannot affect the graph turn. Off unless the sink is `Some` (no `GREENTIC_EVENTS_NATS_URL` → `agent_audit_sink` is `None` → the `None` arm is byte-identical to today).

## 5. Scope boundaries (YAGNI)
**In v1:** thread `Option<AuditSink>` to `build_graph_node_handler` + `RuntimeGraphNodeHandler`; inject `AgentAuditObserver` (real tenant, B-3's observer) at both `run_one_agent_turn` and `run_one_supervisor_turn` `.step` sites; off-by-default; a test that the `None` path is unchanged + the `Some` path emits.
**Deferred:** changing the graph runtime's per-visit synthetic **state** tenant (intentional — out of scope); guardrails on graph-node runtimes (a separate pre-existing TODO); a graph-specific event type (reuse `agent.tool_call`/`tool_result`).

## 6. Testing
- `run_one_agent_turn` / `run_one_supervisor_turn` with a test `AuditSink` (B-3's `from_sender`) + a tool-invoking agent → assert `audit.<real_tenant>.agent.tool_call`/`tool_result` enqueued under the REAL tenant (not `"graph"`); with `audit_sink: None` → nothing enqueued and the `.step` path is used (byte-identical). The synthetic state tenant is unchanged in both.
- `build_graph_node_handler` accepts + stores the `Option<AuditSink>`.

## 7. Rollout
Additive; off unless NATS configured; reuses B-3 (`AgentAuditObserver`, `AuditSink`); no admin/aw-runtime/other-repo change; runner-host only. Target `research`. Completes agent-graph audit coverage (flow `dw.agent` done by B-3).
