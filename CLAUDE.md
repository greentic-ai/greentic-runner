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
  tests/                     # Integration test harness
```

The `greentic-runner` crate is the thin entrypoint. Almost all runtime logic lives in `greentic-runner-host`.

i18n is handled inline in runner-host (`crates/greentic-runner-host/src/runner/i18n.rs`), not as a separate workspace crate.

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

### WASM Component Model

- **Target**: `wasm32-wasip2` (WASI Preview 2, Component Model)
- **Wasmtime 45** with `component-model` + `cranelift`
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

- **Rust 1.95.0**, edition 2024, pinned via `rust-toolchain.toml`
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

Provider secrets: `SLACK_SIGNING_SECRET`, `WEBEX_WEBHOOK_SECRET`, `WHATSAPP_VERIFY_TOKEN`, `WHATSAPP_APP_SECRET`, `TELEGRAM_BOT_TOKEN`.

## Workspace Features

- `verify` (default) — validate pack files before loading
- `telemetry` — OTLP export via `greentic-telemetry`
- `fault-injection` — testing fault injection
- `session-redis` — Redis session storage backend
- `component-v0-6-introspection` — v0.6 component inspection

## Git Conventions

Do NOT add Claude co-author attribution to commits or PRs.
