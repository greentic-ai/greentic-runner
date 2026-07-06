# AW Short-Term Memory MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an agentic worker's LLM call host built-in `remember`/`recall` tools backed by the in-memory short-term `MemoryProvider`, gated on `config.memory.short_term`.

**Architecture:** Mirror the existing long-term tier. Add a `short_term_memory` field + builder to `AgentRuntime`; a `short_term` module with the `"host"` tool schemas + an `active` gate; loop wiring that advertises + intercepts + dispatches the two tools; and a one-line host attach of the always-available `InMemoryMemoryProvider`.

**Tech Stack:** Rust (edition 2024), tokio, serde_json. Crates: `greentic-aw-runtime`, `greentic-runner-host`.

## Global Constraints

- Rust edition 2024. No `.unwrap()`/`.expect()` in non-test code.
- Reserved host extension id is `"host"` (same as long-term). Tool names exactly `"remember"` and `"recall"`.
- Tools are advertised + dispatched only when `short_term_active(provider_present, config)` is true (`config.memory.short_term` set AND a provider attached). Agents that don't opt in see nothing.
- Provider scoping is `(tenant, session_id, key)`; `session_id` is the `run_step` parameter.
- Conventional commits; NO Claude/AI co-author or attribution.
- Work in worktree `.worktrees/sp5c-shortterm-memory` on branch `feat/aw-short-term-memory-mvp` (greentic-runner). Do NOT touch the main worktree.
- Pre-commit/CI: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test`. Run the crate's tests before each commit: `cargo test -p greentic-aw-runtime`. (If a full clippy is blocked on a build lock or disk, run `cargo fmt --all` + the task's focused tests, commit, and note it.)

## Reference facts (verified against this checkout)

- `crates/greentic-aw-runtime/src/memory.rs`: `pub trait MemoryProvider: Send + Sync` with `remember<'a>(&'a self, tenant: &'a TenantContext, session_id: &'a str, record: MemoryRecord) -> Pin<Box<dyn Future<Output=Result<(), MemoryError>> + Send + 'a>>` and `recall<'a>(..., query: &'a MemoryQuery) -> Pin<Box<... Result<Option<MemoryRecord>, MemoryError> ...>>`. `MemoryRecord { key: String, value: String }`, `MemoryQuery { key: String }`. `pub struct InMemoryMemoryProvider` with `pub fn new()`.
- `crates/greentic-aw-runtime/src/lib.rs`: `AgentRuntime` has `pub(crate) long_term_memory: Option<Arc<dyn long_term::LongTermMemory>>` and `pub(crate) knowledge: ...`; `new()` initialises `long_term_memory: None, knowledge: None`; `#[must_use] pub fn with_long_term_memory(mut self, memory: Arc<dyn long_term::LongTermMemory>) -> Self { self.long_term_memory = Some(memory); self }`.
- `crates/greentic-aw-runtime/src/long_term.rs`: `pub(crate) const RECALL_MEMORY_EXTENSION_ID: &str = "host"`; `RECALL_MEMORY_TOOL: &str = "recall_memory"`; `pub(crate) fn long_term_active(has_provider: bool, config: &AgentConfig) -> bool`; `pub(crate) fn recall_memory_tool_schema() -> crate::llm::LlmToolSchema { LlmToolSchema { extension_id, tool_name, description, parameters: serde_json::json!({...}) } }`.
- `crates/greentic-aw-runtime/src/loop.rs`: `pub async fn run_step(... session_id: &str, ...)`; `let lt_active = crate::long_term::long_term_active(runtime.long_term_memory.is_some(), &config);` (~line 111); tool-schema push `if lt_active { tools_schema.push(crate::long_term::recall_memory_tool_schema()); }` (~line 195); the dispatch intercept arm for `recall_memory` (~lines 250-267) sits **before** `if !is_tool_allowed(...)`; `async fn host_recall_memory(runtime: &AgentRuntime, tenant: &TenantContext, call: &crate::state::ToolCallRecord) -> serde_json::Value` (~line 475).
- `crates/greentic-aw-runtime/src/state.rs`: `pub struct ToolCallRecord { call_id: String, extension_id: String, tool_name: String, args: serde_json::Value }`. `ChatMessage::Tool { call_id, content }` and `AgentStep::ToolCall { name, call_id, result }` are used by the recall_memory arm.
- `crates/greentic-runner-host/src/runner/agent_node.rs`: `build_agent_runtime()` builds `base` and attaches long-term/knowledge (`let base = crate::runner::long_term_memory::attach(base).await;` under a feature) before returning.
- Tests to mirror: `crates/greentic-aw-runtime/tests/loop_scripted.rs` long-term tests (`recall_memory_tool_advertised_when_active`, `recall_memory_call_is_handled_host_side`, etc.) — same harness for short-term.

---

## Task 1: `AgentRuntime` short-term field + builder

**Files:**
- Modify: `crates/greentic-aw-runtime/src/lib.rs`

**Interfaces:**
- Produces: `AgentRuntime.short_term_memory: Option<Arc<dyn crate::memory::MemoryProvider>>`; `#[must_use] pub fn with_short_term_memory(self, memory: Arc<dyn crate::memory::MemoryProvider>) -> Self`.

- [ ] **Step 1: Add the field**

In `crates/greentic-aw-runtime/src/lib.rs`, add to the `AgentRuntime` struct next to `knowledge`:

```rust
    /// Short-term ("working") memory backend, scoped per `(tenant, session, key)`.
    /// `None` disables the short-term tier. Set via
    /// [`AgentRuntime::with_short_term_memory`]; the host attaches the always-
    /// available in-memory provider, and the tools are gated by
    /// `config.memory.short_term`.
    pub(crate) short_term_memory: Option<Arc<dyn crate::memory::MemoryProvider>>,
```

- [ ] **Step 2: Initialise it in `new()`**

In the `Self { ... }` returned by `new()`, add `short_term_memory: None,` next to `long_term_memory: None,`.

- [ ] **Step 3: Add the builder**

Next to `with_long_term_memory`:

```rust
    /// Wire the short-term ("working") memory backend. Coexists with the
    /// long-term tier; defaults off when not set. The MVP host attaches the
    /// in-memory provider unconditionally; the `remember`/`recall` tools are
    /// advertised only when `config.memory.short_term` is set.
    #[must_use]
    pub fn with_short_term_memory(
        mut self,
        memory: Arc<dyn crate::memory::MemoryProvider>,
    ) -> Self {
        self.short_term_memory = Some(memory);
        self
    }
```

- [ ] **Step 4: Build to verify it compiles**

Run: `cargo build -p greentic-aw-runtime 2>&1 | tail -15`
Expected: compiles (the field is unused for now — `#[allow(dead_code)]` is NOT needed because `pub(crate)` + the builder reference it; if a dead-code warning appears under `-D warnings`, it is resolved by Task 3 which reads the field. To keep this commit clean on its own, it is acceptable that Task 1 + Task 2 + Task 3 land before a full `-D warnings` clippy; run `cargo build` here, not clippy).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw-runtime): add short-term memory field + builder to AgentRuntime"
```

(If the pre-commit clippy fails on a `field is never read` lint for `short_term_memory`, that read lands in Task 3; commit Task 1 with `--no-verify` after `cargo fmt --all` + `cargo build -p greentic-aw-runtime` succeed, and note it in the report. The lint clears once Task 3 lands.)

---

## Task 2: `short_term` module (tool schemas + active gate)

**Files:**
- Create: `crates/greentic-aw-runtime/src/short_term.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (add `mod short_term;`)
- Test: in `short_term.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::config::AgentConfig`, `crate::llm::LlmToolSchema`.
- Produces: `SHORT_TERM_EXTENSION_ID`, `REMEMBER_TOOL`, `RECALL_TOOL` consts; `pub(crate) fn short_term_active(has_provider: bool, config: &AgentConfig) -> bool`; `pub(crate) fn remember_tool_schema() -> LlmToolSchema`; `pub(crate) fn recall_tool_schema() -> LlmToolSchema`.

