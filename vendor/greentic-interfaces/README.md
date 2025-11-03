# Greentic Interfaces

Shared WebAssembly Interface Types (WIT) packages and Rust bindings for the Greentic next-gen platform. The crate is MIT licensed and evolves additively—new fields or functions land in minor releases, while breaking changes require a new package version.

## Overview

The repository serves two goals:

- Authoritative WIT contracts that describe how packs interact with the Greentic host. The contracts cover core types, host imports, the pack component API, and provider self-description metadata.
- Ergonomic Rust bindings with thin conversion helpers so runtime code can move between WIT-generated types and the rich structures from [`greentic-types`](../greentic-types).

All Greentic runtimes, components, and tools **must** depend on these WIT packages and the shared `greentic-types` crate. No other repository may re-declare the contracts or duplicate the shared models.

## WIT packages

The `wit/` directory contains four additive packages:

| Package | Contents |
|---------|----------|
| `greentic:interfaces-types@0.1.0` | Canonical data structures (`TenantCtx`, `SessionCursor`, `Outcome`, `AllowList`, `NetworkPolicy`, `PackRef`, `SpanContext`, etc.). |
| `greentic:interfaces-host@0.1.0`  | Host-side imports a pack can call (`secrets.get`, `telemetry.emit`, `state.get/set`, `session.update`). |
| `greentic:interfaces-provider@0.1.0` | Provider self-description (`ProviderMeta`). |
| `greentic:interfaces-pack@0.1.0` | Component world exporting `meta()` and `invoke()` for pack execution.

The build script stages each package (plus dependencies) into `$OUT_DIR/wit-staging` so downstream tooling resolves imports deterministically. The absolute path is exported as `WIT_STAGING_DIR`, so consumers never need write access to the package directory even when building from crates.io.

## Rust bindings

This crate is intentionally ABI-only: `greentic_interfaces::bindings::generated` exposes the raw `wit-bindgen` output for the `interfaces-pack` world, and the helper modules translate between those generated types and the richer models in [`greentic-types`](../greentic-types). No Wasmtime adapters ship here.

The bindings follow the `TenantCtx`, `Outcome`, `ProviderMeta`, and `AllowList` shapes defined in the design manifesto so runner, deployer, connectors, and packs all share the same ABI.

### Example: invoking a pack component

```rust
use greentic_interfaces::bindings::exports::greentic::interfaces_pack::component_api;
use greentic_interfaces::bindings;

fn empty_allow_list() -> bindings::greentic::interfaces_types::types::AllowList {
    bindings::greentic::interfaces_types::types::AllowList {
        domains: Vec::new(),
        ports: Vec::new(),
        protocols: Vec::new(),
    }
}

struct GreetingComponent;

impl component_api::Guest for GreetingComponent {
    fn meta() -> component_api::ProviderMeta {
        component_api::ProviderMeta {
            name: "hello-tool".into(),
            version: "0.1.0".into(),
            capabilities: vec!["invoke".into()],
            allow_list: empty_allow_list(),
            network_policy: bindings::greentic::interfaces_types::types::NetworkPolicy {
                egress: empty_allow_list(),
                deny_on_miss: false,
            },
        }
    }

    fn invoke(input: String, tenant: component_api::TenantCtx) -> component_api::Outcome {
        let message = format!("Hello {}, {}!", tenant.tenant, input);
        component_api::Outcome::Done(message)
    }
}
```

The packed component returns a `Outcome::Done(String)` which maps directly to `greentic_types::Outcome<String>` via the conversion helpers described below.

A minimal `examples/crates-io-consumer` binary shows how to depend on the published crate without any workspace patches.

## Conversion helpers

`src/mappers.rs` implements thin `From`/`TryFrom` conversions between WIT-generated types and their `greentic-types` equivalents:

- `TenantCtx ↔ greentic_types::TenantCtx`
- `SessionCursor ↔ greentic_types::SessionCursor`
- `Outcome<string> ↔ greentic_types::Outcome<String>`
- `AllowList ↔ greentic_types::policy::AllowList`
- `NetworkPolicy ↔ greentic_types::policy::NetworkPolicy`
- `SpanContext ↔ greentic_types::telemetry::SpanContext`

These helpers avoid business logic—each mapping is a total, lossless transformation so packs and hosts can interoperate without bespoke glue.

Unit tests under `src/mappers.rs` and integration tests in `tests/mapping_roundtrip.rs` ensure round-trips preserve the data the runner depends on (tenant identity, session cursors, expected input hints, etc.).

## Provider metadata validation

`greentic_interfaces::validate::validate_provider_meta` checks the minimal invariants for provider self-description:

- Non-empty provider name.
- Valid semantic version string.
- Allow-lists contain no empty hosts, zero ports, or unnamed custom protocols.
- Strict network policies (`deny_on_miss = true`) must include at least one allow rule.

Call this helper before accepting provider metadata to avoid surprising runtime failures.

## Testing, formatting, and linting

Quality gates are enforced locally and in CI:

```bash
# Format
cargo fmt --all

# Lint (covers library, tests, and examples)
cargo clippy --all-targets --all-features -- -D warnings

# Run the full test matrix (includes schema snapshots)
cargo test --all-features
```

The `wit_build` integration test parses every staged WIT package to ensure the build script emits well-formed bundles, and the schema snapshot (guarded by the `schema` feature) keeps provider metadata schemas stable.

CI mirrors these commands so pull requests fail fast if formatting drifts, clippy raises a regression, or contracts stop compiling.

## Releases & Publishing

Version numbers come from each crate's `Cargo.toml`. When a commit lands on `master`, the auto-tag workflow checks whether any crate manifests changed and creates lightweight tags in the form `<crate>-v<semver>` (for single-crate repos this matches the repository name). The publish workflow then runs the lint/test gate, and finally invokes `katyo/publish-crates` to publish changed crates to crates.io using the `CARGO_REGISTRY_TOKEN` secret. Publishing is idempotent, so rerunning on the same commit succeeds even if the versions are already available.

## Maintenance notes

- Updating WIT contracts only happens additively. Introduce a new version instead of breaking existing packages.
- Regenerate the Rust bindings by running `cargo build`; the build script handles staging and `wit-bindgen` output automatically.
- When adding new WIT structures, remember to extend the conversion helpers and the snapshot test so hosts and packs keep sharing the same ABI.
- The schema snapshot lives under `tests/snapshots/`. Run `INSTA_ACCEPT=auto cargo test --features schema` whenever the provider metadata shape changes to refresh the snapshot intentionally.

## Runtime support

Consumers that need to execute packs via Wasmtime should depend on the sibling `greentic-interfaces-wasmtime` crate (introduced in a follow-up PR) or wire Wasmtime directly. This package purposefully avoids runtime glue so it can stay focused on ABI contracts and type conversions.
