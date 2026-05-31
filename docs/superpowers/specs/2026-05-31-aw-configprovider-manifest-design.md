# Agentic-Worker Tools Live: ConfigProvider + Manifest Auto-Load

**Date:** 2026-05-31
**Status:** Design (approved for planning)
**Repos:** `greentic-runner` (crates `greentic-aw-runtime`, `greentic-runner-host`) + `greentic-start`. All work on `research`.

## Overview

The agentic worker can already invoke extension tools — the pipeline
(`ConfigProvider` → run loop → `ExtensionRuntime::list_tools`/`invoke_tool`)
exists and is wired into `greentic-runner-host` behind `feature = "agentic-worker"`,
with a concrete `HostConfigProvider` that loads `AgentConfig` (including
`tools: Vec<ToolRef>`) from the bindings-YAML `agents:` section. What's missing
is the **convenience layer**: a designer-composed **Digital Worker manifest**
(`DigitalWorkerManifest`, with its `extension_tools` snapshot) is not auto-loaded
into `AgentConfig` — operators hand-write the tool list in YAML.

This feature delivers two phases:
- **A.** Prove/enable the existing manual path end-to-end (integration test + doc).
- **B.** A `ManifestConfigProvider` that auto-derives `AgentConfig` from a DW
  manifest loaded from a bundle discovery directory.

## Critical deployment constraint (drives the whole feature)

`feature = "agentic-worker"` depends on `greentic-aw-runtime` (`publish = false`)
and `greentic-ext-runtime` (private git). Those are **stripped from the published
crates.io crates** (the `1.2.0-research` publish stripped them to be publishable).
So a runner built from **published crates has no agentic worker**. The agent — and
therefore everything in this spec — only exists in a **source/git build with
`--features agentic-worker`**.