- [ ] **Step 1: Write the failing tests**

Create `crates/greentic-aw-runtime/src/short_term.rs` with the test module first (plus a temporary empty body so it compiles to a failure):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, MemoryProviderRef, MemorySettings};

    fn cfg_with_short_term(present: bool) -> AgentConfig {
        let mut c = AgentConfig::default();
        if present {
            c.memory = Some(MemorySettings {
                short_term: Some(MemoryProviderRef {
                    provider: "in-memory".into(),
                    capability: "cap://memory/short-term".into(),
                    params: serde_json::Map::new(),
                    credential_ref: None,
                }),
                long_term: None,
            });
        }
        c
    }

    #[test]
    fn active_only_when_provider_and_config_present() {
        assert!(short_term_active(true, &cfg_with_short_term(true)));
        assert!(!short_term_active(false, &cfg_with_short_term(true)));
        assert!(!short_term_active(true, &cfg_with_short_term(false)));
    }

    #[test]
    fn remember_schema_shape() {
        let s = remember_tool_schema();
        assert_eq!(s.tool_name, REMEMBER_TOOL);
        assert_eq!(s.extension_id, SHORT_TERM_EXTENSION_ID);
        let req = s.parameters.get("required").and_then(|v| v.as_array()).unwrap();
        let names: Vec<&str> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"key") && names.contains(&"value"));
    }

    #[test]
    fn recall_schema_shape() {
        let s = recall_tool_schema();
        assert_eq!(s.tool_name, RECALL_TOOL);
        assert_eq!(s.extension_id, SHORT_TERM_EXTENSION_ID);
        let req = s.parameters.get("required").and_then(|v| v.as_array()).unwrap();
        let names: Vec<&str> = req.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"key"));
    }
}
```

Note: `AgentConfig` may not derive `Default` — if it doesn't, build the config the way `cfg_with_short_term`'s sibling tests in `long_term.rs`/`config.rs` build theirs (check `config.rs` tests for the constructor used) and set `memory` accordingly. Use the real construction the codebase uses; do not invent a `Default` that isn't there.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p greentic-aw-runtime short_term 2>&1 | tail -20`
Expected: FAIL — `short_term_active`/schemas/consts not found (module body empty).

