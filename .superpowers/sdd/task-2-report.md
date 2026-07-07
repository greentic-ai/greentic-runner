# Task 2 Report: `flow:` tool resolution + `AgentRuntime.with_flow_source` (aw-runtime)

## Status: DONE

## Call Sites Updated

### `crates/greentic-aw-runtime/src/tools.rs`

Signature changes:

- `list_tools_for_llm`: added `flows: Option<&FlowToolCatalog>` param (after `components`); added `flow:` branch (keyed by single `flow_ref`) before the extension fallthrough.
- `missing_tools`: added `flows: Option<&FlowToolCatalog>` param (after `components`); added `flow:` branch. A `flow:` ref present in the catalog is NOT reported missing; absent one IS. Mirrors `component:` and `mcp:` branches exactly.
- `dispatch_tool_call`: added `flows: Option<Arc<FlowToolCatalog>>` param (after `components`); added `flow:` branch delegating to `FlowToolCatalog::dispatch` (total, never returns `Err`) or returning `{"error":"..."}` when no catalog is wired.

Existing test call sites updated (all received `None` as the new flows arg):

- `list_tools_for_llm_with_no_extensions_returns_empty`
- `missing_tools_reports_unloaded_extension`
- `missing_tools_reports_mcp_tool_absent_from_catalog`
- `mcp_ref_listed_from_catalog`
- `non_mcp_ref_unchanged`
- `dispatch_routes_mcp_ref` (3 dispatch call sites)
- `component_ref_listed_from_catalog`
- `non_component_ref_unaffected_by_catalog`
- `dispatch_routes_component_ref` (3 dispatch call sites)

New test added:

- `tools::tests::flow_prefixed_tool_is_listed_and_dispatched` — builds a `FlowToolCatalog` from a `FakeFlowInvoker`, asserts the `flow:lookup` ref appears in `list_tools_for_llm` output, then dispatches it and asserts no error value.

### `crates/greentic-aw-runtime/src/lib.rs`

- Added `pub(crate) flows: Option<Arc<crate::flow_source::FlowToolSource>>` field next to `components`.
- Initialized `flows: None` in `AgentRuntime::new`.
- Added `with_flow_source(mut self, flows: Option<Arc<crate::flow_source::FlowToolSource>>) -> Self` builder next to `with_component_source`.

### `crates/greentic-aw-runtime/src/loop.rs`

- Added flow catalog resolution block after the component catalog block, before the preflight call.
- Updated `missing_tools` call: added `flow_catalog.as_deref()`.
- Updated `list_tools_for_llm` call: added `flow_catalog.as_deref()`.
- Updated `dispatch_tool_call` call: added `flow_catalog.clone()`.

### `crates/greentic-aw-runtime/tests/tools_live.rs`

- Updated `unloaded_tool_is_invisible_to_llm_and_dispatch_fails_safe`: both `list_tools_for_llm` and `dispatch_tool_call` calls updated with `None` flows.

## `missing_tools` treatment

Yes, `missing_tools` takes the same catalogs and the `flow:` branch was added. A `flow:` ref present in the catalog is not missing; one absent (or no catalog) is reported missing. This is consistent with the `mcp:` and `component:` branches.

## Real `loop.rs` call-site shape

`flow_catalog` has type `Option<Arc<FlowToolCatalog>>`. Passed as:
- `flow_catalog.as_deref()` to `list_tools_for_llm` and `missing_tools` (both take `Option<&FlowToolCatalog>`)
- `flow_catalog.clone()` to `dispatch_tool_call` (takes `Option<Arc<FlowToolCatalog>>`)

## Test run (fail → pass)

Before implementation: `list_tools_for_llm` and `dispatch_tool_call` did not accept a `flows` param — the new test would not compile.

After implementation:
```
cargo test -p greentic-aw-runtime
test tools::tests::flow_prefixed_tool_is_listed_and_dispatched ... ok
# all other tests: ok
# 0 failures
```

Build: `cargo build -p greentic-aw-runtime` → `Finished` cleanly.

## Concerns

None. All callers updated cleanly. No partial updates outstanding.
