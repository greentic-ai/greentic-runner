# Spec — AW runtime: bridge `DigitalWorkerManifest.extension_tools` → `ToolRef`

Date: 2026-05-26
Crate: `greentic-aw-runtime`
Sub-project: 4b-iii (parallel-safe, additive)

## Problem

The AW runtime builds its LLM tool catalog from `AgentConfig.tools: Vec<ToolRef>`
(`ToolRef { extension_id, tool_name }`). At runtime, `tools::list_tools_for_llm`
resolves each `ToolRef` against the **live** `ExtensionRuntime::list_tools()`.

Sub-project #2 introduced `DigitalWorkerManifest` (crate `greentic-dw-manifest`),
which snapshots the tools a Digital Worker invokes in
`extension_tools: Vec<ExtensionTool>`. Each `ExtensionTool` carries
`extension_id`, `tool_name`, a `capabilities: Vec<String>` list, and a verbatim
snapshot of the tool's schema/description.

There is currently no path from a manifest's `extension_tools` into the AW
runtime's `AgentConfig.tools`. A `ConfigProvider` that wants to source its tool
list from a manifest snapshot has nothing to call.

## Goal

Provide a pure, additive converter:

```rust
pub fn manifest_to_tool_refs(manifest: &DigitalWorkerManifest) -> Vec<ToolRef>
```

so a `ConfigProvider` can populate `AgentConfig.tools` from a manifest snapshot,
while the runtime keeps its existing **live** schema resolution.

## Behaviour

- Iterate `manifest.extension_tools` in declaration order.
- Include only tools whose `capabilities` contains `"agentic_worker"`.
- Skip tools without that capability (e.g. flow-only tools).
- Map each included tool → `ToolRef { extension_id, tool_name }`
  (clone of the snapshot's `extension_id`/`tool_name`).
- De-duplicate exact `(extension_id, tool_name)` pairs, keeping the first
  occurrence; preserve order otherwise.
- Empty manifest (no `extension_tools`) → empty `Vec`.

## Explicitly out of scope (this slice)

- **Snapshot-wins-at-runtime.** The snapshot's `description` / `input_schema_json`
  are NOT used to feed the LLM. The runner keeps live `list_tools_for_llm`
  resolution against `ExtensionRuntime`. This converter only supplies the *set*
  of `(extension_id, tool_name)` pairs to wire as `ToolRef`s.
- Runner-host wiring (`crates/greentic-runner-host/`) — owned by Phase 4.
- Drift detection / version pinning warnings.

## Constraints

- Additive only: new file `src/manifest_tools.rs`, one `pub mod` line in
  `lib.rs`, one dependency line in `Cargo.toml`, one new test file.
- No `unwrap()` / `panic!()` in non-test code (crate already denies via lints).
- Edition 2024, Rust 1.95, file ≤ 500 lines.

## Real-type confirmation

Inspected `greentic-dw-manifest` (git `greenticai/greentic-dw`, branch `research`,
rev `43055c6`). Confirmed:
- `DigitalWorkerManifest.extension_tools: Vec<ExtensionTool>`.
- `ExtensionTool` fields: `extension_id`, `extension_version`, `tool_name`,
  `description`, `input_schema_json`, `output_schema_json: Option<String>`,
  `capabilities: Vec<String>`, `agentic_worker_metadata: AgenticWorkerMetadata`.
- The manifest's own validation requires every `ExtensionTool.capabilities` to
  contain `"agentic_worker"`, confirming `"agentic_worker"` is the correct
  filter literal.

All documented field names match the real structs; no deviations.
