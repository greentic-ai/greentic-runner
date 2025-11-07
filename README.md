# greentic-runner

Greentic runner host for executing agentic flows packaged as Wasm components.  
The workspace hosts the runner binary in `crates/host` plus integration tests in `crates/tests` and sample bindings under `examples/bindings/`.

## Getting Started

```bash
cargo run -p greentic-runner -- --pack /path/to/pack.wasm --bindings examples/bindings/default.bindings.yaml --port 8080
```

On startup the host loads the supplied pack, enumerates flows, and exposes the messaging webhook for Telegram under `/messaging/telegram/webhook`.

## Architecture

- **Legacy host (`legacy-host` feature, default):** HTTP server that loads packs, exposes Telegram/webhook adapters, and schedules timers. Production deployments continue to use this path unchanged.
- **New runner (`new-runner` feature):** Library + CLI built around `RunnerApi`, a policy-governed `StateMachine`, and an `AdapterRegistry`. Dependencies (sessions/state, telemetry, secrets) are injected through host traits so the legacy integrations can be bridged without duplication.
- **Host bundle:** Secrets, telemetry, session, and state providers packaged as a `HostBundle` and shared between the server and the new runner.
- **Adapter registry:** Connectors register under stable names (`messaging.telegram`, `email.google`, …); the state machine resolves outbound calls via the registry and enforces idempotent sends.

```
TenantCtx → RunnerApi → StateMachine → Host Bundle (session/state/secrets/telemetry)
                                         ↘ AdapterRegistry → Connectors
```

## Quickstart (new runner)

```bash
# Build the CLI behind the new-runner feature
cargo run -p greentic-runner --bin greentic-runner-cli --no-default-features --features new-runner -- \
  list-flows --manifest ./manifests/sample_flows.json

cargo run -p greentic-runner --bin greentic-runner-cli --no-default-features --features new-runner -- \
  run-flow --manifest ./manifests/sample_flows.json \
  --flow-id flow.test \
  --tenant '{"env":"dev","tenant":"acme","attempt":0}' \
  --input '{"value":42}'
```

`sample_flows.json` is a JSON array of flow definitions that the new runner ingests. Each definition declares summaries, schemas, and a linear set of steps. Adapter steps reference registry names such as `demo.adapter`.

## Telemetry

Telemetry initialises automatically through the `greentic_types::telemetry::main` macro. Configure exporters and logging via environment variables:

```
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
RUST_LOG=info
OTEL_RESOURCE_ATTRIBUTES=deployment.environment=dev
```

## Runner API Usage

```rust
use greentic_runner::newrunner::builder::RunnerBuilder;
use greentic_runner::newrunner::policy::Policy;
use greentic_runner::newrunner::registry::AdapterRegistry;
use greentic_runner::newrunner::shims::{InMemorySessionHost, InMemoryStateHost};
use greentic_runner::newrunner::host::HostBundle;
use greentic_runner::newrunner::state_machine::{FlowDefinition, FlowStep};
use greentic_runner::newrunner::FlowSummary;
use greentic_runner::newrunner::api::{RunFlowRequest, RunnerApi};
use greentic_runner::glue::{FnSecretsHost, FnTelemetryHost};
use greentic_types::{EnvId, TenantCtx, TenantId};
use std::sync::Arc;

#[greentic_types::telemetry::main(service_name = "example-runner")]
async fn main() -> anyhow::Result<()> {
    let secrets = Arc::new(FnSecretsHost::new(|name| std::env::var(name).map_err(|err| err.into())));
    let telemetry = Arc::new(FnTelemetryHost::new(|span, fields| {
        for (k, v) in fields { tracing::info!(?span, key = *k, value = *v); }
        Ok(())
    }));
    let session = Arc::new(InMemorySessionHost::new());
    let state = Arc::new(InMemoryStateHost::new());
    let host = HostBundle::new(secrets, telemetry, session, state);

    let flows = vec![FlowDefinition::new(
        FlowSummary {
            id: "flow.test".into(),
            name: "Test Flow".into(),
            version: "1.0.0".into(),
            description: None,
        },
        serde_json::json!({"type": "object"}),
        vec![FlowStep::Complete { outcome: serde_json::json!({"echo": true}) }],
    )];

    let mut builder = RunnerBuilder::new()
        .with_host(host)
        .with_adapters(AdapterRegistry::default())
        .with_policy(Policy::default());
    for flow in flows { builder = builder.with_flow(flow); }
    let runner = builder.build()?;

    let tenant = TenantCtx::new(EnvId::from("dev"), TenantId::from("acme"))
        .with_provider("example-runner")
        .with_flow("flow.test");

    let response = runner.run_flow(RunFlowRequest {
        tenant,
        flow_id: "flow.test".into(),
        input: serde_json::json!({}),
        session_hint: None,
    }).await?;

    println!("{}", serde_json::to_string_pretty(&response.outcome)?);
    Ok(())
}
```

## Maintenance Notes

- **Features:** `legacy-host` (default) keeps existing HTTP server; `new-runner` enables the new CLI, API, and state machine. `redis` is reserved for future session/state backends; `schema` will emit JSON schemas for public structs.
- **Runtime selection:**
  - Stable (default) enables `stable-wasmtime`, which pins Wasmtime to `<38` for compatibility with the stable toolchain.
  - Nightly workloads can opt into `nightly-wasmtime` to compile against Wasmtime `38`. This requires building with `--no-default-features --features legacy-host,nightly-wasmtime` (or the equivalent for the CLI) and the repo-local nightly toolchain.
  - Non-execution commands (for example, schema inspection) do not require either runtime feature.
- **Policy & retry:** `src/newrunner/policy.rs` centralises exponential backoff with jitter. Both the new runner and the legacy server can reuse this helper to keep retry semantics aligned.
- **Adapters:** Register legacy adapters through `glue::FnAdapterBridge` or migrate them to implement `newrunner::Adapter` directly.
- **Testing:** `cargo test --workspace --all-features` exercises the new state machine (requires enabling `new-runner`). Redis-backed tests will attach once the external crates land.
- **Session/state shims:** Temporary in-memory implementations live under `src/newrunner/shims/` and provide CAS semantics plus TTL. Replace them with `greentic-session` / `greentic-state` once published.

## Local Checks

Run the local CI mirror before pushing:

```bash
ci/local_check.sh
```

Toggles:

- `LOCAL_CHECK_ONLINE=1` – enable steps that need the network.
- `LOCAL_CHECK_STRICT=1` – treat missing tools as fatal and run all optional checks.
- `LOCAL_CHECK_VERBOSE=1` – print every command.

The script installs a lightweight `pre-push` hook on first run so pushes stay green.

## Releases & Publishing

- Crate versions come directly from each crate's `Cargo.toml`.
- Pushing to `master` tags every crate whose version changed as `<crate-name>-v<semver>`.
- The publish workflow runs after tagging and pushes the changed crates to crates.io.
- Publishing is idempotent; it succeeds even when crates are already up to date.

## License

MIT
