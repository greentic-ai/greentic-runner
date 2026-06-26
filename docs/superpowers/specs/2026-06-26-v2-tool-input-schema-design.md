# v2 declarative tool extensions must carry per-tool input schemas

**Date:** 2026-06-26
**Status:** Design (ready to execute)
**Repos:** `greentic-designer-sdk` (contract), `greentic-designer-extensions` (ext-runtime), `component-tavily-ext` (+ other v2 tool exts), `greentic-runner` (pin bumps)

## Problem

An agentic worker (`dw.agent`) cannot actually USE a v2 declarative tool
extension, because the tool's input parameter schema never reaches the LLM. The
LLM therefore calls the tool with empty arguments and the tool rejects them.

### Evidence (end-to-end trace, DeepSeek + Tavily)

Run of the `tavily-research` e2e (greentic-runner desktop, multi-provider
greentic-llm backend, Tavily tool installed + `GREENTIC_EXT_ALLOW_UNSIGNED=1`):

```
node "research" (dw.agent, provider deepseek) trail:
  [0] tool_call tavily_search  → error: "decode tavily_search input: missing field `query`"   (args: null)
  [1] tool_call tavily_extract → error: "decode tavily_extract input: missing field `urls`"   (args: null)
  [2] reply: "...couldn't perform a live search... based on my own knowledge..."
```

The LLM **does** decide to call `tavily_search` (the tool is offered), but sends
no `query` — because the schema it was given had no `query` property.

### Root cause (confirmed in code)

1. The AW loop builds the LLM tool list via
   `greentic-aw-runtime::tools::list_tools_for_llm`, which calls
   `ExtensionRuntime::list_tools(extension_id)` and uses each entry's
   `input_schema_json` as the LLM `parameters`. When it is empty/unparseable it
   falls back to `{"type":"object","properties":{}}` (no params).

2. For a **v2 declarative** extension (`describe.json`
   `apiVersion: greentic.ai/v2`, e.g. Tavily),
   `ExtensionRuntime::list_tools` takes the declarative path and maps each tool
   via `contribution_tool_to_definition`
   (`greentic-designer-extensions/crates/greentic-ext-runtime/src/runtime.rs:722-734`),
   which sets **`input_schema_json: String::new()`** by design — the doc comment
   states "the v2 declarative contract omits per-tool schemas from
   `describe.json`".

3. The v2 `Tool` contribution struct
   (`greentic-designer-sdk/crates/greentic-extension-sdk-contract/src/describe/contributions/tool.rs`)
   has fields `name`, `export`, `runtime_ref`, `capabilities`,
   `secret_requirements` — **no input schema** — and is
   `#[serde(deny_unknown_fields)]`, so a tool extension cannot even add the
   schema to its `describe.json` without a struct change first.

So: the contract has no slot for a tool's input schema, the runtime emits an
empty one, and the LLM is left blind to the tool's parameters. This affects
**every** v2 declarative tool extension used by an agentic worker, not just
Tavily.

## Goal

A v2 declarative tool extension declares each tool's JSON-Schema input shape in
its `describe.json`; the runtime surfaces it through `list_tools`; the AW loop
passes it to the LLM, so the LLM calls the tool with correct arguments.

## Design

Additive, backward-compatible across four repos. Existing tool exts that omit
the new field keep working (the runtime emits the same empty schema as today).

### 1. `greentic-designer-sdk` — contract (`greentic-extension-sdk-contract`)

Add an optional input-schema field to the v2 `Tool` contribution struct
(`src/describe/contributions/tool.rs`):

```rust
/// JSON Schema for the tool's input arguments, surfaced to the LLM as the
/// function `parameters`. Absent → the runtime emits an empty schema (the tool
/// is offered but the model cannot infer its arguments). Additive: older
/// describes decode with `None`.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub input_schema: Option<serde_json::Value>,
```

`#[serde(deny_unknown_fields)]` stays (the field is now known). Optionally add a
`description: Option<String>` at the same time so v2 tools can carry a human
description too (the runtime also blanks `description` today). Bump the crate
version; publish / tag.

### 2. `greentic-designer-extensions` — runtime (`greentic-ext-runtime`)

`contribution_tool_to_definition` (`src/runtime.rs:722-734`) reads the new
field:

```rust
crate::types::ToolDefinition {
    name: t.name.clone(),
    description: t.description.clone().unwrap_or_default(),
    input_schema_json: t
        .input_schema
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_default(),
    output_schema_json: None,
    capabilities: t.capabilities.clone(),
    agentic_worker_metadata: None,
    secret_requirements: t.secret_requirements.clone(),
}
```

Update the doc comment (no longer "left empty"). Bump the dep on the new
sdk-contract version; tag a new ext-runtime release.

### 3. `component-tavily-ext` (and other v2 tool exts) — `describe.json`

Add `input_schema` to each tool. Tavily:

```json
{ "name": "tavily_search", "export": "...", "runtime_ref": "tavily-tool",
  "capabilities": ["agentic_worker"], "secret_requirements": [...],
  "input_schema": {
    "type": "object",
    "properties": {
      "query": { "type": "string", "description": "the web search query" },
      "max_results": { "type": "integer", "default": 5 }
    },
    "required": ["query"]
  }
}
```

`tavily_extract` → `{ "urls": { "type": "array", "items": {"type":"string"} } }`,
`required: ["urls"]`. The schemas MUST match what the wasm `decode` expects
(the wasm is unchanged — only `describe.json` gains the schema). Re-pack the
`.gtxpack` (describe.json + the existing `extension.wasm`); republish.

### 4. `greentic-runner` — pin bumps

Bump `greentic-extension-sdk-contract` and `greentic-ext-runtime` to the new
versions in `greentic-aw-runtime/Cargo.toml` (and anywhere else they are
pinned). No code change in the runner — `list_tools_for_llm` already consumes
`input_schema_json`.

## Sequencing

1, then 2 (depends on 1's published contract), then 3 (depends on 1 for the
`describe.json` schema + the gtxpack), then 4 (depends on 2's tag). For local
end-to-end verification before publishing, use `[patch.crates-io]` /
`[patch."https://…"]` in the runner workspace pointing at the sibling clones —
note the sibling ext-runtime is currently on `chore/bump-1.2.22` while the
runner pins tag `v1.2.25-research`, so align versions first or the patch will
not resolve.

## Testing

- **Unit (ext-runtime):** `contribution_tool_to_definition` with a tool carrying
  `input_schema` → `input_schema_json` is the serialized schema; without it →
  empty (unchanged).
- **Contract round-trip (sdk):** a `describe.json` with `input_schema` decodes
  and re-serializes losslessly; one without decodes to `None`.
- **e2e (greentic-runner):** the existing `tavily_research_e2e` with
  `E2E_QUESTION` forcing a search now shows a trail of
  `tool_call(tavily_search, {query:…}) → tool_result(<web results>) → reply`
  with a real answer + source URL, instead of the "missing field `query`" error.

## Out of scope

- Reading the schema from the wasm component (the v1 WIT-introspection path) for
  v2 declarative exts — the declarative `describe.json` is the chosen source of
  truth; WIT introspection stays the v1 path.
- The already-landed multi-provider LLM backend + tool secret/HTTP/unsigned
  wiring (branch `feat/aw-greentic-llm-multiprovider`, commits 202e221 +
  195696d) — this spec is the remaining piece that makes v2 tools fully usable.
