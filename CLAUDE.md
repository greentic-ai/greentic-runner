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

`ci/local_check.sh` steps: `fmt`, `dependency_sanity`, `clippy`, `host_smoke`, `crate_tests`, `workspace_tests`, `conformance`, `package`.

The `dependency_sanity` step detects multiple wasmtime versions in the dependency tree (local workspace crates vs published greentic-* version skew) and fails early before clippy/tests hit confusing trait-mismatch errors.

## Workspace Layout

```
crates/
  greentic-runner/           # Binary + public library (thin CLI wrapper)
  greentic-runner-host/      # Core runtime (~29K lines): pack loading, flow engine,
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

**Core lifecycle & runtime**

| Module | Responsibility |
|--------|---------------|
| `host.rs` | Multi-tenant builder, `RunnerHost` lifecycle |
| `boot.rs` | Startup bootstrap sequence |
| `runtime.rs` | `TenantRuntime`, atomic pack swapping via `ArcSwap` |
| `runtime_refs.rs` | Shared runtime reference handles |
| `runtime_wasmtime.rs` | Wasmtime engine/linker setup, WASI linkage |
| `config.rs` | Runtime configuration loading and defaults |
| `watcher.rs` | Pack index polling and hot reload |

**Pack & component loading**

| Module | Responsibility |
|--------|---------------|
| `pack.rs` | Component loading, flow/template discovery, Wasmtime linking |
| `component_api.rs` | Component invoke API (describe, invoke) |
| `verify.rs` | Ed25519 signature verification |
| `wasi.rs` | WASI host-import wiring |
| `cache/` | Component artifact caching (memory LRU + disk, singleflight dedup) |

**Flow engine (`engine/`)**

| Module | Responsibility |
|--------|---------------|
| `engine/mod.rs` | `FlowEngine` entry, DAG walker |
| `engine/state_machine.rs` | State transitions, pause/resume orchestration |
| `engine/builder.rs` | Engine builder pattern |
| `engine/policy.rs` | Execution policy enforcement |
| `engine/registry.rs` | Node-type registry |
| `engine/glue/`, `engine/shims/` | Adapter glue and host shims for component calls |

**Runner (invocation pipeline)**

| Module | Responsibility |
|--------|---------------|
| `runner/engine.rs` | Flow DAG interpretation, node execution |
| `runner/operator.rs` | Built-in operators (emit, wait, flow call, provider invoke) |
| `runner/invocation.rs` | Invocation envelope construction and dispatch |
| `runner/flow_adapter.rs` | Flow-to-runtime adapter |
| `runner/schema_validator.rs` | Runtime schema validation |
| `runner/contract_cache.rs` | Cached component contract descriptors |
| `runner/contract_introspection.rs` | v0.6 component introspection support |
| `runner/templating.rs` | Handlebars template rendering |
| `runner/i18n.rs` | Inline i18n locale handling |
| `runner/adapt_timer.rs` | Timer/cron ingress normalization |
| `runner/adapt_events_email.rs` | Email event ingress normalization |

**HTTP layer (`http/`)**

| Module | Responsibility |
|--------|---------------|
| `http/mod.rs` | Axum router assembly (ingress adapters, admin, health) |
| `http/admin.rs` | Admin API endpoints |
| `http/health.rs` | Health/readiness probes |

**Cross-cutting**

| Module | Responsibility |
|--------|---------------|
| `activity.rs` | Canonical `Activity` type for ingress normalization |
| `routing.rs` | Tenant + flow routing from inbound requests |
| `identify_hint.rs` | Provider identification hints for routing |
| `provider.rs` | Provider abstraction layer |
| `provider_core.rs` | Provider-core schema integration |
| `provider_core_only.rs` | Guard enforcing provider-core-only mode |
| `oauth.rs` | OAuth token brokering for components |
| `secrets.rs` | Secrets host-import implementation |
| `storage/` | Session + state backend adapters (in-memory, Redis) |
| `metrics.rs` | Prometheus metrics collectors |
| `operator_metrics.rs` | Per-operator metric instrumentation |
| `operator_registry.rs` | Operator registry (node-type → handler mapping) |
| `telemetry.rs` | OpenTelemetry span setup |
| `telemetry_scan.rs` | Telemetry configuration scanning |
| `greentic_x_provider.rs` | Greentic-X extension provider (behind `greentic-x-provider` feature) |
| `gtbind.rs` | Bindings generation helpers |
| `fault/` | Fault injection framework (behind `fault-injection` feature) |
| `testing/` | Test utilities and mock helpers |
| `trace/` | Distributed tracing helpers |
| `validate/` | Runtime validation utilities |

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
- `telemetry` — explicit span annotation via `greentic-telemetry` (telemetry crate always linked; feature gates `annotate_span` calls)
- `fault-injection` — testing fault injection
- `session-redis` — Redis session storage backend
- `component-v0-6-introspection` — v0.6 component inspection
- `greentic-x-provider` — Greentic-X extension runtime integration (host crate; pulls `greentic-x-runtime` + `greentic-x-types`)
- `legacy-gen-bindings` — gates the `greentic-gen-bindings` binary (runner crate only)

## Git Conventions

Do NOT add Claude co-author attribution to commits or PRs.
