# Agentic-Worker Tools Live: ConfigProvider + Manifest Auto-Load

**Date:** 2026-05-31
**Status:** Design (revised after planning research — see "Planning corrections")
**Repos:** `greentic-runner` (crates `greentic-aw-runtime`, `greentic-runner-host`) + `greentic-start`. All work on `research`.

## Planning corrections (2026-05-31)

Reading the actual source corrected two premises the first draft relied on:

1. **The manifest cannot produce a full `AgentConfig`.** `DigitalWorkerManifest`
   (and its `deep_agent: DeepAgentConfig`) carry **no `system_prompt` and no
   `llm` provider/model** — `deep_agent` holds only capability references.
   `AgentConfig.system_prompt` / `.llm` / `.limits` exist **only in the
   operator's YAML** (`HostConfig.agents`, parsed by `HostConfigProvider`). So a
   manifest can supply `tools` (+ `agent_id`), nothing else.
2. **There is no `.cbor` manifest file.** greentic-dw's wizard emits the manifest
   as **JSON** (`serde_json::to_string_pretty`, `greentic-dw-cli/src/wizard.rs:218`)
   and the real artifact is materialised into a `.gtpack` ZIP downstream — no
   `<agent_id>.cbor` convention exists.

Consequence: **Phase B is a tools *overlay*, not a fallback.** The YAML
`HostConfigProvider` stays authoritative for `system_prompt`/`llm`/`limits`; the
manifest only overrides `AgentConfig.tools`. The manifest file is **JSON**
(the exact `serde_json` shape the wizard already emits), discovered at
`<manifests_dir>/<agent_id>.json`.

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
- **B.** A `ManifestToolOverlayProvider` decorator that auto-derives
  `AgentConfig.tools` from a DW manifest (JSON) discovered on disk, overlaying it
  onto the YAML base config (which keeps supplying `system_prompt`/`llm`/`limits`).

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

- **Integration test** (`greentic-aw-runtime/tests/`): `ExtensionRuntime::for_test()`
  is **empty** (no extensions; `list_tools`/`invoke_tool` return NotFound). A real
  dispatch test must load a fixture extension via `signed_fixture(...)` +
  `register_loaded_from_dir(...)` (the `scaffold_e2e.rs` / `runtime_load.rs`
  pattern in greentic-ext-runtime, using `greentic-extension-sdk-testing`). Build
  an `AgentRuntime` with that fixture-loaded `ExtensionRuntime`, an
  `InMemoryConfigProvider` returning an `AgentConfig` whose `tools` holds the
  fixture `ToolRef`, and a `MockLlmBackend` (from `mock.rs`) scripted to emit one
  `ToolCallRecord` for that tool then a final reply. Run `AgentRuntime::step` and
  assert the fixture tool was listed (`list_tools_for_llm` non-empty) and
  dispatched (`dispatch_tool_call` → `invoke_tool`, result threaded back). The
  scaffolded fixture tool returns a stub result — assert dispatch occurred, not a
  specific business value. (Planning resolves whether to reuse ext-runtime's
  `tests/support` helpers or vendor a minimal signed fixture into aw-runtime.)
- **Doc**: the bindings-YAML `agents:` form (see `HostConfig` parsing) showing
  manual `tools: [{ extension_id, tool_name }]` declaration — how to enable tools
  today without a manifest.

## Phase B — `ManifestToolOverlayProvider`

### Component

A new `ManifestToolOverlayProvider<P: ConfigProvider>` (in `greentic-aw-runtime`),
a **decorator** wrapping a base provider `P` (the YAML `HostConfigProvider`). It
implements the existing `ConfigProvider` trait:

```
fn agent_config(&self, tenant: &TenantContext, agent_id: &str)
    -> Future<Output = Result<AgentConfig, ConfigError>>
```

Behaviour for `agent_id`:
1. `base = self.inner.agent_config(tenant, agent_id).await?` — this yields
   `system_prompt`, `llm`, `limits` (and `base.tools` as the fallback tool list).
   If the inner returns `AgentNotFound`, propagate it unchanged: with no base
   there is no LLM/prompt, so the agent cannot run.
2. Resolve the manifest path `<manifests_dir>/<agent_id>.json` (discovery dir
   below). If the file is absent, return `base` unchanged (YAML tools win).
3. If present: read + `serde_json::from_slice::<DigitalWorkerManifest>`, then
   `manifest.validate()`. On decode/validation failure, **log a warning and
   return `base` unchanged** (fail-soft — a malformed manifest must not take the
   agent down; the YAML config still serves).
