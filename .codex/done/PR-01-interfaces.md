# PR-01-interfaces: Downstream repos must use `greentic_interfaces::canonical` (never `bindings::*`)

**Date:** 2026-02-19  
**Scope:** Any repo that depends on `greentic-interfaces`

## Why
`greentic-interfaces` generates WIT bindings under **world/version-scoped modules** (e.g. `bindings::greentic_component_0_6_0_component::...`).  
Those paths are **not stable** across versions/world sets, and in external consumer builds there is **no guaranteed** `bindings::greentic::...` root.

To make all downstream code stable and version-ready, `greentic-interfaces` exposes a **canonical facade**:

- `greentic_interfaces::canonical::types` (WIT-derived shared types)
- (optionally) `greentic_interfaces::canonical::node` / `core` as added by PR-IF-01

Downstream repos must **only** use the facade. `bindings::*` is considered **internal**.

## Rule (must-follow)
> **Never import from `greentic_interfaces::bindings::*`** (or `bindings::greentic::*`) in application/library code, tests, or README/examples.

**The only allowed location for `bindings::*` references is inside `greentic-interfaces` itself** (its ABI facade module).

## Required changes in downstream repos

### 1) Update imports
Replace any usage like:

```rust
use greentic_interfaces::bindings::greentic_component_0_6_0_component::greentic::interfaces_types::types as wit_types;
```

with:

```rust
use greentic_interfaces::canonical::types as wit_types;
```

Then keep using:
- `wit_types::ErrorCode`
- `wit_types::Protocol`
- `wit_types::AllowList`
- etc.

### 2) Update type aliases and matches
Replace patterns like:

```rust
type WitProtocol = greentic_interfaces::bindings::greentic::interfaces_types::types::Protocol;

if let greentic_interfaces::bindings::greentic::interfaces_types::types::Protocol::Custom(v) = protocol { ... }
```

with:

```rust
type WitProtocol = greentic_interfaces::canonical::types::Protocol;

if let greentic_interfaces::canonical::types::Protocol::Custom(v) = protocol { ... }
```

### 3) Update tests and README/examples too
Tests and docs are copy-paste sources; they must follow the same rule.

## Search patterns to fix
Search for any of the following strings in the repo:

- `greentic_interfaces::bindings::`
- `bindings::greentic::`
- `interfaces_types::types::` (when prefixed by `bindings::`)
- `greentic_component_` (in a `bindings::` path)

## Acceptance criteria
- `cargo test` passes
- No references to `greentic_interfaces::bindings` remain (except inside greentic-interfaces itself)
- README/examples compile (if you have doctests or example builds)

## Recommended CI guardrail (optional)
In downstream repos, add a lightweight check (script or CI step) that fails if new `bindings::` usage is introduced:

```bash
rg -n "greentic_interfaces::bindings::|\bbindings::greentic::" . && echo "ERROR: use greentic_interfaces::canonical instead" && exit 1 || true
```
