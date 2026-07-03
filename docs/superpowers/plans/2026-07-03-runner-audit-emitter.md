# Runner Flow-Step Audit Emitter (EPIC-B slice B-2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** greentic-runner emits a best-effort audit `EventEnvelope` per flow node execution (end + error) to NATS `audit.<tenant>.flow.<event>`, which the already-shipped greentic-admin `audit.>` subscriber ingests into `audit_activity`. No admin change.

**Architecture:** A new best-effort `AuditSink` (bounded-channel + background-drain NATS publisher, mirroring sorx `NatsEventSink`). `TraceRecorder` (the existing single `ExecutionObserver`) gains an optional `AuditSink` and emits an audit event in `on_node_end`/`on_node_error` alongside its existing file buffering — avoiding any change to the engine's observer API. The connected `async_nats::Client` is cloned before it is moved into the response-listener loop and threaded down to the `TraceRecorder` construction site.

**Tech Stack:** Rust (edition 2024), `async-nats` + `greentic-types` (both already direct deps of `greentic-runner-host`), tokio, `serde_json`, `chrono`.

## Global Constraints

- **Crate:** all work is in `crates/greentic-runner-host` unless a step says otherwise. NO admin/other-repo change.
- **Cross-repo envelope contract (THE correctness requirement):** the emitted JSON must decode under greentic-admin's `activity_from_envelope`, which reads `tenant.tenant` (fallback top-level `tenant` string), `type`, `source`, `subject`, `time`, `correlation_id`, `payload`. Build a `greentic_types::EventEnvelope` DIRECTLY (not via `BusinessEventBuilder`) with `type="greentic.runner.flow.node_end"|"...node_error"`, `source="runner"`, `tenant=<flow TenantCtx>`, `subject="flow:<flow_id>/node:<node_id>"`, `time=Utc::now()`, `correlation_id=<session id>`, `payload={node_id,component_id,operation,outcome,duration_ms,error?}`. Read the real `EventEnvelope`/`TenantCtx` field names + serde renames in `greentic-types/src/events.rs` before constructing.
- **Best-effort, never blocks/fails execution:** the sink's `emit` is non-blocking (bounded channel, capacity 1024); full → drop + `tracing::warn!` + counter; publish error → warn; a NATS failure must never propagate into flow execution. Mirror the trace recorder's existing best-effort flush semantics.
- **Off by default:** with no NATS client (no `GREENTIC_EVENTS_NATS_URL`), the sink is `None` and `TraceRecorder` behaves exactly as today (file-only). Zero default-path change.
- **Volume:** emit on `on_node_end` + `on_node_error` ONLY, never `on_node_start`.
- **No new deps** (`async-nats`, `greentic-types`, `serde_json`, `chrono`, `tokio` are all present — verify in `crates/greentic-runner-host/Cargo.toml`).
- **Conventional commits, NO Claude co-author.** Target branch `research`.
- **Build discipline (shared machine, ~85GB free but greentic-runner is a LARGE workspace):** run cargo in the worktree only; FOREGROUND; prefer `cargo test -p greentic-runner-host <name>` for targeted runs; NEVER pkill/kill or delete another worktree's `target/`. The first build is cold/heavy — block and wait.

---

### Task 1: audit `EventEnvelope` builder + cross-repo contract test

**Files:**
- Create: `crates/greentic-runner-host/src/trace/audit_event.rs` (+ `pub mod audit_event;` in `src/trace/mod.rs`)
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `struct NodeAuditRecord<'a> { tenant: &'a greentic_types::TenantCtx, flow_id: &'a str, node_id: &'a str, component_id: &'a str, operation: &'a str, session_id: &'a str, duration_ms: u64, outcome: Outcome, error: Option<&'a str> }` where `enum Outcome { Ok, Error }`.
  - `fn build_audit_event(rec: &NodeAuditRecord, now: chrono::DateTime<chrono::Utc>, id: String) -> greentic_types::EventEnvelope` — constructs the envelope per the Global-Constraints contract.
  - `fn audit_subject(tenant: &str, event: &str) -> String` → `format!("audit.{tenant}.flow.{event}")`.

