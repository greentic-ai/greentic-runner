# Agentic-worker compute scale-to-zero — design

**Date:** 2026-06-17
**Status:** Approved (design); ready for implementation plan
**Scope owner repo:** `greentic-runner` (engine toggle + JetStream durability). Infra (GKE/KEDA/JetStream server) is a **devops hand-off**.

## Problem

Digital-worker (agentic) execution today runs **in-process** inside the always-on runner replica:
a messaging webhook → `FlowEngine` → `NodeKind::DwAgent` → `RuntimeAgentNodeHandler::execute()` →
`AgentRuntime::step()` (LLM inference + wasmtime tool runtime) all happen in the same process that
serves webhooks (`greentic-runner-host/src/runner/engine.rs:803-824`, `runtime.rs:236-245`). The
heavy, bursty compute is coupled to the front door, so the operator pays for idle compute capacity
even when no worker is running — there is no way to scale the agentic compute to zero.

A decoupled path already exists but is **dormant**: the `agentic.call` node publishes
`greentic.agentic.request.v1` to NATS and an `aw-serve` consumer (`aw-event-bridge::run_bridge`)
handles it (`engine.rs:881-893`, `agent_node.rs:637-653`). It is unused by real flows and uses
**core NATS (fire-and-forget)** — no durability and no queue depth for autoscaling
(`aw-event-bridge/src/lib.rs:153-169`).

## Goal

The agentic compute (LLM + wasmtime) scales **0→N on demand** and bills nothing when idle, **without
changing existing flows/packs** and with **zero regression** for operators who don't opt in. The
webhook/WS front door stays warm (it is light once compute is decoupled).

## Decisions (locked)

1. **Routing = transparent flow-engine toggle.** When a flag is set, `execute_dw_agent` dispatches
   via the existing `agentic.call` NATS path instead of running `AgentRuntime::step()` in-process.
   Flows/packs are unchanged — the node stays `dw.agent`; the engine decides in-process vs NATS.
2. **Transport/platform = NATS JetStream + KEDA on GKE.** Reuse the existing NATS dispatch; add
   JetStream durability so a durable consumer can be scaled to zero by KEDA on consumer lag.

## Architecture

```
Webhook/WS ─▶ [Runner front-door: warm, light]
                  │  FlowEngine reaches NodeKind::DwAgent
                  │  ── toggle ON ─▶ publish greentic.agentic.request.v1 (JetStream) + PAUSE flow
                  ▼
            [NATS JetStream stream + durable pull consumer]   ← KEDA reads lag
                  │
            [aw-serve pool on GKE]  ◀── KEDA scales 0→N on lag
                  │  AgentRuntime::step() (LLM + wasmtime), idempotent
                  ▼
            publish greentic.agentic.response.v1 ─▶ flow RESUME (FlowResumeStore)
            [Redis] state / token-meter / idempotency ledger — always-on (cheap)
```

The only thing that idles to zero is the `aw-serve` pool. The front door stays `minScale≥1` (WS
connections are long-lived) but carries no LLM/wasmtime on the hot path when the toggle is on.

## Components

### A. Transparent dispatch toggle (greentic-runner-host)

- A flag — `GREENTIC_AW_DISPATCH` (`inproc` default | `nats`), resolvable globally and per-tenant —
  controls `execute_dw_agent`. When `nats`, `execute_dw_agent` **reuses the existing `agentic.call`
  machinery**: `remote_dispatch::NatsDispatcher` publish to `greentic.agentic.request.v1`, PAUSE the
  flow via `FlowResumeStore`, and RESUME on `greentic.agentic.response.v1` via
  `dispatch_listener` + `runtime_session_resumer`. When `inproc` (default), behaviour is byte-identical
  to today (`AgentRuntime::step()` in-process) — **zero regression**.
- Correlation + resume reuse the `sorla.call`/`agentic.call` convention already proven in
  `runtime.rs` (correlation id `<session hint>::pack=<id>::flow=<id>` + `::thread`/`::reply` markers).
  The dw.agent node's input (`{user_text}`) maps to `AgentInput` exactly as the in-process path does,
  so the response shape (`{reply, trail, terminated_by}`) is identical.
- Files: `greentic-runner-host/src/runner/engine.rs` (`execute_dw_agent`), `runner/remote_dispatch.rs`,
  `runner/dispatch_listener.rs`, `runner/runtime_session_resumer.rs`, `runtime.rs` (wiring).

### B. JetStream durability (`aw-event-bridge` + `greentic-aw-runtime::serve`)

