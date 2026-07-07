# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

greentic-runner is the production runtime for the Greentic platform. It loads `.gtpack` archives containing WebAssembly components and flow definitions, exposes HTTP ingress adapters for messaging providers (Telegram, Teams, Slack, WebChat, Webex, WhatsApp, generic webhook, timer/cron), and executes flows with pause/resume session semantics. The workspace produces three binaries (`greentic-runner`, `greentic-runner-cli`, `greentic-gen-bindings`) and a library crate (`greentic_runner`).

## Build & Test

```bash
# Full local CI mirror (fmt → clippy → tests → package dry-run)
ci/local_check.sh

# Individual steps
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features

# Single test
cargo test -- test_name_here

# Crate-scoped tests
cargo test -p greentic-runner
cargo test -p greentic-runner-host

# Host smoke test (requires example packs)
RUN_HOST=always ci/local_check.sh

# Conformance suite
RUN_CONFORMANCE=1 ci/local_check.sh

# Heavy WASM fixture tests
GREENTIC_HEAVY_WASM=1 cargo test --workspace

# Select specific CI steps
LOCAL_CHECK_STEPS=fmt,clippy ci/local_check.sh
```

`ci/local_check.sh` steps: `fmt`, `clippy`, `host_smoke`, `crate_tests`, `workspace_tests`, `conformance`, `package`.

## Workspace Layout

```
crates/
  greentic-runner/           # Binary + public library (thin CLI wrapper)
  greentic-runner-host/      # Core runtime (~9K lines): pack loading, flow engine,
                             #   ingress adapters, session/state, admin API
  greentic-runner-desktop/   # Desktop CLI integration
  runner-core/               # Pack resolution, signing verification, cache helpers
  greentic-aw-runtime/       # Agentic-worker runtime (Plan-Act-Observe loop) +
                             #   `serve` mode (NATS) + `aw-serve` test-mock bin
  aw-event-bridge/           # NATS bridge: consumes greentic.agentic.request.v1,
                             #   dispatches to AgentDispatchInvoker (agentic.call side)
  telco-x-event-bridge/      # NATS bridge: consumes greentic.telco-x.request.v1,
                             #   dispatches to TelcoXDispatchInvoker (telco-x.call side).
                             #   Phase 2 transport scaffold + EchoInvoker placeholder +
                             #   telco-x-serve bin (real ops = a TelcoXDispatchInvoker impl)
  greentic-i18n/             # Compile-time i18n (embedded locale bundles)
  tests/                     # Integration test harness
```

The `greentic-runner` crate is the thin entrypoint. Almost all runtime logic lives in `greentic-runner-host`.

## Architecture

### Runtime Hierarchy

`RunnerHost` → manages multiple `TenantRuntime`s (one per tenant) → each owns `PackRuntime`s (loaded from `.gtpack` archives) → each contains Wasmtime `Component`s, flows, and templates.

### Flow Execution Pipeline

1. Ingress adapter normalizes raw provider payload into canonical schema (`Activity`)
2. Router selects tenant + flow via bindings config
3. Session key derived: `{tenant}:{provider}:{conversation}:{user}` — checked for suspended `FlowSnapshot`
4. `FlowEngine` interprets flow DAG, executing nodes in sequence
5. Each node may: invoke a WASM component, call a provider, emit a response, or `session.wait` (pause)
6. On `session.wait`: snapshot persisted → next inbound activity resumes from that point

### Agentic Workers (`dw.agent` node)

A flow node keyed `dw.agent.<agent_id>` (component `dw.agent`, operation = agent_id)
dispatches to the agentic-worker runtime (`crates/greentic-aw-runtime`, Plan-Act-Observe
loop). Gated behind the `agentic-worker` feature (default-on). Tools come from
`.gtxpack` design-extensions loaded from `GREENTIC_EXTENSIONS_DIR/design/` at boot
(`agent_node::build_ext_runtime` scans + registers them) and from per-tenant MCP servers;
the OpenAI backend encodes tool function names as `<ext>_FN_<tool>` (OpenAI rejects dots).
Needs `GREENTIC_AW_REDIS_URL` (state) + an LLM key (`GREENTIC_LLM_API_KEY`/`OPENAI_API_KEY`).
Worker config is an `AgentConfig` (`greentic-aw-runtime/src/config.rs`), supplied via pack
manifest, `<agent_id>.json` in `GREENTIC_AGENT_MANIFESTS_DIR`, or the admin endpoint.

