# Agentic-worker scale-to-zero — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the agentic compute (LLM + wasmtime) scale 0→N on demand by routing `dw.agent` execution over a durable NATS JetStream path served by a separately-scalable `aw-serve` pool — with zero regression when the toggle is off.

**Architecture:** Reuse the already-wired `agentic.call` machinery (publish → pause flow → JetStream → `aw-serve` consumer → response → resume). Three runner PRs: (1) JetStream durability in the consumer, (2) a flag that reroutes `dw.agent` into that path, (3) cold-start warmup + a live e2e. Infra (GKE/KEDA/JetStream server) is a devops hand-off.

**Tech Stack:** Rust, `async-nats 0.46` (JetStream module), `greentic-types::runtime_dispatch` contract, Redis (idempotency), `aw-serve` test-mock bin.

**Spec:** `docs/superpowers/specs/2026-06-17-agentic-worker-scale-to-zero-design.md`

**Key facts (verified from source — do not re-derive):**
- The `agentic.call` path is fully built: `NodeKind::AgenticCall` → `execute_agentic_call` → `execute_remote_dispatch(ctx, "agentic", target, payload)` (engine.rs:881-1006); pause via `DispatchOutcome::wait`; `runtime.rs:236-354` spawns a response listener for `"agentic"` + wires `NatsDispatcher` when `GREENTIC_EVENTS_NATS_URL` is set; `RuntimeSessionResumer` handles resume. **The in-process `dw.agent` path (`execute_dw_agent`, engine.rs:802-837) is what real flows use today.**
- `execute_remote_dispatch` reads the node payload's `{await, operation, deadline_ms, input}` and wraps `input` into `RuntimeDispatchRequest`. It builds the correlation id with `::pack=/::flow=/::thread=/::reply=` markers the resumer parses back.
- Consumer: `aw-event-bridge::run_bridge` uses **core NATS** `client.subscribe(request_topic("agentic"))` (= `greentic.agentic.request.v1`) — no JetStream, no ack (aw-event-bridge/src/lib.rs).
- Serve: `greentic_aw_runtime::serve::{serve, RuntimeAgentDispatchInvoker, build_test_mock_runtime}`; `aw-serve` bin runs the test-mock runtime.
- Idempotency primitive: `ToolLedger` (`greentic-aw-runtime/src/tools.rs`) `get/record` keyed by `{tenant.key_prefix()}:{session}:tool_calls:{call_id}` — per-tool-call. A dispatch-level dedup keyed by correlation id is NOT present yet.
- Flag house-style: `should_serve_agentic_inproc(get_env)` — a **pure fn over a `get_env` closure**, truthy = `1|true|yes|on`.
- `async-nats 0.46` JetStream: `async_nats::jetstream::new(client)` → context; `get_or_create_stream(stream::Config)`, `create_consumer`/`get_or_create_consumer(consumer::pull::Config)`, `consumer.messages()` stream, `msg.ack().await`, `msg.double_ack().await`.

**Scope note:** 3 runner PRs (PR1 JetStream, PR2 toggle+idempotency, PR3 cold-start+e2e) + 1 infra hand-off doc. PR1 is self-contained and lands first.

---

## File structure

| File | Responsibility | PR |
| --- | --- | --- |
| `crates/aw-event-bridge/src/jetstream.rs` (new) | JetStream stream + durable pull consumer setup; `run_bridge_jetstream` | 1 |
| `crates/aw-event-bridge/src/lib.rs` (modify) | gate: JetStream when configured, else core-NATS `run_bridge` | 1 |
| `crates/aw-event-bridge/Cargo.toml` (modify) | no dep change (jetstream is in async-nats 0.46); confirm | 1 |
| `crates/greentic-runner-host/src/runner/agent_node.rs` (modify) | `dw_agent_dispatch_mode()` flag resolver | 2 |
| `crates/greentic-runner-host/src/runner/engine.rs` (modify) | `dispatch_node` DwAgent arm: branch in-process vs remote-dispatch | 2 |
| `crates/greentic-aw-runtime/src/serve.rs` (modify) | dispatch-level idempotency in `RuntimeAgentDispatchInvoker` | 2 |
| `crates/greentic-aw-runtime/src/dispatch_ledger.rs` (new) | Redis dispatch-result cache keyed by correlation id | 2 |
| `crates/greentic-runner-host/tests/agentic_scale_to_zero_e2e.rs` (new) | live e2e: dw.agent (toggle on) → NATS → aw-serve → resume | 3 |
| `crates/greentic-aw-runtime/src/serve.rs` (modify) | warm-on-start hook (cwasm/pack) | 3 |
| `docs/runbooks/aw-scale-to-zero-infra.md` (new) | GKE + KEDA + JetStream hand-off for devops | 3 |