- [ ] **Step 1: Write the failing contract round-trip test** (this is the crux — it replicates the admin decoder):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    // Mirror greentic-admin::audit_ingest::activity_from_envelope's field reads.
    fn admin_decode(body: &Value) -> (String, String, String, String, String, String) {
        let tenant = body.get("tenant").and_then(|t| t.get("tenant")).and_then(Value::as_str)
            .or_else(|| body.get("tenant").and_then(Value::as_str)).unwrap().to_string();
        let ty = body.get("type").and_then(Value::as_str).unwrap().to_string();
        let source = body.get("source").and_then(Value::as_str).unwrap().to_string();
        let subject = body.get("subject").and_then(Value::as_str).unwrap().to_string();
        let time = body.get("time").and_then(Value::as_str).unwrap().to_string();
        let corr = body.get("correlation_id").and_then(Value::as_str).unwrap().to_string();
        (tenant, ty, source, subject, time, corr)
    }
    #[test]
    fn built_event_decodes_under_admin_contract() {
        let tenant = /* build a greentic_types::TenantCtx for ("t1","prod") — read the real ctor */;
        let rec = NodeAuditRecord { tenant: &tenant, flow_id: "f1", node_id: "n1",
            component_id: "greentic:http", operation: "call", session_id: "s1",
            duration_ms: 12, outcome: Outcome::Ok, error: None };
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-03T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let env = build_audit_event(&rec, now, "id1".to_string());
        let body = serde_json::to_value(&env).unwrap();
        let (tenant, ty, source, subject, time, corr) = admin_decode(&body);
        assert_eq!(tenant, "t1");
        assert_eq!(ty, "greentic.runner.flow.node_end");
        assert_eq!(source, "runner");
        assert_eq!(subject, "flow:f1/node:n1");
        assert!(time.starts_with("2026-07-03T00:00:00"));
        assert_eq!(corr, "s1");
        assert!(body.get("payload").and_then(|p| p.get("duration_ms")).is_some());
    }
    #[test]
    fn error_outcome_sets_type_and_payload_error() {
        // outcome=Error → type ends with node_error, payload.error == the message, payload.outcome=="error"
    }
    #[test]
    fn subject_is_well_formed() { assert_eq!(audit_subject("t1","node_end"), "audit.t1.flow.node_end"); }
}
```
(Read `greentic-types/src/events.rs` for the real `EventEnvelope` constructor/field access + `TenantCtx` ctor + how `tenant` serializes — adapt the `/* build TenantCtx */` line + the assertion on `tenant.tenant` accordingly. If `TenantCtx` serializes the tenant id under a different key, the admin contract test will catch it — that is the point.)

- [ ] **Step 2: Run — expect FAIL** (`cargo test -p greentic-runner-host audit_event`).
- [ ] **Step 3: Implement** `NodeAuditRecord`/`Outcome`/`build_audit_event`/`audit_subject`. `type` = `node_end` when `Ok` else `node_error`; `payload` = a `serde_json::json!({...})` with node_id/component_id/operation/outcome/duration_ms and `error` only when present.
- [ ] **Step 4: Run — expect PASS + commit** (`feat(audit): runner audit EventEnvelope builder + admin-contract round-trip`).

---

### Task 2: `AuditSink` — best-effort bounded-channel NATS publisher

**Files:**
- Create: `crates/greentic-runner-host/src/trace/audit_sink.rs` (+ `pub mod audit_sink;`)
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: `audit_event` (subject builder), `async_nats::Client`.
- Produces: `struct AuditSink { tx: tokio::sync::mpsc::Sender<(String, Vec<u8>)> }` with:
  - `fn new(client: async_nats::Client) -> AuditSink` — spawns the background drain task (subscribe side is the admin's; here we only publish). Channel capacity 1024.
  - `fn emit(&self, subject: String, envelope: &greentic_types::EventEnvelope)` — `serde_json::to_vec` then `try_send((subject, bytes))`; on `Err(TrySendError::Full|Closed)` → `tracing::warn!` + return (drop). NEVER blocks, NEVER returns an error.

- [ ] **Step 1: Write failing tests.**
  - `emit` on a sink whose receiver is saturated (fill 1024 without draining, using a sink built with a dummy/closed client OR by exposing the channel in `#[cfg(test)]`) does not panic and does not block — the 1025th `emit` returns immediately. (Factor so the test can construct the channel without a live NATS: e.g. a `AuditSink::from_sender(tx)` test ctor, and assert `try_send` full-drop behavior at the `emit` level.)
  - `emit` serializes the envelope to the expected bytes (round-trip `serde_json::from_slice` back to a `Value` with the `type` field).
- [ ] **Step 2: Run — expect FAIL.**
- [ ] **Step 3: Implement.** The drain task: `while let Some((subject, bytes)) = rx.recv().await { if let Err(e) = client.publish(subject, bytes.into()).await { tracing::warn!(%e, "audit publish failed"); } }`. Do NOT `flush` per message (fire-and-forget). Provide a `#[cfg(test)] from_sender` ctor for the saturation test.
- [ ] **Step 4: Run — expect PASS + commit** (`feat(audit): best-effort bounded-channel AuditSink`).

---

### Task 3: `TraceRecorder` fan-out to the sink

**Files:**
- Modify: `crates/greentic-runner-host/src/trace/recorder.rs` (add `Option<AuditSink>` field + emit in `on_node_end`/`on_node_error`)
- Test: inline `#[cfg(test)]` in recorder.rs

**Interfaces:**
- Consumes: `audit_sink::AuditSink`, `audit_event::{build_audit_event, audit_subject, NodeAuditRecord, Outcome}`.
- Produces: a `TraceRecorder` constructor variant that accepts `Option<AuditSink>` + the flow `TenantCtx`/`flow_id`/`session_id` context needed to build the record (read what `TraceRecorder::new` already takes; the flow/pack/tenant are available at its construction site per the spec).