4. On success: `base.tools = manifest_to_tool_refs(&manifest)` (existing, tested,
   `agentic_worker`-filtered + order-preserving + de-duped) and return `base`.
   `system_prompt`/`llm`/`limits` are untouched — only the tool set is overlaid.

The manifest supplies **only** the tool set. Everything else stays YAML-authored.

### Discovery directory

A configurable manifests directory, mirroring how extensions are discovered
(`agent_node.rs::extension_discovery_dir`):
- env `GREENTIC_AGENT_MANIFESTS_DIR` if set and non-empty, else
- `~/.greentic/agents/` (via `$HOME`), else
- `std::env::temp_dir()/greentic/agents` (last-resort, keeps the fn total).

`agent_id` maps to `<manifests_dir>/<agent_id>.json`. A manifest whose
`manifest.id != agent_id` is logged and ignored (the file is keyed by filename,
not by re-scanning every manifest).

### Manifest file format

The exact JSON `DigitalWorkerManifest` shape greentic-dw's wizard already emits
(`serde_json`). v1 expects the operator to drop `<agent_id>.json` into the
manifests dir. (Auto-extracting it from the `.gtpack` ZIP is a later nicety —
Open item 2.)

### Wiring (overlay decorator)

In `greentic-runner-host` `build_agent_node_handler()`, wrap the existing
`HostConfigProvider` in `ManifestToolOverlayProvider::new(host_provider,
manifests_dir)`, then wrap that in the existing `CachingConfigProvider`. This
preserves Phase A's YAML path exactly (no manifest → base returned unchanged) and
adds Phase B's tool auto-overlay when a manifest is present.

## Error handling (Phase B is fail-soft)

`ManifestToolOverlayProvider` never *introduces* a failure: a missing manifest,
a JSON decode error, a failed `manifest.validate()`, or an `id` mismatch all
**log a warning and return the YAML base unchanged**. The only error it
propagates is the inner provider's `AgentNotFound` (no base config at all). This
guarantees a broken manifest degrades to the operator's YAML tool list rather
than taking the agent offline. The loop's existing error handling is unchanged.

## Testing

- Phase A integration test (above).
- Phase B unit tests (all in `greentic-aw-runtime`, using `InMemoryConfigProvider`
  as the base + a `tempfile` manifests dir):
  - manifest present + valid → `base.tools` replaced by the manifest's
    agentic-worker tool refs; `system_prompt`/`llm`/`limits` unchanged.
  - manifest absent → `base` returned verbatim (YAML tools preserved).
  - manifest malformed JSON → warning + `base` returned verbatim.
  - manifest `id` ≠ `agent_id` → ignored + `base` returned verbatim.
  - inner `AgentNotFound` → propagated.
- Reuse existing `manifest_tools.rs` tests for the tool-ref filtering itself.

## Open items (resolve in planning)

1. ~~`DeepAgentConfig` → `AgentConfig` mapping~~ — RESOLVED: the manifest carries
   no prompt/llm/limits; Phase B overlays `tools` only and leaves the rest to YAML.
2. **`.gtpack` extraction** — v1 reads a loose `<agent_id>.json`. Auto-extracting
   the manifest from the composed `.gtpack` ZIP is deferred; document the manual
   drop for now.
3. ~~`agent_id` ↔ manifest~~ — RESOLVED: keyed by filename `<agent_id>.json`;
   `manifest.id != agent_id` is ignored with a warning.
4. **git-dep rev pin** — tag `research` at a SHA for greentic-start's git
   dependency (reproducible build).
5. **greentic-start feature enablement** — confirm enabling `agentic-worker` on
   the git-dep host pulls aw-runtime + ext-runtime cleanly and greentic-start
   builds (it currently builds against the stripped published crate).

## End-to-end (target)

operator declares the agent's `system_prompt` + `llm` once in the bundle YAML →
designer composes a DW (selects extension tools) → `DigitalWorkerManifest` (JSON)
→ operator drops `<agent_id>.json` in the manifests dir → `gtc start`
(greentic-start, git-dep host with `agentic-worker`) → a `DwAgent` flow node for
`agent_id` → `ManifestToolOverlayProvider` reads the YAML base and overlays the
manifest's tool set onto `AgentConfig.tools` → the LLM loop lists + invokes the
extension tools (Tavily / GitHub-MCP / SQL) via `ExtensionRuntime` → the agent
answers using live tools, all in one bundle.
