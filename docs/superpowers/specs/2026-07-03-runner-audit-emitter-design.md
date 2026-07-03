# EPIC-B v2 (slice B-2) — Runner Flow-Step Audit Emitter — Design Spec

**Status:** Draft — 2026-07-03
**Initiative:** Agentic platform coverage PRD, EPIC-B "durable queryable audit". This slice makes the audit store capture **runtime flow activity** by having greentic-runner emit a per-node audit event that the (already-shipped) greentic-admin NATS `audit.>` subscriber ingests.

## 1. Problem & goal

EPIC-B v1 shipped an admin `audit_activity` store + an **inert** NATS `audit.>` subscriber (no publishers exist yet) + a dual-write from designer telemetry. So today the store only sees designer-authoring events, not what the runtime actually did. This slice adds the first real runtime emitter: **greentic-runner publishes a best-effort audit event per flow node execution** (end + error), which the admin subscriber decodes into `audit_activity` rows — no admin change.

## 2. The cross-repo contract (correctness crux)

The admin subscriber `audit_ingest::activity_from_envelope` (greentic-admin, on research) decodes a JSON body with exactly these fields:
- `tenant.tenant` (fallback: a top-level `tenant` string) → tenant id
- `type` → event_type
- `source`
- `subject`
- `time` → occurred_at
- `correlation_id`
- `payload`

Both repos depend on `greentic_types::{EventEnvelope, TenantCtx}`, so a serialized `EventEnvelope` already produces this shape (`r#type` → `type`; `tenant: TenantCtx` → `{tenant, env, ...}`). The emitter constructs an `EventEnvelope` directly (not via `BusinessEventBuilder`, avoiding its business-event `cap://` type + required-metadata constraints), with:
- `type = "greentic.runner.flow.node_end"` | `"greentic.runner.flow.node_error"` (plain dotted, mirroring the admin test's `sorx.endpoint.invoked`).
- `source = "runner"`.
- `tenant = <the flow's TenantCtx>`.
- `subject = "flow:<flow_id>/node:<node_id>"`.
- `time = Utc::now()`.
- `correlation_id = <session id>`.
- `payload = { node_id, component_id, operation, outcome: "ok"|"error", duration_ms, error?: <message> }` — redaction is the admin subscriber's job (it scrubs on ingest), so the runner sends the structured payload as-is.

**A round-trip test in the runner replicates the admin decode logic** (the same field reads) against a constructed+serialized envelope, so the contract is locked on the producer side and can't silently drift.

## 3. Architecture

Two seams already exist and are reused; the design deliberately avoids restructuring the engine's observer API.

### 3.1 Reuse the existing `ExecutionObserver` via `TraceRecorder` fan-out
The engine's per-node hook is `ExecutionObserver` (`on_node_start`/`on_node_end`/`on_node_error`), and `TraceRecorder` (`src/trace/recorder.rs`) is the single production impl, attached at `src/engine/runtime.rs` where the full flow `TenantCtx` is in scope. Rather than change the engine's single observer slot to a list (an API touch), **`TraceRecorder` gains an optional `AuditSink`** and, in `on_node_end`/`on_node_error`, additionally emits an audit `EventEnvelope` — in addition to its existing file-buffering. When no sink is present (no NATS), `TraceRecorder` behaves exactly as today.

### 3.2 `AuditSink` — best-effort, non-blocking, drop-on-failure
A new `AuditSink` (mirror the sorx `NatsEventSink` pattern): a synchronous `emit(EventEnvelope)` pushes onto a **bounded channel (capacity 1024)** and returns immediately; a tokio background task drains the channel, serializes, and publishes to NATS subject `audit.<tenant>.flow.<event>`. A full channel drops the event (at-most-once) with a warn + counter; a publish error warns and continues. A NATS failure NEVER affects flow execution. The sink is constructed only when a NATS client is available (see §3.3), else it is `None`.

### 3.3 NATS client threading (the load-bearing change)
The connected `async_nats::Client` (built at `src/runtime.rs` gated on `GREENTIC_EVENTS_NATS_URL`) is currently consumed by the response-listener loop and otherwise private to `NatsDispatcher`. This slice **clones the client before that move** and threads `Option<async_nats::Client>` down to where `TraceRecorder` is constructed, so the `AuditSink` can be built from the clone. No new dependency (`async-nats` + `greentic-types` are already direct deps of `greentic-runner-host`).

## 4. Data flow
```
node executes → ExecutionObserver::on_node_end/on_node_error (existing hook)
  → TraceRecorder buffers the TraceStep (existing file path, unchanged)
  → if AuditSink present: build EventEnvelope → sink.emit() (push to bounded channel, non-blocking)
       → background drain task → publish audit.<tenant>.flow.<event> → admin audit.> subscriber → audit_activity row
```

## 5. Gating & failure semantics
- **Off by default:** with no `GREENTIC_EVENTS_NATS_URL`, no client, no `AuditSink`, zero behavior change. (Optionally a second env `GREENTIC_AUDIT_EMIT=1` to enable independently of the dispatch wiring — v1 reuses the existing NATS gate to stay simple; documented so it can be split later.)
- **Never blocks/fails execution:** bounded-channel push is non-blocking; full → drop + counter; publish error → warn. Mirrors the trace recorder's existing best-effort flush.
- **Volume control:** emit on `on_node_end` + `on_node_error` only (NOT `on_node_start`) to halve volume. Per-node granularity is intended (the audit store is per-event). Sampling/rate-limiting is a follow-up if volume proves high.

## 6. Scope boundaries (YAGNI)
**In v1:** per-node flow audit emit (end + error) → NATS `audit.<tenant>.flow.<event>`, best-effort bounded-channel sink, the cross-repo contract round-trip test, NATS-client clone-and-thread, gating off-by-default.

**Deferred:**
- Agentic-worker (`dw.agent`) step emit — `greentic-aw-runtime` has its own `StepObserver` seam (separate from the flow `ExecutionObserver`); a parallel emitter there is slice B-3.
- SoRX/OperaX emitters (their runtimes live in separate repos; the sorx design already has a `NatsEventSink` to point at `audit.>`).
- Sampling / rate-limiting / per-tenant enable flags.
- `on_node_start` emit (begin/end pairing) and richer payload (input/output hashes are in `TraceStep` but omitted here to keep payloads small + non-sensitive).
- A dedicated `GREENTIC_AUDIT_EMIT` gate independent of the dispatch NATS URL.

## 7. Testing
- **Contract round-trip (the crux):** build the audit `EventEnvelope` for a sample node-end, `serde_json::to_value`, then run the SAME field reads the admin `activity_from_envelope` performs (`tenant.tenant`, `type`, `source`, `subject`, `time`, `correlation_id`, `payload`) and assert each resolves to the expected value — proving an admin subscriber would produce a correct row.
- **Sink best-effort:** a full bounded channel drops without blocking/erroring; `emit` never returns an error; no NATS → sink is `None` and `TraceRecorder` is unchanged.
- **Observer fan-out:** with a sink present, `on_node_end` enqueues exactly one event; `on_node_start` enqueues none; `on_node_error` enqueues one with `outcome:"error"` + the error message. The existing trace-file behavior is unchanged (a test that the file path still works with a sink attached).
- **Subject:** `audit.<tenant>.flow.node_end` is well-formed for a tenant id.

## 8. Rollout
- Additive; off unless NATS is configured; no admin change (the subscriber already exists). Target branch `research`.
- Follow-ups: B-3 (agentic-worker step emit), the SoRX/OperaX emitters, sampling, and the independent audit gate.