- [ ] **Step 3: Implement the module**

Prepend (above the test module) in `crates/greentic-aw-runtime/src/short_term.rs`:

```rust
//! Short-term ("working") memory tool surface for the agentic-worker loop.
//!
//! Mirrors [`crate::long_term`]'s host-tool pattern: a reserved `"host"`
//! extension id with built-in `remember`/`recall` tools, advertised + dispatched
//! only when the short-term tier is active. The backend is a
//! [`crate::memory::MemoryProvider`] wired via
//! [`crate::AgentRuntime::with_short_term_memory`].

use crate::config::AgentConfig;
use crate::llm::LlmToolSchema;

/// Reserved extension id for host-provided built-in tools (shared with long-term).
pub(crate) const SHORT_TERM_EXTENSION_ID: &str = "host";
/// Host built-in tool: store a key/value into short-term memory.
pub(crate) const REMEMBER_TOOL: &str = "remember";
/// Host built-in tool: read a value back from short-term memory by key.
pub(crate) const RECALL_TOOL: &str = "recall";

/// The short-term tier is active when a provider is attached AND the agent's
/// config opts in via `memory.short_term`. Mirrors [`crate::long_term::long_term_active`].
pub(crate) fn short_term_active(has_provider: bool, config: &AgentConfig) -> bool {
    has_provider
        && config
            .memory
            .as_ref()
            .and_then(|m| m.short_term.as_ref())
            .is_some()
}

/// LLM-facing schema for the host built-in `remember` tool.
pub(crate) fn remember_tool_schema() -> LlmToolSchema {
    LlmToolSchema {
        extension_id: SHORT_TERM_EXTENSION_ID.to_string(),
        tool_name: REMEMBER_TOOL.to_string(),
        description: "Store a short-term (working-memory) value under a key for this \
             conversation. Use to remember things the user tells you within this session."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Identifier to store the value under." },
                "value": { "type": "string", "description": "The value to remember." }
            },
            "required": ["key", "value"]
        }),
    }
}

/// LLM-facing schema for the host built-in `recall` tool.
pub(crate) fn recall_tool_schema() -> LlmToolSchema {
    LlmToolSchema {
        extension_id: SHORT_TERM_EXTENSION_ID.to_string(),
        tool_name: RECALL_TOOL.to_string(),
        description: "Read a short-term (working-memory) value back by the key it was \
             stored under in this conversation."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Identifier the value was stored under." }
            },
            "required": ["key"]
        }),
    }
}
```

