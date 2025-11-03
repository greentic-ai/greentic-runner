# mcp-exec

Greentic's executor for Wasm tools that implement the `wasix:mcp` interface.
The crate handles lookup, verification, and execution of MCP-compatible
components while exposing host capabilities such as secrets, telemetry, and
HTTP fetch.

## Features

- Local and remote (HTTP) tool stores with SHA-256 integrity checks.
- Signature policy stubs ready for digest/signature enforcement.
- Wasmtime component runtime with Greentic host imports wired in.
- Utility helpers for describing tools via conventional actions.

## Usage

```rust
use greentic_types::{EnvId, TenantCtx, TenantId};
use mcp_exec::{ExecConfig, ExecRequest, RuntimePolicy, ToolStore, VerifyPolicy};
use serde_json::json;

let tenant = TenantCtx {
    env: EnvId("dev".into()),
    tenant: TenantId("acme".into()),
    tenant_id: TenantId("acme".into()),
    team: None,
    team_id: None,
    user: None,
    user_id: None,
    trace_id: None,
    correlation_id: None,
    deadline: None,
    attempt: 0,
    idempotency_key: None,
    impersonation: None,
};

let cfg = ExecConfig {
    store: ToolStore::HttpSingleFile {
        name: "weather_api".into(),
        url: "https://example.invalid/weather_api.wasm".into(),
        cache_dir: std::env::temp_dir(),
    },
    security: VerifyPolicy::default(),
    runtime: RuntimePolicy::default(),
    http_enabled: true,
};

let output = mcp_exec::exec(
    ExecRequest {
        component: "weather_api".into(),
        action: "forecast_weather".into(),
        args: json!({"location": "AMS"}),
        tenant: Some(tenant),
    },
    &cfg,
)?;
```

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Set `RUN_ONLINE_TESTS=1` to exercise the live weather integration test that
retrieves the published Wasm component over HTTPS.