An agent may be marked `conversational` (`AgentConfig.conversational`, default false).
Conversational agents are offered a host built-in `end_conversation` tool (reserved `host`
extension id) plus a system-prompt note; when the model calls it, the loop terminates the
turn with `TerminationReason::ConversationEnded` and the closing message (`final_message`
arg, else the accompanying reply) becomes the final reply — routed through the same
outbound-guardrail/save path as a normal `FinalReply`. This is SP1 of the in-flow
conversational chat-segment epic (`docs/superpowers/specs/2026-07-07-conversational-agent-chat-segment-epic-design.md`).

A conversational `dw.agent` node (`NodeKind::DwAgent.conversational`, default false) is a
multi-turn segment: after each agent turn the engine parks and re-enters the same node
(`NodeControl::LoopHere`) on the next inbound message, until the agent's output carries
`terminated_by == "conversation_ended"`, at which point the flow advances to the node's
successor (SP2). Non-conversational `dw.agent` is unchanged (one-shot). The flow-doc
`conversational` flag wiring is SP3; the out-of-process (Nats) dispatch path and a
`max_turns` safety cap are deferred follow-ups.

### Async runtime dispatch (`sorla.call` node)

A native flow node `sorla.call` (component `sorla.call`, operation = the sorx target)
dispatches work to the separate `greentic-sorx` runtime over NATS pub/sub. Node input:
`{ "await": true|false, "operation": "<op>", "deadline_ms": <u64?>, "input": {...} }`.
`await: true` PAUSES the flow (reuses `FlowResumeStore`/ingress resume) and resumes when the
runtime's response arrives; `await: false` continues immediately. Implemented natively
(`runner/remote_dispatch.rs` `RemoteDispatchHandler`/`NatsDispatcher`, `runner/engine.rs`
`execute_sorla_call`, `runner/dispatch_listener.rs` response-listener,
`runner/runtime_session_resumer.rs`), wired in `runtime.rs` when `GREENTIC_EVENTS_NATS_URL`
is set. Subjects: `greentic.sorla.request.v1` / `greentic.sorla.response.v1`; correlation id
= `<bare session hint>::pack=<id>::flow=<id>`. Contract in `greentic-types::runtime_dispatch`;
sorx side is the `sorx-event-bridge` crate. Same pattern is intended for `agentic.call` /
`operala.call`. Waits from an inbound with a non-empty thread/reply_to ARE resumable: the
node appends opaque `::thread=<t>::reply=<r>` markers to the correlation id (omitted when
empty) and `RuntimeSessionResumer` parses them back into the synthesized `ReplyScope` so
`FlowResumeStore::fetch` recomputes the same `scope_hash` as `save`. The sorx bridge echoes
the correlation verbatim, so no sorx change is required.

### Agentic dispatch serve mode (`agentic.call` runtime side)

The `agentic.call` node uses runtime name `"agentic"` → subjects
`greentic.agentic.request.v1` / `greentic.agentic.response.v1` (same contract,
headers, and correlation-marker rules as `sorla.call`). The runtime-side consumer
is the `aw-event-bridge` crate: it shares the `greentic-types::runtime_dispatch`
contract directly (aw-runtime pins the same types lineage as the runner, so no
mirroring) and exposes an `AgentDispatchInvoker` seam + `run_bridge`. The
production invoker (`greentic_aw_runtime::serve::RuntimeAgentDispatchInvoker`)
maps `target` → agent id, `input.user_text` → `AgentInput`, the correlation hint
→ session id, and serialises `AgentOutput` to `{reply, trail, terminated_by}`
(identical to the in-process `dw.agent` node output). Serve entries:
`greentic_aw_runtime::serve::serve(nats_url, runtime)` and the host-level
`agent_node::serve_agentic(nats_url, merged_agents)` (reuses the shared
`build_agent_runtime` so in-process and serve paths build an identical runtime).
For a credit-free live e2e there is an `aw-serve` bin (features `serve,test-mock`):
`GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 cargo run -p greentic-aw-runtime
--features serve,test-mock --bin aw-serve` (env `AW_SERVE_AGENT_ID`,
`AW_SERVE_REPLY`). `dw.agent` (in-process) stays untouched — `agentic.call` is the
out-of-process path.

