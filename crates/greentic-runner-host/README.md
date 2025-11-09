# greentic-runner-host

`greentic-runner-host` packages the legacy Greentic runner host shim as a standalone crate. It covers bindings/config loading, pack verification, Wasmtime runtime glue, and convenience adapters for messaging/webhook/timer activities so consumers no longer have to vendor the runtime internals.

## Quick start

```rust
use greentic_runner_host::{Activity, HostBuilder, HostConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = HostConfig::load_from_path("./bindings/customera.yaml")?;
    let host = HostBuilder::new().with_config(config).build()?;

    host.start().await?;
    host.load_pack("customera", "./packs/customera/index.ygtc".as_ref()).await?;

    let activity = Activity::text("hello")
        .with_tenant("customera")
        .from_user("u-1");

    let replies = host.handle_activity("customera", activity).await?;
    for reply in replies {
        println!("reply: {}", reply.payload());
    }

    host.stop().await?;
    Ok(())
}
```

## Cargo features

- `verify` *(default)* – validate pack files exist before loading.
- `mcp` – enable tool invocation through the [`mcp-exec`](https://crates.io/crates/mcp-exec) bridge.
- `telemetry` – wire OTLP export via [`greentic-telemetry`](https://crates.io/crates/greentic-telemetry).

## License

This project is licensed under the [MIT License](./LICENSE).
