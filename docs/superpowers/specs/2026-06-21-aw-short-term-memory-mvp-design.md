# Agentic Worker Short-Term Memory MVP (Design)

- **Date:** 2026-06-21
- **Status:** Design approved, ready for planning
- **Surface:** greentic-runner (`greentic-aw-runtime` + `greentic-runner-host`). Runner-only.
- **Part of:** AW Composer roadmap SP-5 ("memory & RAG"). This is the runner half (SP-5c). The designer half (bump runner rev + map `knowledge` into `AgentConfig`) follows as a separate greentic-designer PR once this lands.

## 1. Background

Knowledge/RAG and long-term memory already work in the agentic-worker loop. **Short-term
("working") memory does not**: `crates/greentic-aw-runtime/src/memory.rs` ships the
`MemoryProvider` trait + an always-available `InMemoryMemoryProvider`, and
`AgentConfig.memory.short_term` exists, but nothing consumes it — `loop.rs` never calls a
`MemoryProvider`, `AgentRuntime` has no short-term field, and the host never attaches a
provider.

The long-term tier is the exact pattern to mirror: a reserved `"host"` extension id,
built-in tool(s) advertised only when the tier is active, intercepted in the loop before the
allow-list, dispatched to the runtime's backend.

## 2. Goal (MVP)

When an agent's config opts in (`config.memory.short_term` set), the LLM can call two
host built-in tools — **`remember`** and **`recall`** — backed by the in-memory provider,
scoped to `(tenant, session_id, key)`. No new dependencies, no feature flags, no schema
changes, no external backend (Redis comes later).

## 3. Non-goals

- No persistent backend (Redis/Chronicle) — the in-memory provider only (per-process,
  lost on restart). The seam is built so a real backend swaps in later via
  `with_short_term_memory`.
- No auto-injection of short-term memory into the system prompt (long-term does that for
  facts; short-term MVP is tool-driven only).
- No designer/UI changes (separate PR).

## 4. Design (mirror the long-term tier)

### 4.1 Runtime seam — `AgentRuntime`
Add `pub(crate) short_term_memory: Option<Arc<dyn MemoryProvider>>` (default `None`) and a
`#[must_use] pub fn with_short_term_memory(mut self, memory: Arc<dyn MemoryProvider>) -> Self`,
mirroring `with_long_term_memory` (`crates/greentic-aw-runtime/src/lib.rs`).

### 4.2 New module `crates/greentic-aw-runtime/src/short_term.rs`
Mirrors `long_term.rs`'s tool surface:
- `pub(crate) const SHORT_TERM_EXTENSION_ID: &str = "host";`
- `pub(crate) const REMEMBER_TOOL: &str = "remember";`
- `pub(crate) const RECALL_TOOL: &str = "recall";`
- `pub(crate) fn short_term_active(has_provider: bool, config: &AgentConfig) -> bool` →
  `has_provider && config.memory.as_ref().and_then(|m| m.short_term.as_ref()).is_some()`
  (mirror `long_term_active`).
- `pub(crate) fn remember_tool_schema() -> LlmToolSchema` — params `{ key, value }`, both
  required.
- `pub(crate) fn recall_tool_schema() -> LlmToolSchema` — params `{ key }`, required.

### 4.3 Loop wiring — `crates/greentic-aw-runtime/src/loop.rs`
- Compute `let st_active = crate::short_term::short_term_active(runtime.short_term_memory.is_some(), &config);` next to `lt_active`.
- When `st_active`, push `remember_tool_schema()` + `recall_tool_schema()` onto `tools_schema`
  (beside the existing `recall_memory` push).
- In the tool-call loop, **before the allow-list check**, intercept the two host tools
  (beside the `recall_memory` arm):
  - `remember`: parse `{key, value}` → `provider.remember(&tenant, session_id, MemoryRecord{key,value})` → result `{"ok": true}` or `{"error": ...}`.
  - `recall`: parse `{key}` → `provider.recall(&tenant, session_id, &MemoryQuery{key})` → `{"value": <string|null>}` or `{"error": ...}`.
  - Both record the tool result into `state.messages` + `trail` exactly like `recall_memory`, then `continue`.
- Handlers `host_remember(...)` / `host_recall(...)` mirror `host_recall_memory` (read `call.args`, call the provider via `runtime.short_term_memory.as_ref()`, return a JSON value). `session_id` is already in `run_step` scope.

### 4.4 Host attach — `crates/greentic-runner-host/src/runner/agent_node.rs`
In `build_agent_runtime()`, attach the always-available provider (no feature gate):
`let base = base.with_short_term_memory(Arc::new(greentic_aw_runtime::memory::InMemoryMemoryProvider::new()));`
Placed beside the long-term/knowledge attach. Tools stay gated by `config.memory.short_term`
in the loop, so agents that don't opt in see no `remember`/`recall` and incur no overhead.

## 5. Boundaries

- `memory.rs` (trait + `InMemoryMemoryProvider`) and `config.rs` (`MemorySettings.short_term`)
  are unchanged — already correct.
- New surface is one runtime field + builder, one small module, the loop intercepts/handlers,
  and one host line. Each unit is independently testable.

## 6. Error handling

- Provider errors (`MemoryError`) surface to the LLM as `{"error": "<message>"}` — never panic,
  no `unwrap`/`expect` in non-test code (the in-memory provider already maps lock-poisoning to
  `MemoryError::Backend`).
- Missing `key`/`value` args → an `{"error": ...}` result the LLM can react to (mirror how
  `host_recall_memory` defaults/uses args).
- `recall` of an absent key → `{"value": null}` (not an error).

## 7. Testing

- **Unit (`short_term.rs`):** `remember_tool_schema`/`recall_tool_schema` carry the right
  `extension_id`/`tool_name`/required params; `short_term_active` true only when provider
  present AND `config.memory.short_term` set.
- **Loop (`tests/loop_scripted.rs`, mirror the long-term tests):**
  - `remember`/`recall` advertised when short-term active; absent when `config.memory.short_term` is `None`.
  - A scripted `tool_call("host", "remember", {key,value})` stores the pair; a following
    `recall` returns the value.
  - `recall` of a missing key returns `{"value": null}`.
- **Memory unit tests** in `memory.rs` already cover the provider round-trip.

## 8. Risks

- Low — additive, mirrors a proven tier, in-memory only, fully config-gated. The one
  cross-crate touch (host attach) is a single line. Cross-repo follow-up (designer rev-bump +
  knowledge mapping) is tracked separately and not part of this PR.