**Opt-in in-process co-host (`GREENTIC_AGENTIC_SERVE_INPROC`)**: a single-node
deployment can co-host the agentic-worker service inside the main runner process
instead of running a separate `aw-serve`. **Default OFF** — distributed deploys
run `aw-serve` standalone so the agentic service scales independently. When
`GREENTIC_AGENTIC_SERVE_INPROC` is truthy (`1`/`true`/`yes`/`on`) AND
`GREENTIC_EVENTS_NATS_URL` is set, `greentic_runner_host::run()` spawns
`serve_agentic` exactly once per process (NOT per-tenant — a per-tenant spawn
would put multiple competing subscribers on `greentic.agentic.request.v1`). The
gate is the pure `agent_node::should_serve_agentic_inproc`; the spawn is
`maybe_spawn_inproc_agentic_serve` in `lib.rs`, feature-gated behind
`agentic-worker`. Process-level base agent configs come ONLY from
`GREENTIC_AGENT_MANIFESTS_DIR` (`<agent_id>.json` full `AgentConfig` files, loaded
by `agent_node::load_process_agent_configs`) — pack-embedded and per-tenant
`HostConfig.agents` are not visible at process startup. Skips with a warning (and
the runner continues normally) when no agents are configured, or when the runtime
cannot be built (no `GREENTIC_AW_REDIS_URL` / no LLM key).

### WASM Component Model

- **Target**: `wasm32-wasip2` (WASI Preview 2, Component Model)
- **Wasmtime 43** with `component-model` + `cranelift`
- Components export `greentic:component@0.6.0` world
- Host links: WASI-p2, WASI-HTTP, WASI-TLS, state/session/secrets/telemetry/OAuth helpers
- Components run on dedicated threads via `tokio::task::block_in_place` to avoid blocking the async runtime
- Two-tier cache: memory LRU + disk, keyed by `EngineProfile`

### Pack Hot-Reload

Pack index (JSON, local/HTTPS/cloud) polled at `PACK_REFRESH_INTERVAL` (default 30s). On change: resolve locators → verify digest/signature → cache artifacts → atomically swap `TenantRuntime` via `ArcSwap`. Overlays independently deployable per tenant.

### Key Modules in `greentic-runner-host`

| Module | Responsibility |
|--------|---------------|
| `pack.rs` | Component loading, flow/template discovery, Wasmtime linking |
| `runner/engine.rs` | Flow DAG interpretation, node execution |
| `host.rs` | Multi-tenant builder, `RunnerHost` lifecycle |
| `runtime.rs` | `TenantRuntime`, atomic pack swapping |
| `http/` | Axum routes (ingress adapters, admin, health) |
| `runner/adapt_*.rs` | Ingress normalization per provider |
| `runner/operator.rs` | Built-in operators (emit, wait, flow call, provider invoke) |
| `engine/state_machine.rs` | State transitions, pause/resume orchestration |
| `cache/` | Component artifact caching (memory + disk) |
| `watcher.rs` | Pack index polling and hot reload |
| `verify.rs` | Ed25519 signature verification |

## Conventions