- Replace core-NATS `subscribe` with a **JetStream stream `greentic-agentic`** bound to
  `greentic.agentic.request.v1` and a **durable pull consumer** (queue group `agentic-workers`),
  with explicit **ack** + `max_deliver`/retry policy. This gives crash-safety (a replica dying
  mid-request redelivers, no lost message) and a durable consumer KEDA can read lag from.
- `aw-serve` becomes a pull-consumer worker; `run_bridge` uses `pull_subscribe` + ack instead of
  fire-and-forget `subscribe`.
- Files: `aw-event-bridge/src/lib.rs` (`run_bridge`), `greentic-aw-runtime/src/serve.rs`.
- Backward-compat: keep a feature/flag so the core-NATS path still works for non-JetStream
  deployments, or gate JetStream on a `GREENTIC_AW_JETSTREAM` env — TBD in the plan, default to
  JetStream when a stream is configured.

### C. At-least-once / idempotency

- JetStream redelivery is at-least-once, so a `step()` must be safe to repeat. **Reuse the existing
  `RedisToolLedger`** (idempotency ledger already wired into `AgentRuntime`) keyed by the dispatch
  correlation id, so a redelivered request does not double-execute tools/LLM. Dedupe at the serve
  entry (`RuntimeAgentDispatchInvoker`) before invoking `step()`.

### D. Cold-start mitigation

- Bake the **cwasm cache** into the `aw-serve` image and warm the pack cache at start (reuse the
  existing auto-adopt-cwasm behaviour). Without this, the first request after each scale-up pays the
  full wasmtime-instantiate + `.gtpack`-load cost. KEDA `minReplicaCount: 0` with an `activationLag`
  threshold avoids flapping.

### E. Front door stays warm

- The webhook/WS ingress keeps `minScale≥1` (WS is long-lived; cannot scale to zero). This is correct
  — only the compute scales to zero. No change beyond confirming the front door no longer carries the
  agentic compute when the toggle is on.

## Infra hand-off (devops — not runner code)

Documented as acceptance for devops, not implemented here:
- GKE `Deployment` for `aw-serve` with `minReplicas: 0`.
- **KEDA `ScaledObject`** using the NATS-JetStream scaler on the `agentic-workers` consumer's
  pending/lag metric (`minReplicaCount: 0`, sensible `cooldownPeriod`).
- JetStream server: stream `greentic-agentic` (subject `greentic.agentic.request.v1`), retention +
  replicas per durability needs.
- Env on `aw-serve`: `GREENTIC_EVENTS_NATS_URL`, `GREENTIC_AW_REDIS_URL`, LLM key, agent manifests
  source (`GREENTIC_AGENT_MANIFESTS_DIR` or admin), cwasm cache path.

## Testing

- **Engine toggle**: unit — `inproc` path unchanged (existing tests stay green); `nats` path publishes
  `greentic.agentic.request.v1` + pauses the flow (assert via the existing dispatch test harness).
- **JetStream**: durability — a nacked/crash-interrupted message is redelivered; idempotency — a
  duplicated correlation id yields a single tool/LLM effect (assert via `RedisToolLedger`).
- **e2e**: message → publish → `aw-serve` (the existing `aw-serve` `test-mock` bin) → response →
  flow resume produces the same `{reply, trail, terminated_by}` as the in-process path.

## Decomposition (3 runner PRs + 1 handoff)

1. **JetStream durability** in `aw-event-bridge`/`serve` (prerequisite; self-contained; can land
   first without touching the engine).
2. **Engine toggle** `dw.agent` → `agentic.call` dispatch + idempotency dedupe at serve entry.
3. **Cold-start**: cwasm bake + pack warm in the `aw-serve` image build.
4. **Infra hand-off doc**: GKE + KEDA + JetStream config (devops executes).

## Risks / open items

1. **Toggle correctness is the biggest lift** — pause/resume + session-resume correlation must work
   for `dw.agent` exactly as it already does for `sorla.call`. Verify the `dw.agent` node's
   await/resume semantics match (it currently returns synchronously; the NATS path makes it a
   pausing node).
2. **JetStream vs core-NATS coexistence** — decide whether JetStream replaces or is gated alongside
   the core-NATS path (default-on when a stream exists).
3. **GKE is new infra** if the platform is Cloud-Run-only today — devops dependency.
4. **Idempotency key** must be stable across redelivery (use the dispatch correlation id, not a
   per-attempt id).
5. **Front door cannot scale to zero** (WS) — savings come only from the compute pool.
