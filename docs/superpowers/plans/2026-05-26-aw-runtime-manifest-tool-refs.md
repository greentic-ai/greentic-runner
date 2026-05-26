# Plan — AW runtime: `manifest_to_tool_refs`

Date: 2026-05-26
Spec: `docs/superpowers/specs/2026-05-26-aw-runtime-manifest-tool-refs.md`

## Approach (TDD, additive)

### Step 1 — dependency (done)
Add `greentic-dw-manifest` (git, branch `research`) to
`crates/greentic-aw-runtime/Cargo.toml` `[dependencies]`. Fetch + verify real
types. (Confirmed in spec.)

### Step 2 — failing tests
New integration test file `crates/greentic-aw-runtime/tests/manifest_tools.rs`.
Build `DigitalWorkerManifest` / `ExtensionTool` fixtures. Because
`greentic-dw-manifest` does not re-export `AgenticWorkerMetadata` (from
`greentic-extension-sdk-contract`) and that crate is not a direct dependency,
construct `ExtensionTool` fixtures by deserialising JSON — the required
`agentic_worker_metadata` field accepts `{}` (it derives `Default`). This keeps
the test free of an extra dev-dependency.

Cases:
1. `agentic_worker`-capable tools mapped to `ToolRef`.
2. Tools lacking `agentic_worker` capability skipped.
3. Empty manifest → empty `Vec`.
4. Duplicate `(extension_id, tool_name)` pairs de-duped, first kept.
5. Declaration order preserved.

### Step 3 — implementation
New file `crates/greentic-aw-runtime/src/manifest_tools.rs`:

```rust
pub fn manifest_to_tool_refs(manifest: &DigitalWorkerManifest) -> Vec<ToolRef>
```

- Filter `capabilities.iter().any(|c| c == "agentic_worker")`.
- Map to `ToolRef`, de-dup via a seen-set on `(extension_id, tool_name)`,
  preserve order.
- Add a `#[cfg(test)]` unit module mirroring the behaviour (pure-fn level)
  so logic is covered even without the integration harness.

Wire `pub mod manifest_tools;` into `lib.rs` (single line) and re-export the
function for ergonomics.

### Step 4 — gates
`cargo build`, `cargo test`, `cargo fmt --check`, `cargo clippy -D warnings`
(all `-p greentic-aw-runtime`). Husky pre-commit runs on commit.

## Commits
1. `docs:` spec + plan.
2. `feat:` + `test:` implementation + tests (single commit; tests + impl land
   together so every commit compiles and passes).

## Parallel-safety
Touches only `crates/greentic-aw-runtime/` + new `docs/` files. No edits to
`loop.rs` / `tools.rs` / `llm.rs` or anything under `crates/greentic-runner-host/`.