- **Rust 1.94.0**, edition 2024, pinned via `rust-toolchain.toml`
- **YAML**: Uses `serde_yaml_gtc` (imported as `serde_yaml_bw`), not `serde_yaml`
- **Error handling**: `anyhow::Result<T>` with `.context()`; `thiserror` for domain errors
- **Serialization**: JSON for messages, CBOR for components/caching, YAML for config
- **Concurrency**: `ArcSwap` for lock-free runtime swaps, `parking_lot::Mutex` preferred over `std::sync::Mutex`
- **Dynamic dispatch**: `dyn SessionHost`, `dyn StateHost`, `dyn SecretsManager` for pluggable backends
- **i18n**: Locale bundles embedded at compile time via `build.rs`; user-facing strings use i18n keys, never hardcoded
- **Provider-core only**: Packs use `greentic:provider/schema-core@1.0.0` components; legacy typed provider worlds are not supported
- `GREENTIC_PROVIDER_CORE_ONLY=1` is set by default in CI

## Public API Surface

```rust
// Mirror the CLI
greentic_runner::run_http_host(RunnerConfig) -> Result<()>

// Embedded host (no HTTP server) for tests/tools
greentic_runner::start_embedded_host(HostBuilder) -> Result<RunnerHost>
```

## Binaries

| Binary | Purpose |
|--------|---------|
| `greentic-runner` | Main HTTP host (pack watcher + ingress) |
| `greentic-runner-cli` | CLI companion |
| `greentic-gen-bindings` | Inspects a `.gtpack` and emits a `bindings.yaml` seed |

## Key Environment Variables

| Variable | Purpose |
|----------|---------|
| `PACK_REFRESH_INTERVAL` | Watcher cadence (e.g. `30s`, `5m`) |
| `PACK_CACHE_DIR` | Artifact cache directory (default `.packs`) |
| `PACK_PUBLIC_KEY` | Ed25519 public key for signature verification |
| `PACK_VERIFY_STRICT` | Fail on missing/invalid signatures |
| `PORT` | HTTP server port (also `--port` CLI flag) |
| `DEFAULT_TENANT` | Fallback tenant for routing |
| `TENANT_RESOLVER` | Routing mode: host/header/jwt/env |
| `ADMIN_TOKEN` | Bearer token for `/admin` endpoints (loopback-only when unset) |
| `GREENTIC_EVENTS_NATS_URL` | NATS bus URL; enables `sorla.call`/`operala.call`/`agentic.call`/`telco-x.call` dispatch + in-proc agentic serve. The runner registers response listeners for `sorla`/`operala`/`agentic`/`telco-x`; the `telco-x-event-bridge` crate serves `greentic.telco-x.request.v1` (Phase 2 transport scaffold). A credit-free `telco-x-serve` bin runs the bridge with the built-in `EchoInvoker` (`cargo run -p telco-x-event-bridge --bin telco-x-serve`); real Telco-X operations need a production `TelcoXDispatchInvoker` impl. Until then `telco-x.call` only echoes. See `docs/superpowers/specs/2026-06-21-telco-x-runtime-dispatch-design.md` in the workspace root |
| `GREENTIC_AGENTIC_SERVE_INPROC` | Opt-in (default OFF): co-host the agentic-worker NATS service in-process; truthy (`1`/`true`/`yes`/`on`) + `GREENTIC_EVENTS_NATS_URL` set |
| `GREENTIC_AGENT_MANIFESTS_DIR` | Dir of `<agent_id>.json` full `AgentConfig` files; process-level agent source for in-proc serve |
| `GREENTIC_AW_REDIS_URL` | Agentic-worker state store (required for `dw.agent` + in-proc serve runtime) |

Provider secrets: `SLACK_SIGNING_SECRET`, `WEBEX_WEBHOOK_SECRET`, `WHATSAPP_VERIFY_TOKEN`, `WHATSAPP_APP_SECRET`, `TELEGRAM_BOT_TOKEN`.

## Workspace Features

- `verify` (default) — validate pack files before loading
- `telemetry` — OTLP export via `greentic-telemetry`
- `fault-injection` — testing fault injection
- `session-redis` — Redis session storage backend
- `component-v0-6-introspection` — v0.6 component inspection

## Git Conventions

Do NOT add Claude co-author attribution to commits or PRs.