---

## PR1 — JetStream durability in the consumer

### Task 1.1: JetStream stream + durable pull consumer setup (pure config)

**Files:** Create `crates/aw-event-bridge/src/jetstream.rs`; `pub mod jetstream;` in `lib.rs`.
Before cargo: `export CARGO_TARGET_DIR="$HOME/.cache/greentic-target/runner"`.

- [ ] **Step 1: Write the failing test** (config builders are pure — assert names/subjects)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stream_config_binds_agentic_request_subject() {
        let cfg = agentic_stream_config();
        assert_eq!(cfg.name, "greentic-agentic");
        assert!(cfg.subjects.iter().any(|s| s == "greentic.agentic.request.v1"));
    }
    #[test]
    fn consumer_config_is_durable_explicit_ack() {
        let cfg = agentic_consumer_config();
        assert_eq!(cfg.durable_name.as_deref(), Some("agentic-workers"));
        assert!(matches!(cfg.ack_policy, async_nats::jetstream::consumer::AckPolicy::Explicit));
    }
}
```

- [ ] **Step 2: Run → fail** `cargo test -p aw-event-bridge jetstream -- --nocapture` (functions missing).

- [ ] **Step 3: Implement the pure config builders + setup fn**

```rust
//! JetStream durability for the agentic dispatch consumer: a `greentic-agentic`
//! stream binds `greentic.agentic.request.v1`, and a durable pull consumer
//! `agentic-workers` (explicit ack) lets `aw-serve` replicas share the queue and
//! lets KEDA scale on consumer lag. Replaces the core-NATS fire-and-forget path
//! when JetStream is configured.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer, stream};
use greentic_types::request_topic;
use crate::RUNTIME_NAME;

pub const STREAM_NAME: &str = "greentic-agentic";
pub const DURABLE_CONSUMER: &str = "agentic-workers";

#[must_use]
pub fn agentic_stream_config() -> stream::Config {
    stream::Config {
        name: STREAM_NAME.to_string(),
        subjects: vec![request_topic(RUNTIME_NAME)], // greentic.agentic.request.v1
        retention: stream::RetentionPolicy::WorkQueue,
        ..Default::default()
    }
}

#[must_use]
pub fn agentic_consumer_config() -> consumer::pull::Config {
    consumer::pull::Config {
        durable_name: Some(DURABLE_CONSUMER.to_string()),
        ack_policy: consumer::AckPolicy::Explicit,
        // Redeliver a few times on crash before parking; the invoker is idempotent
        // (PR2) so redelivery is safe.
        max_deliver: 5,
        ..Default::default()
    }
}