**Deployment decision (Option 2):** to run the agent inside the single
`gtc start` bundle (consistent with the project's "1 bundle" rule), `greentic-start`
depends on `greentic-runner-host` + `greentic-runner-desktop` via a **git
dependency pinned to a `research` rev** (NOT the stripped published crate), with
`agentic-worker` enabled. That pulls the full host (agent loop + aw-runtime +
ext-runtime + sql) into the bundle. The existing SQL-gateway injection stays,
now sourced via git. (The crates.io `1.2.0-research` publish remains valid for
non-agent consumers of the `sql` module, but is not the agent-delivery path.)

## Phase A — prove/enable the existing tool path

No new production code beyond a test + doc; this makes the existing pipeline
trustworthy and documented.

- **Integration test** (`greentic-aw-runtime`): build an `ExtensionRuntime` via
  `ExtensionRuntime::for_test()` with a fixture tool; an `InMemoryConfigProvider`
  (or `HostConfigProvider`) returning an `AgentConfig` whose `tools` contains the
  fixture `ToolRef`; a mock LLM (existing `mock.rs`) that emits one tool call for
  that tool. Run `AgentRuntime::step` and assert: the tool schema is listed for
  the LLM (`list_tools_for_llm`), the call is dispatched
  (`dispatch_tool_call` → `ExtensionRuntime::invoke_tool`), and the result returns
  into the conversation.
- **Doc**: the bindings-YAML `agents:` form showing manual
  `tools: [{ extension_id, tool_name }]` declaration (how to enable tools today).

## Phase B — `ManifestConfigProvider`

### Component

A new `ManifestConfigProvider` (in `greentic-aw-runtime`) implementing the
existing `ConfigProvider` trait:

```
fn agent_config(&self, tenant: &TenantContext, agent_id: &str)
    -> Future<Output = Result<AgentConfig, ConfigError>>
```

For `agent_id`:
1. Resolve the manifest path in the **agents discovery dir** (configurable; see
   below): `<agents_dir>/<agent_id>.cbor`.
2. Read + decode the `DigitalWorkerManifest` (CBOR via `ciborium`). Missing file →
   `ConfigError::AgentNotFound`; decode error → a decode `ConfigError`.
3. Build `AgentConfig`:
   - `agent_id` ← the requested id (assert it matches `manifest.id`).
   - `tools` ← `manifest_to_tool_refs(&manifest)` (existing, tested,
     `agentic_worker`-filtered + de-duped).
   - `system_prompt` / `llm` / `limits` ← from `manifest.deep_agent`
     (`DeepAgentConfig`) + contracts, with documented defaults for any field the
     manifest does not carry (see Open item 1).

### Discovery directory

A configurable agents directory, mirroring how extensions are discovered:
- env `GREENTIC_AGENTS_DIR` if set, else
- the bundle's `<bundle_root>/agents/` if running under a bundle, else
- `~/.greentic/agents/`.
`agent_id` maps to `<agents_dir>/<agent_id>.cbor` (Open item 3 confirms
filename-vs-`manifest.id`).

### Wiring (layered provider)

In `greentic-runner-host` `build_agent_node_handler()`, replace the bare
`HostConfigProvider` with a **layered provider**: a small `LayeredConfigProvider`
that first tries `ManifestConfigProvider` (manifest discovery) for the `agent_id`,
and on `AgentNotFound` **falls back to** `HostConfigProvider` (the YAML
`agents:`). Wrap the whole thing in the existing `CachingConfigProvider`. This
preserves Phase A's manual path and adds Phase B's auto-load.

### Manifest authoring/placement

`greentic-dw` (the DW compose CLI/wizard) already produces a
`DigitalWorkerManifest` (`extension_tools` snapshot at compose time). The operator
places the composed `<agent_id>.cbor` into the bundle's agents dir. (Open item 2
confirms dw-cli's exact output format/location.)

## Error handling

`ManifestConfigProvider` maps: missing manifest → `AgentNotFound` (so the layered
provider falls back to YAML); CBOR decode failure → a clear `ConfigError`;
`manifest.id != agent_id` → error. The loop's existing error handling is unchanged.

## Testing

- Phase A integration test (above).
- Phase B: unit test `manifest_to_agent_config(manifest) -> AgentConfig` (pure
  mapping, fixture manifest → asserts tools + system_prompt/llm/limits); a
  `ManifestConfigProvider` test loading a fixture `.cbor` from a temp dir →
  asserts the built `AgentConfig`; `AgentNotFound` on missing file; the layered
  provider falls back to YAML when no manifest.
- Reuse existing `manifest_tools.rs` tests for the tool-ref filtering.

## Open items (resolve in planning)

1. **`DeepAgentConfig` → `AgentConfig` mapping** — verify which of
   system_prompt/model/limits `deep_agent` (and the locale/tenancy contracts)
   actually carry; define defaults for any gaps (e.g. default `AgentLimits`,
   required `llm` provider). If `deep_agent` lacks `llm`, decide: error, or merge
   from a YAML/bundle default.
2. **dw-cli manifest output** — confirm how `greentic-dw` writes the
   `DigitalWorkerManifest` (CBOR? path?) so the discovery dir + decode match.
3. **`agent_id` ↔ manifest** — filename `<agent_id>.cbor` vs scanning for
   `manifest.id == agent_id`.
4. **git-dep rev pin** — tag `research` at a SHA for greentic-start's git
   dependency (reproducible build).
5. **greentic-start feature enablement** — confirm enabling `agentic-worker` on
   the git-dep host pulls aw-runtime + ext-runtime cleanly and greentic-start
   builds (it currently builds against the stripped published crate).

## End-to-end (target)

designer composes a DW (selects extension tools) → `DigitalWorkerManifest` →
operator drops `<agent_id>.cbor` in the bundle's agents dir → `gtc start`
(greentic-start, git-dep host with `agentic-worker`) → a `DwAgent` flow node for
`agent_id` → `ManifestConfigProvider` builds `AgentConfig` (tools from the
manifest) → the LLM loop lists + invokes the extension tools (Tavily / GitHub-MCP /
SQL) via `ExtensionRuntime` → the agent answers using live tools, all in one bundle.