- [ ] **Step 1: Read `recorder.rs` + the `ExecutionObserver` impl** (`on_node_start`/`on_node_end`/`on_node_error` + the `NodeEvent` fields: `context: &FlowContext` with `tenant`/`pack_id`/`flow_id`/`node_id`/`session_id`, `node`, `payload`). Identify where `duration_ms`/`component_id`/`operation`/error are available in `on_node_end`/`on_node_error` (the `TraceStep` it already builds has them).
- [ ] **Step 2: Write failing tests** — construct a `TraceRecorder` with a test `AuditSink` (its channel exposed via the test ctor from Task 2); drive `on_node_end` with a synthetic `NodeEvent` → assert exactly one message enqueued with `type` ending `node_end` and the right subject; `on_node_start` → zero enqueued; `on_node_error` → one with `node_error` + the error message. Assert the existing file-buffering still records the step (the trace path is unchanged). (Mirror the existing recorder tests for how a `NodeEvent`/`FlowContext` is built in tests.)
- [ ] **Step 3: Run — expect FAIL.**
- [ ] **Step 4: Implement.** Add `audit_sink: Option<AuditSink>` + the captured `TenantCtx`/context to `TraceRecorder`; in `on_node_end`/`on_node_error`, after the existing step buffering, `if let Some(sink) = &self.audit_sink { let rec = NodeAuditRecord{..}; let env = build_audit_event(&rec, Utc::now(), <uuid>); sink.emit(audit_subject(tenant, event), &env); }`. Do not emit in `on_node_start`. Keep the existing constructor working (sink defaults to `None`).
- [ ] **Step 5: Run — expect PASS + commit** (`feat(audit): TraceRecorder emits audit events on node end/error`).

---

### Task 4: NATS client clone-and-thread + construction wiring

**Files:**
- Modify: `crates/greentic-runner-host/src/runtime.rs` (clone the client before the listener-loop move; thread `Option<async_nats::Client>` to the recorder construction site)
- Modify: `crates/greentic-runner-host/src/engine/runtime.rs` (build the `AuditSink` from the threaded client at the `TraceRecorder` construction site; pass it into `TraceRecorder`)
- Test: an integration-style test if the harness supports it; otherwise rely on Tasks 1-3 unit coverage + a compile-level wiring check.

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Read the client construction** (`src/runtime.rs`, the `GREENTIC_EVENTS_NATS_URL` block that builds `async_nats::connect` → `NatsDispatcher::new(client.clone())` → and the response-listener loop that moves the client). Clone the client into an `Option<async_nats::Client>` (`audit_nats_client`) BEFORE the loop consumes it.
- [ ] **Step 2: Thread it** to the `StateMachineRuntime`/`PackFlowAdapter` builder and down to where `TraceRecorder` is constructed (`src/engine/runtime.rs`, the site with the flow `TenantCtx`). Read the current builder signatures and add an `Option<async_nats::Client>` parameter/field along the path.
- [ ] **Step 3: Build the sink at the recorder site:** `let sink = audit_nats_client.clone().map(AuditSink::new); TraceRecorder::new_with_audit(..., sink)` (or pass `None` when absent). When the client is absent, `TraceRecorder` is constructed exactly as before.
- [ ] **Step 4: Verify no default-path change** — with no NATS env, the client is `None`, the sink is `None`, and existing runner tests still pass. Run the runner-host test suite: `cargo test -p greentic-runner-host` (foreground, heavy — wait).
- [ ] **Step 5: Gate + commit.** `cargo fmt --all`; `cargo clippy -p greentic-runner-host --all-targets -- -D warnings`; `cargo test -p greentic-runner-host`. Commit (`feat(audit): thread NATS client to TraceRecorder + wire the audit sink`).

---

## Self-Review

- **Spec coverage:** §2 contract → Task 1 (+ the admin-decode round-trip test); §3.2 sink → Task 2; §3.1 fan-out → Task 3; §3.3 client threading → Task 4; §5 gating/off-by-default → Task 4 Step 4 + Global Constraints; §7 tests → per-task tests (contract, saturation-drop, fan-out on_node_end/start/error). §6 deferred (aw/sorx/sampling) → out of plan.
- **Placeholder scan:** the `/* build TenantCtx */` + "read the real EventEnvelope/TenantCtx" + "read recorder.rs / the client construction" instructions are deliberate — the exact `greentic-types` constructors, the `TraceRecorder::new` signature, and the `runtime.rs` client-move site must be read from the repo (not invented). Every task names its exact file to read. The contract test is fully specified. No TBD left as work-defining.
- **Type consistency:** `NodeAuditRecord`/`Outcome`/`build_audit_event`/`audit_subject` defined Task 1, consumed Tasks 2/3; `AuditSink`/`emit`/`from_sender` Task 2 ↔ Tasks 3/4; `Option<async_nats::Client>` threaded Task 4 → sink ctor Task 2. Subject format `audit.<tenant>.flow.<event>` consistent Task 1 (`audit_subject`) ↔ Task 3 (emit).
- **Scope:** 3 new small files + 3 modified (recorder, 2 runtime wiring); one plan; runner-host crate only; no admin/other-repo change.