Add `mod short_term;` to `crates/greentic-aw-runtime/src/lib.rs` next to `mod long_term;` (match its visibility — if `long_term` is `pub(crate) mod`/`mod`, mirror it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime short_term 2>&1 | tail -20`
Expected: PASS — the three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/short_term.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw-runtime): add short_term module (remember/recall tool schemas + gate)"
```

---

## Task 3: Loop wiring — advertise, intercept, dispatch

**Files:**
- Modify: `crates/greentic-aw-runtime/src/loop.rs`
- Test: `crates/greentic-aw-runtime/tests/loop_scripted.rs`

**Interfaces:**
- Consumes: Task 1 (`runtime.short_term_memory`), Task 2 (`short_term::*`), `MemoryRecord`/`MemoryQuery` from `crate::memory`.

- [ ] **Step 1: Write the failing tests**

In `crates/greentic-aw-runtime/tests/loop_scripted.rs`, mirror the long-term tests. First read how the long-term tests build a runtime + scripted LLM (`build_lt_runtime` and the `recall_memory_*` tests) and how they attach memory. Add a short-term runtime builder + tests:

```rust
// --- Short-term memory (MVP) ---
// Build a runtime with the in-memory short-term provider attached and a config
// whose memory.short_term is set, so short_term_active() is true.
fn build_st_runtime(
    llm_script: Vec<ScriptedTurn>, // use the same scripted-LLM type the lt tests use
    cfg_inner: AgentConfig,
) -> (AgentRuntime, TenantContext) {
    // Mirror build_lt_runtime, but call `.with_short_term_memory(Arc::new(
    // greentic_aw_runtime::memory::InMemoryMemoryProvider::new()))`.
    // Reuse the same ConfigProvider/state-store/llm wiring the lt builder uses.
    unimplemented!("fill in by mirroring build_lt_runtime in this file")
}
```

Then add these tests (fill the bodies by mirroring the long-term equivalents in the same file — same scripted-LLM, same `run_step` call, same assertions style):

```rust
#[tokio::test]
async fn remember_and_recall_tools_advertised_when_active() {
    // config.memory.short_term set + provider attached → the tool names list
    // the LLM receives includes "remember" and "recall".
}

#[tokio::test]
async fn short_term_tools_absent_when_disabled() {
    // config.memory.short_term = None → neither "remember" nor "recall" advertised.
}

#[tokio::test]
async fn remember_then_recall_roundtrips_via_tools() {
    // Script: turn 1 emits tool_call("c1","host","remember",{key:"fav",value:"blue"});
    // turn 2 emits tool_call("c2","host","recall",{key:"fav"}); assert the recall
    // tool result message content is {"value":"blue"}.
}

#[tokio::test]
async fn recall_missing_key_returns_null() {
    // tool_call recall {key:"absent"} → result content {"value": null}.
}
```

(The exact scripted-LLM types + `run_step(...)` signature live in this test file's existing long-term tests — copy that scaffolding verbatim, swapping `recall_memory` for `remember`/`recall` and the long-term mock for the in-memory short-term provider.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p greentic-aw-runtime --test loop_scripted short_term 2>&1 | tail -20`  (adjust the filter to the test names)
Expected: FAIL — tools not advertised / not handled (loop not wired yet).

- [ ] **Step 3: Compute `st_active` + advertise the tools**

In `crates/greentic-aw-runtime/src/loop.rs`, next to `let lt_active = ...;`:

```rust
    let st_active =
        crate::short_term::short_term_active(runtime.short_term_memory.is_some(), &config);
```

Where `recall_memory_tool_schema()` is pushed:

```rust
        if lt_active {
            tools_schema.push(crate::long_term::recall_memory_tool_schema());
        }
        if st_active {
            tools_schema.push(crate::short_term::remember_tool_schema());
            tools_schema.push(crate::short_term::recall_tool_schema());
        }
```

- [ ] **Step 4: Intercept + dispatch the two tools**

Immediately after the `recall_memory` intercept arm (before `if !is_tool_allowed(...)`), add:

```rust
                // --- Host built-in: short-term `remember` / `recall` ---
                if st_active && call.tool_name == crate::short_term::REMEMBER_TOOL {
                    observer.on_tool_call(&call.tool_name, &call.call_id);
                    let result = host_remember(runtime, &tenant, session_id, &call).await;
                    observer.on_tool_result(&call.tool_name, &call.call_id, &result);
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: result.clone(),
                    });
                    trail.push(AgentStep::ToolCall {
                        name: call.tool_name.clone(),
                        call_id: call.call_id,
                        result,
                    });
                    continue;
                }
                if st_active && call.tool_name == crate::short_term::RECALL_TOOL {
                    observer.on_tool_call(&call.tool_name, &call.call_id);
                    let result = host_recall(runtime, &tenant, session_id, &call).await;
                    observer.on_tool_result(&call.tool_name, &call.call_id, &result);
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: result.clone(),
                    });
                    trail.push(AgentStep::ToolCall {
                        name: call.tool_name.clone(),
                        call_id: call.call_id,
                        result,
                    });
                    continue;
                }
```

(Match the exact field/variant names used by the `recall_memory` arm in this file — `observer`, `ChatMessage::Tool`, `AgentStep::ToolCall`, `trail`. If the recall_memory arm differs, mirror it exactly.)

- [ ] **Step 5: Add the handlers**

Next to `host_recall_memory`, add:

```rust
/// Handle a host built-in `remember` call: store `{key, value}` into short-term
/// memory for this `(tenant, session)`. Returns `{"ok": true}` or `{"error": ...}`.
async fn host_remember(
    runtime: &AgentRuntime,
    tenant: &TenantContext,
    session_id: &str,
    call: &crate::state::ToolCallRecord,
) -> serde_json::Value {
    let Some(provider) = runtime.short_term_memory.as_ref() else {
        return serde_json::json!({ "error": "short-term memory not configured" });
    };
    let key = call.args.get("key").and_then(|v| v.as_str()).unwrap_or_default();
    let value = call.args.get("value").and_then(|v| v.as_str()).unwrap_or_default();
    if key.is_empty() {
        return serde_json::json!({ "error": "missing 'key'" });
    }
    let record = crate::memory::MemoryRecord {
        key: key.to_string(),
        value: value.to_string(),
    };
    match provider.remember(tenant, session_id, record).await {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
}

/// Handle a host built-in `recall` call: read a value back by `key`. Returns
/// `{"value": <string|null>}` or `{"error": ...}`.
async fn host_recall(
    runtime: &AgentRuntime,
    tenant: &TenantContext,
    session_id: &str,
    call: &crate::state::ToolCallRecord,
) -> serde_json::Value {
    let Some(provider) = runtime.short_term_memory.as_ref() else {
        return serde_json::json!({ "error": "short-term memory not configured" });
    };
    let key = call.args.get("key").and_then(|v| v.as_str()).unwrap_or_default();
    if key.is_empty() {
        return serde_json::json!({ "error": "missing 'key'" });
    }
    let query = crate::memory::MemoryQuery { key: key.to_string() };
    match provider.recall(tenant, session_id, &query).await {
        Ok(Some(record)) => serde_json::json!({ "value": record.value }),
        Ok(None) => serde_json::json!({ "value": serde_json::Value::Null }),
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime 2>&1 | tail -25`
Expected: PASS — the new short-term loop tests + all existing aw-runtime tests (incl. long-term).

- [ ] **Step 7: Commit**

```bash
git add crates/greentic-aw-runtime/src/loop.rs crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "feat(aw-runtime): wire short-term remember/recall host tools into the loop"
```

---

## Task 4: Host attach the in-memory provider

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs`

**Interfaces:**
- Consumes: `AgentRuntime::with_short_term_memory` (Task 1), `InMemoryMemoryProvider` (`greentic_aw_runtime::memory`).

- [ ] **Step 1: Attach the provider in `build_agent_runtime()`**

In `crates/greentic-runner-host/src/runner/agent_node.rs`, in `build_agent_runtime()`, beside the long-term/knowledge attaches (after the `base` runtime is constructed, before it is returned), add:

```rust
    // Short-term ("working") memory: the in-memory provider is always available
    // (no external deps); the `remember`/`recall` tools stay gated by
    // `config.memory.short_term` in the agentic loop, so agents that don't opt
    // in see no tools and incur no overhead.
    let base = base.with_short_term_memory(std::sync::Arc::new(
        greentic_aw_runtime::memory::InMemoryMemoryProvider::new(),
    ));
```

(Confirm `with_short_term_memory` and `memory::InMemoryMemoryProvider` are reachable from this crate — `greentic_aw_runtime` is a dependency here; if the `memory` module is not `pub`, expose `InMemoryMemoryProvider` via the crate's public surface in `lib.rs` the same way other host-injected types are exported, and note it. Prefer `pub use`/`pub mod memory` only if needed.)

- [ ] **Step 2: Build + test the host crate**

Run: `cargo build -p greentic-runner-host 2>&1 | tail -15`
Expected: compiles.

Run: `cargo test -p greentic-runner-host 2>&1 | tail -15`
Expected: existing tests pass (no regression).

- [ ] **Step 3: Full clippy across the workspace**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -20`
Expected: clean (this also clears any Task-1 `field is never read` lint, now that the loop reads `short_term_memory`).

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs
git commit -m "feat(runner-host): attach in-memory short-term provider to agent runtime"
```

---

## Manual verification (after Task 4)

- An agent whose `AgentConfig.memory.short_term` is set advertises `remember`/`recall` to the LLM; calling `remember{key,value}` then `recall{key}` returns the stored value within the same session. An agent without `memory.short_term` advertises neither tool.

## Self-Review (completed during planning)

- **Spec coverage:** §4.1 runtime seam → Task 1; §4.2 module → Task 2; §4.3 loop wiring → Task 3; §4.4 host attach → Task 4; §7 testing folded into Tasks 2–3.
- **Placeholder scan:** the `short_term.rs` module + handlers + schemas are complete code; the loop-test bodies are described against the exact long-term tests in the same file to copy (the scripted-LLM types are file-local and must be mirrored, not invented).
- **Type consistency:** `short_term_active(has_provider, config)`, `REMEMBER_TOOL`/`RECALL_TOOL`/`SHORT_TERM_EXTENSION_ID`, `with_short_term_memory`, `runtime.short_term_memory`, `MemoryRecord{key,value}`/`MemoryQuery{key}` are consistent across Tasks 1–4 and match the verified signatures.