/// Ensure the stream + durable pull consumer exist; return the consumer handle.
pub async fn ensure_consumer(
    client: &async_nats::Client,
) -> Result<consumer::Consumer<consumer::pull::Config>> {
    let js = jetstream::new(client.clone());
    let stream = js
        .get_or_create_stream(agentic_stream_config())
        .await
        .context("get_or_create greentic-agentic stream")?;
    let consumer = stream
        .get_or_create_consumer(DURABLE_CONSUMER, agentic_consumer_config())
        .await
        .context("get_or_create agentic-workers consumer")?;
    Ok(consumer)
}
```

(Adjust `stream::Config`/`consumer::pull::Config` field names to async-nats 0.46 exactly — if `max_deliver` is `i64`, cast; if `RetentionPolicy::WorkQueue` is the variant name, keep; the test pins the contract.)

- [ ] **Step 4: Run → pass** `cargo test -p aw-event-bridge jetstream -- --nocapture`.

- [ ] **Step 5: Commit** `git add crates/aw-event-bridge/src/jetstream.rs crates/aw-event-bridge/src/lib.rs && git commit -m "feat(aw-bridge): JetStream stream + durable consumer config"`

### Task 1.2: `run_bridge_jetstream` (pull + ack loop, reuse `handle_message`)

**Files:** Modify `crates/aw-event-bridge/src/jetstream.rs` + `lib.rs`.

- [ ] **Step 1: Write the failing test** — the message-handling core is already covered by the existing `build_response`/`handle_message` tests; add a test that `ack` is called after a successful handle. Use a small fake: extract the per-message body into a pure `handle_jetstream_message(client, invoker, payload, headers) -> Result<()>` that reuses `build_response`, and test it maps a request to a published response (mirror the existing `handle_invokes_and_maps_agent_output_to_response` test but for the JetStream message shape). Assert the response is built (ack is an I/O side-effect verified in the e2e, PR3).

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement the pull loop**, reusing the existing `handle_message` body (decode headers → `build_response` → publish response). Per the async-nats 0.46 pull API:

```rust
pub async fn run_bridge_jetstream(
    client: async_nats::Client,
    invoker: std::sync::Arc<dyn crate::AgentDispatchInvoker>,
) -> Result<()> {
    use futures_util::StreamExt;
    let consumer = ensure_consumer(&client).await?;
    let mut messages = consumer.messages().await.context("open jetstream messages")?;
    while let Some(item) = messages.next().await {
        let msg = item.context("jetstream message")?;
        let client = client.clone();
        let invoker = invoker.clone();
        // Process then ACK. On handler error, do NOT ack → JetStream redelivers
        // (up to max_deliver); the invoker is idempotent so redelivery is safe.
        tokio::spawn(async move {
            // `jetstream::Message` derefs to the core message (payload + headers).
            match crate::handle_message(&client, invoker, msg.message.clone()).await {
                Ok(()) => {
                    if let Err(error) = msg.ack().await {
                        tracing::error!(%error, "jetstream ack failed");
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "agentic handler failed; leaving unacked for redelivery");
                }
            }
        });
    }
    Ok(())
}
```

(Confirm the 0.46 type: `consumer.messages()` yields `Result<jetstream::Message>`; `jetstream::Message` exposes `.message` (the core `async_nats::Message`) + `.ack()`. Adjust `handle_message`'s signature if it needs `&async_nats::Message` vs owned — it currently takes `async_nats::Message`.)

- [ ] **Step 4: Run → pass** + `cargo build -p aw-event-bridge`.

- [ ] **Step 5: Commit** `feat(aw-bridge): JetStream pull+ack consumer loop`.

### Task 1.3: Gate JetStream vs core-NATS in `serve`

**Files:** Modify `crates/greentic-aw-runtime/src/serve.rs` (`serve` fn) + `aw-event-bridge/src/lib.rs` (re-export `run_bridge_jetstream`).

- [ ] **Step 1: Write the failing test** for a pure gate `fn use_jetstream(get_env) -> bool` (truthy `GREENTIC_AW_JETSTREAM`, default ON when a stream is desired — choose default: **ON**, with `GREENTIC_AW_JETSTREAM=0` to force core-NATS for legacy):

```rust
#[test]
fn jetstream_default_on_unless_disabled() {
    assert!(use_jetstream(|_| None));                       // default ON
    assert!(!use_jetstream(|k| (k=="GREENTIC_AW_JETSTREAM").then(|| "0".into())));
    assert!(use_jetstream(|k| (k=="GREENTIC_AW_JETSTREAM").then(|| "on".into())));
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** `use_jetstream` (pure) + branch in `serve()`:

```rust
pub fn use_jetstream(get_env: impl Fn(&str) -> Option<String>) -> bool {
    match get_env("GREENTIC_AW_JETSTREAM") {
        Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        None => true, // default ON
    }
}

pub async fn serve(nats_url: &str, runtime: Arc<AgentRuntime>) -> Result<()> {
    let client = async_nats::connect(nats_url).await.with_context(|| format!("connecting to NATS at {nats_url}"))?;
    let invoker = Arc::new(RuntimeAgentDispatchInvoker::new(runtime));
    if use_jetstream(|k| std::env::var(k).ok()) {
        tracing::info!(nats_url, "aw serve: JetStream durable consumer");
        aw_event_bridge::run_bridge_jetstream(client, invoker).await
    } else {
        tracing::info!(nats_url, "aw serve: core-NATS consumer (legacy)");
        aw_event_bridge::run_bridge(client, invoker).await
    }
}
```

- [ ] **Step 4: Run → pass** + `cargo build -p greentic-aw-runtime --features serve`.

- [ ] **Step 5: Commit** `feat(aw-runtime): serve gates JetStream (default) vs core-NATS`.

---

## PR2 — Engine toggle: `dw.agent` → durable agentic dispatch

### Task 2.1: `dw_agent_dispatch_mode` flag resolver (pure)

**Files:** Modify `crates/greentic-runner-host/src/runner/agent_node.rs` (add near `should_serve_agentic_inproc`).

- [ ] **Step 1: Failing test** (mirror the house pure-over-closure style):

```rust
#[test]
fn dw_agent_dispatch_mode_defaults_inproc_and_parses_nats() {
    assert_eq!(dw_agent_dispatch_mode(|_| None), DwAgentDispatch::InProcess);
    assert_eq!(dw_agent_dispatch_mode(|k| (k=="GREENTIC_AW_DISPATCH").then(|| "nats".into())), DwAgentDispatch::Nats);
    assert_eq!(dw_agent_dispatch_mode(|k| (k=="GREENTIC_AW_DISPATCH").then(|| "inproc".into())), DwAgentDispatch::InProcess);
}
```

- [ ] **Step 2: Run → fail** `cargo test -p greentic-runner-host dw_agent_dispatch_mode`.

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DwAgentDispatch { InProcess, Nats }

/// Resolve how `dw.agent` nodes execute. `GREENTIC_AW_DISPATCH=nats` routes them
/// over the durable agentic NATS path (scale-to-zero compute); anything else
/// (incl. unset) keeps the in-process path — zero regression by default.
pub fn dw_agent_dispatch_mode(get_env: impl Fn(&str) -> Option<String>) -> DwAgentDispatch {
    match get_env("GREENTIC_AW_DISPATCH").as_deref().map(str::trim).map(str::to_ascii_lowercase) {
        Some(ref m) if m == "nats" => DwAgentDispatch::Nats,
        _ => DwAgentDispatch::InProcess,
    }
}
```

- [ ] **Step 4: Run → pass.** **Step 5: Commit** `feat(runner): GREENTIC_AW_DISPATCH flag resolver`.

### Task 2.2: Route `dw.agent` through `execute_remote_dispatch` when flag=nats

**Files:** Modify `crates/greentic-runner-host/src/runner/engine.rs` (`dispatch_node` DwAgent arm + a small helper). The engine needs the resolved mode — resolve once at engine construction and store `dw_agent_dispatch: DwAgentDispatch` on `FlowEngine` (set where the engine is built in `runtime.rs`), so `dispatch_node` reads `self.dw_agent_dispatch`.

- [ ] **Step 1: Failing test** — a unit test on the DwAgent arm behavior. Since `execute_remote_dispatch` needs a `RemoteDispatchHandler`, use the existing engine test harness (there are dw.agent tests at engine.rs:3305+). Add: with `dw_agent_dispatch = Nats` and a mock `RemoteDispatchHandler` recording the dispatch, a `dw.agent` node publishes a dispatch with `runtime="agentic"`, `target=<agent_id>`, and the node payload wrapped as `input`, and returns a `DispatchOutcome::wait` (pending). With `InProcess` (default), the existing in-process behavior is unchanged (existing tests stay green).

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** — change the `NodeKind::DwAgent` arm:

```rust
NodeKind::DwAgent { agent_id } => match self.dw_agent_dispatch {
    DwAgentDispatch::Nats => {
        // Reroute to the durable out-of-process agentic path. Wrap the raw
        // node payload as the dispatch `input` (the serve invoker reads
        // input.user_text). await=true → pause+resume, identical to agentic.call.
        let remote_payload = serde_json::json!({ "await": true, "input": payload });
        self.execute_remote_dispatch(ctx, "agentic", agent_id, remote_payload).await
    }
    DwAgentDispatch::InProcess => self
        .execute_dw_agent(ctx, agent_id, payload)
        .await
        .map(DispatchOutcome::complete),
},
```

Add `dw_agent_dispatch: DwAgentDispatch` field to `FlowEngine` + a setter/constructor param; in `runtime.rs` set it via `dw_agent_dispatch_mode(|k| std::env::var(k).ok())`. Import `DwAgentDispatch` from `agent_node`.

IMPORTANT guard: when `Nats` but no `remote_dispatch_handler` is configured (NATS URL unset), `execute_remote_dispatch` already errors with a clear context — but to avoid a footgun, log a startup warning in `runtime.rs` if `dw_agent_dispatch == Nats && GREENTIC_EVENTS_NATS_URL` unset. Add that warning.

- [ ] **Step 4: Run → pass** (new test + all existing engine/dw.agent tests green) + `cargo build -p greentic-runner-host`.

- [ ] **Step 5: Commit** `feat(runner): route dw.agent over durable NATS when GREENTIC_AW_DISPATCH=nats`.

### Task 2.3: Dispatch-level idempotency (Redis cache keyed by correlation id)

**Files:** Create `crates/greentic-aw-runtime/src/dispatch_ledger.rs`; modify `serve.rs` `RuntimeAgentDispatchInvoker::invoke`.

Rationale: JetStream redelivery (consumer crash after `step()` before `ack`) would otherwise re-run the LLM. Cache the dispatch `output` by `idempotency_key` (= correlation id) so a redelivered request returns the cached response without re-running the step. Mirror `ToolLedger` (Redis `set_ex`).

- [ ] **Step 1: Failing test** — a `DispatchLedger` trait `get(key)->Option<Value>` / `record(key, Value)`, with a `NoopDispatchLedger` (test) and the invoker checking it: a second `invoke` with the same `idempotency_key` returns the cached output and does NOT call `runtime.step` again. Use a counting fake runtime/ledger to assert step-call-count == 1 across two invokes.

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement** `DispatchLedger` (trait + `RedisDispatchLedger` using the same `ConnectionManager` pattern as `RedisToolLedger`, key `aw:dispatch:{idempotency_key}`, TTL e.g. 1h, `NoopDispatchLedger` for tests). Thread an `Option<Arc<dyn DispatchLedger>>` into `RuntimeAgentDispatchInvoker::new(runtime, ledger)`; in `invoke`, if `Some(key)` and `ledger.get(key)` hits → return cached `InvokeOutcome`; else run `step`, `ledger.record(key, output)`, return. Wire the production ledger in `serve_agentic`/`build_agent_runtime` (reuse the Redis manager already built there); `build_test_mock_runtime` + `aw-serve` use `NoopDispatchLedger`.

- [ ] **Step 4: Run → pass** + `cargo build -p greentic-aw-runtime --features serve,test-mock`.

- [ ] **Step 5: Commit** `feat(aw-runtime): dispatch-level idempotency for at-least-once redelivery`.

---

## PR3 — Cold-start warmup + live e2e + infra hand-off

### Task 3.1: Warm-on-start hook (cwasm/pack) in `serve`

**Files:** Modify `crates/greentic-aw-runtime/src/serve.rs` (call a warm step before `run_bridge*`).

- [ ] **Step 1: Failing test** — a pure `fn warm_targets(get_env) -> Vec<String>` reading `GREENTIC_AW_WARM_PACKS` (comma-sep) returns the parsed list (empty when unset). Assert parsing.
- [ ] **Step 2: Run → fail.**
- [ ] **Step 3: Implement** the parser + a best-effort `async fn warm(...)` that, for each target, triggers the existing cwasm/pack cache load (reuse the runner-host cache warm path if exposed; otherwise log "warm: <n> packs" as a no-op seam the image build relies on — the real cwasm bake is in the Dockerfile, Task 3.3). Call `warm(...)` at the top of `serve()` (non-fatal on error).
- [ ] **Step 4: Run → pass.** **Step 5: Commit** `feat(aw-runtime): warm-on-start seam for cold-start mitigation`.

### Task 3.2: Live e2e — dw.agent (toggle=nats) → aw-serve → resume

**Files:** Create `crates/greentic-runner-host/tests/agentic_scale_to_zero_e2e.rs` (`#[ignore]` by default; needs a NATS server).

- [ ] **Step 1: Write the test** (ignored; skips when `GREENTIC_EVENTS_NATS_URL` unset):

```rust
//! Live e2e: with GREENTIC_AW_DISPATCH=nats, a dw.agent node publishes to NATS,
//! the aw-serve (test-mock) consumer replies, and the paused flow resumes with
//! the canned reply. Run with a local NATS (JetStream-enabled) + `aw-serve`:
//!   nats-server -js &
//!   GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 AW_SERVE_REPLY=pong \
//!     cargo run -p greentic-aw-runtime --features serve,test-mock --bin aw-serve &
//!   GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 \
//!     cargo test -p greentic-runner-host --test agentic_scale_to_zero_e2e -- --ignored --nocapture
#[tokio::test]
#[ignore]
async fn dw_agent_over_nats_resumes_with_reply() {
    let Ok(nats) = std::env::var("GREENTIC_EVENTS_NATS_URL") else { eprintln!("skip: no NATS"); return; };
    // Build a minimal FlowEngine with dw_agent_dispatch=Nats + a NatsDispatcher,
    // a one-node flow `dw.agent.greeter`, run an inbound activity, assert the
    // resumed output reply == "pong" (the aw-serve AW_SERVE_REPLY).
    // (Use the existing engine test harness builders; agent_id "greeter" matches
    //  build_test_mock_runtime's default registration.)
    let _ = nats; // wired by the harness
}
```

Flesh out using the existing engine test harness (the dw.agent tests at engine.rs:3305+ show how to build a `FlowEngine` + run a node). The assertion: the flow pauses on dispatch, the aw-serve reply arrives on `greentic.agentic.response.v1`, the resumer re-enters the flow, and the final reply is `pong`.

- [ ] **Step 2: Run (with NATS + aw-serve up)** per the doc-comment commands → PASS. If no local NATS, it skips (still compiles).
- [ ] **Step 3: Commit** `test(runner): live e2e for dw.agent scale-to-zero NATS path (ignored)`.

### Task 3.3: Infra hand-off doc (devops) + image cwasm bake note

**Files:** Create `docs/runbooks/aw-scale-to-zero-infra.md`.

- [ ] **Step 1: Write the runbook** covering (concrete, for devops):
  - JetStream server: stream `greentic-agentic` (WorkQueue, subject `greentic.agentic.request.v1`), durable consumer `agentic-workers` (explicit ack, max_deliver 5) — created automatically by `ensure_consumer` on first `aw-serve` start, but document the expected config + replication for prod.
  - GKE `Deployment` for `aw-serve` (`cargo run ... --bin aw-serve` image, or the production serve binary) with `minReplicas: 0`; env: `GREENTIC_EVENTS_NATS_URL`, `GREENTIC_AW_REDIS_URL`, LLM key, `GREENTIC_AGENT_MANIFESTS_DIR`, `GREENTIC_AW_JETSTREAM=on`, cwasm cache path.
  - **KEDA `ScaledObject`** using the NATS-JetStream scaler on the `agentic-workers` consumer's pending count (`minReplicaCount: 0`, `activationLagThreshold`, `cooldownPeriod`).
  - Front door (webhook/WS runner) keeps `minScale≥1` + set `GREENTIC_AW_DISPATCH=nats` so it offloads compute.
  - cwasm bake: build the `aw-serve` image with the cwasm cache pre-populated (reuse the existing warmup/auto-adopt-cwasm mechanism) so cold starts are fast.
- [ ] **Step 2: Commit** `docs(runbook): GKE+KEDA+JetStream hand-off for aw scale-to-zero`.

---

## Self-review (author)

- **Spec coverage:** Component A (toggle) → PR2.1/2.2; B (JetStream) → PR1; C (idempotency) → PR2.3; D (cold-start) → PR3.1/3.3; E (front-door warm) → PR3.3 doc. Decomposition matches the spec's 3-PR + handoff. ✓
- **Reuse:** PR2 leans entirely on the already-wired `agentic.call` machinery (execute_remote_dispatch + listener + resumer) — no new dispatch/pause/resume code. The biggest spec risk ("toggle is the biggest lift") is reduced to a payload-wrap + arm-branch + flag.
- **At-least-once:** PR1 leaves a failed handle unacked → redeliver; PR2.3 makes redelivery safe via the dispatch ledger. Consistent.
- **Backward-compat:** `GREENTIC_AW_DISPATCH` defaults InProcess (zero regression); `GREENTIC_AW_JETSTREAM` defaults ON but `serve` falls back to core-NATS when `=0`.
- **async-nats 0.46 field names** (stream::Config/consumer::pull::Config, `messages()`, `ack()`, `jetstream::Message.message`) are the one area to verify against the exact 0.46 API during PR1 — the tests pin the intended contract; adjust field spellings if the crate differs.
- **Open:** the production `serve` binary (vs the `aw-serve` test-mock) — the runbook assumes `aw-serve`; for real LLM/Redis the production serve path is `greentic-runner-host` `serve_agentic` (already built). Note in PR3 which image devops ships (likely a thin prod `aw-serve` equivalent wired to `build_agent_runtime`).
