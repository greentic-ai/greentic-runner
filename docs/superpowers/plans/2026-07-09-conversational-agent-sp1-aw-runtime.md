# Conversational Agent — SP1 (aw-runtime exit signal) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give a `dw.agent` a way to signal "the conversation is over" — a built-in `end_conversation` tool that, when called, terminates the Plan-Act-Observe loop with a new `TerminationReason::ConversationEnded` and renders the agent's closing message as the final reply. Purely additive; non-conversational agents are byte-unchanged.

**Architecture:** Mirror the existing host built-in tool pattern (`short_term::remember`/`recall`, `long_term::recall_memory`). A new opt-in `AgentConfig.conversational: bool` derives a `conv_active` gate in `loop.rs`; when active the loop (a) advertises an `end_conversation` tool schema, (b) appends a one-line system-prompt note telling the agent it may end the conversation, and (c) intercepts the `end_conversation` tool call before the allow-list — setting `terminated_by = ConversationEnded`, using the same-turn assistant text (or the tool's `note`) as `reply`, and returning through the existing FinalReply outbound-guardrail/save/telemetry path so the closing reply renders exactly as a normal reply does today.

**Tech Stack:** Rust (edition 2024, pinned 1.94.0 per `rust-toolchain.toml`), `serde` (snake_case enum), the existing aw-runtime scripted-mock test harness (`tests/loop_scripted.rs`, `MockLlmBackend`, `MockConfigProvider`).

## Global Constraints

- **Repo:** greentic-runner only. Crate: `greentic-aw-runtime`. Branch: `feat/conversational-agent-sp1` → `research` (already created; base = `research` @ #535 merge `d17177e4`).
- **Additive / backward compatible:** `TerminationReason::ConversationEnded` is a new variant (existing matches add an arm, never change one); `AgentConfig.conversational` defaults `false` via `#[serde(default)]`; the `end_conversation` tool is offered ONLY when `conv_active`, so every existing agent's tool set + behaviour is unchanged.
- **`end_conversation` is a host built-in**, extension id `"host"` (shared with `short_term`/`long_term`), tool name `end_conversation`. Intercepted BEFORE the allow-list (like `remember`/`recall`) — it does NOT need to be in `config.tools`.
- **Reply source (spec §SP1):** the closing `reply` is the same-turn assistant `content` if non-empty, else the tool's `note` arg, else empty. It must flow through the existing outbound-guardrail + state-save + telemetry + return path (currently gated on `FinalReply`).
- **Do NOT change** the one-shot path (`FinalReply`/`MaxIterations`/`Timeout`/`Error`/`TokenBudgetExceeded` behaviour), the output contract shape (`{reply, trail, terminated_by}`), or any SP2/SP3/SP4 concern (the flow-node park-loop, the flow-doc flag, the designer toggle). SP1 only makes the runtime *able* to end a conversation; who sets `conversational: true` is SP2/SP3.
- **`ToolCallRecord` field is `args`** (`state.rs:109`), not `arguments`.
- **Conventional commits, NO Claude co-author** (per `greentic-runner/CLAUDE.md` "Git Conventions").
- **Build discipline (shared machine):** `CARGO_BUILD_JOBS=2 cargo ... -j2`, FOREGROUND; scope to `-p greentic-aw-runtime`; never `pkill`/`kill` or delete another worktree's `target/`. Avoid `--all-features` (surrealdb env-fail); default features are enough for this crate's tests.

## File Structure

- **Create:** `crates/greentic-aw-runtime/src/conversation.rs` — the `end_conversation` host-tool surface: constants, `conversational_active(&AgentConfig)`, `end_conversation_tool_schema()`, `augment_system_prompt(&str)`. Mirrors `short_term.rs` exactly (one clear responsibility: the conversational-exit tool surface).
- **Modify:** `crates/greentic-aw-runtime/src/error.rs` — add `ConversationEnded` to `TerminationReason`.
- **Modify:** `crates/greentic-aw-runtime/src/config.rs` — add `conversational: bool` field to `AgentConfig`.
- **Modify:** `crates/greentic-aw-runtime/src/lib.rs` — declare `pub mod conversation;`.
- **Modify:** `crates/greentic-aw-runtime/src/loop.rs` — derive `conv_active`, advertise the schema, augment the prompt, intercept the tool call, extend the return guard. Fix any `AgentConfig { .. }` literal in this crate + downstream crates that the new field breaks (compile fan-out, same as commit `ef355c47`).
- **Test:** inline `#[cfg(test)]` in `error.rs`, `conversation.rs`; integration test in `crates/greentic-aw-runtime/tests/loop_scripted.rs`.

---

### Task 1: Add `TerminationReason::ConversationEnded` (error.rs)

**Files:**
- Modify: `crates/greentic-aw-runtime/src/error.rs:135-141` (the `TerminationReason` enum)
- Test: inline `#[cfg(test)]` in `error.rs` (append to the existing `mod tests`)

**Interfaces:**
- Produces: `TerminationReason::ConversationEnded` — serializes (serde `rename_all = "snake_case"`, already on the enum) to `"conversation_ended"`. Consumed by Task 3 (`loop.rs`) and, downstream, by SP2.

- [ ] **Step 1: Write the failing test.** Append to `mod tests` in `error.rs`:

```rust
#[test]
fn conversation_ended_serializes_snake_case() {
    let json = serde_json::to_string(&TerminationReason::ConversationEnded).unwrap();
    assert_eq!(json, "\"conversation_ended\"");
    let back: TerminationReason = serde_json::from_str("\"conversation_ended\"").unwrap();
    assert_eq!(back, TerminationReason::ConversationEnded);
}
```

- [ ] **Step 2: Run — expect FAIL** (no such variant):

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 conversation_ended_serializes_snake_case`
Expected: FAIL — compile error `no variant named ConversationEnded`.

- [ ] **Step 3: Implement.** Add the variant to the enum (after `FinalReply`):

```rust
pub enum TerminationReason {
    FinalReply,
    /// The agent called the built-in `end_conversation` tool: the conversational
    /// segment is complete and the flow may advance to this node's successor.
    ConversationEnded,
    MaxIterations,
    Timeout,
    Error,
    TokenBudgetExceeded,
}
```

- [ ] **Step 4: Run — expect PASS.**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 conversation_ended_serializes_snake_case`
Expected: PASS. (If any OTHER crate has an exhaustive `match` on `TerminationReason` it will now fail to compile — grep `rg "TerminationReason::" -l` and add a `ConversationEnded => ...` arm mirroring the `FinalReply` arm. At time of writing there are none in non-test code; telemetry uses `Debug`/serde, not an exhaustive match.)

- [ ] **Step 5: Commit.**

```bash
git add crates/greentic-aw-runtime/src/error.rs
git commit -m "feat(aw-runtime): add TerminationReason::ConversationEnded variant"
```

---

### Task 2: `conversation.rs` tool surface + `AgentConfig.conversational` field

**Files:**
- Create: `crates/greentic-aw-runtime/src/conversation.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (add `pub mod conversation;` alongside the other `pub mod` decls at ~:29/:37/:45)
- Modify: `crates/greentic-aw-runtime/src/config.rs:105-119` (`AgentConfig` — add `conversational` field)
- Modify: every `AgentConfig { .. }` struct literal that the new field breaks (compile fan-out — see Step 5)
- Test: inline `#[cfg(test)]` in `conversation.rs`

**Interfaces:**
- Consumes: `AgentConfig` (`config.rs`), `LlmToolSchema` (`llm.rs`).
- Produces:
  - `pub(crate) const CONVERSATION_EXTENSION_ID: &str = "host";`
  - `pub(crate) const END_CONVERSATION_TOOL: &str = "end_conversation";`
  - `pub(crate) fn conversational_active(config: &AgentConfig) -> bool`
  - `pub(crate) fn end_conversation_tool_schema() -> LlmToolSchema`
  - `pub(crate) fn augment_system_prompt(base: &str) -> String`
  - `AgentConfig.conversational: bool` (new field, `#[serde(default)]`)

- [ ] **Step 1: Add the `AgentConfig` field first.** In `config.rs`, after the `knowledge` field (`:118`):

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<KnowledgeSettings>,
    /// Opt-in: this node is a conversational segment. When true the runtime
    /// advertises the built-in `end_conversation` tool and terminates the loop
    /// with `TerminationReason::ConversationEnded` when the agent calls it.
    /// Set by the flow node (SP2/SP3); defaults false ⇒ one-shot behaviour.
    #[serde(default)]
    pub conversational: bool,
}
```

- [ ] **Step 2: Write the failing tests** in a new `crates/greentic-aw-runtime/src/conversation.rs`:

```rust
//! Conversational-segment exit tool for the agentic-worker loop.
//!
//! Mirrors [`crate::short_term`]'s host-tool pattern: a reserved `"host"`
//! extension id with a built-in `end_conversation` tool, advertised +
//! intercepted only when the node opts in via `AgentConfig.conversational`.

use crate::config::AgentConfig;
use crate::llm::LlmToolSchema;

/// Reserved extension id for host-provided built-in tools (shared with memory tiers).
pub(crate) const CONVERSATION_EXTENSION_ID: &str = "host";
/// Host built-in tool: end the current conversational segment.
pub(crate) const END_CONVERSATION_TOOL: &str = "end_conversation";

/// One-line system-prompt note appended for conversational nodes so the model
/// knows it MAY end the conversation (spec §"Tool discoverability").
pub(crate) const END_CONVERSATION_SYSTEM_NOTE: &str =
    "When the user's goal for this conversation has been met, call the \
     `end_conversation` tool to finish. Put your closing message to the user in \
     your assistant reply on that same turn (or pass it as the tool's `note`).";

/// Conversational mode is active when the agent's config opts in.
pub(crate) fn conversational_active(config: &AgentConfig) -> bool {
    config.conversational
}

/// LLM-facing schema for the host built-in `end_conversation` tool.
pub(crate) fn end_conversation_tool_schema() -> LlmToolSchema {
    LlmToolSchema {
        extension_id: CONVERSATION_EXTENSION_ID.to_string(),
        tool_name: END_CONVERSATION_TOOL.to_string(),
        description: "End the current conversation once the user's goal has been \
             met. Provide your closing message as your assistant reply this turn, \
             or pass it as `note`. After this the flow advances to the next step."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Optional closing message to show the user."
                }
            }
        }),
    }
}

/// Append the end-conversation note to a system prompt (call only when active).
pub(crate) fn augment_system_prompt(base: &str) -> String {
    format!("{base}\n\n{END_CONVERSATION_SYSTEM_NOTE}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};

    fn cfg(conversational: bool) -> AgentConfig {
        AgentConfig {
            agent_id: "a".into(),
            system_prompt: "sys".into(),
            tools: vec![],
            guardrails: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "gpt-4".into(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
            memory: None,
            knowledge: None,
            conversational,
        }
    }

    #[test]
    fn active_follows_config_flag() {
        assert!(conversational_active(&cfg(true)));
        assert!(!conversational_active(&cfg(false)));
    }

    #[test]
    fn schema_names_the_host_builtin() {
        let s = end_conversation_tool_schema();
        assert_eq!(s.extension_id, "host");
        assert_eq!(s.tool_name, "end_conversation");
        assert!(s.parameters["properties"]["note"].is_object());
    }

    #[test]
    fn augment_appends_the_note_once() {
        let out = augment_system_prompt("BASE");
        assert!(out.starts_with("BASE"));
        assert!(out.contains("end_conversation"));
    }
}
```

- [ ] **Step 3: Declare the module.** In `lib.rs`, add next to the sibling `pub mod` lines:

```rust
pub mod conversation;
```

- [ ] **Step 4: Run — expect FAIL then iterate to compile.** First failure is the workspace-wide `AgentConfig { .. }` literals now missing `conversational`.

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 conversation::`
Expected: FAIL — `missing field conversational in initializer of AgentConfig` at several sites.

- [ ] **Step 5: Fix every broken literal.** Find them and add `conversational: false,` (or `true` where a test intends conversational) to each:

```bash
rg -n "AgentConfig \{" crates/ | rg -v "pub struct AgentConfig"
```

Expected sites include `error.rs` `config_with`, `mock.rs`, `loop.rs` test helpers, `tests/loop_scripted.rs` config builders, and any `greentic-runner-host` construction of `AgentConfig` (e.g. `runner/agent_node.rs`, `runner/graph_node.rs`). For non-test/product literals add `conversational: false,`. Mirror commit `ef355c47` (the same fan-out for the ToolRef author-contract fields). Deserialized configs need no change (`#[serde(default)]`).

- [ ] **Step 6: Run — expect PASS.**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 conversation::`
Expected: PASS (3 tests).

- [ ] **Step 7: Commit.**

```bash
git add crates/greentic-aw-runtime/src/conversation.rs crates/greentic-aw-runtime/src/lib.rs crates/greentic-aw-runtime/src/config.rs
git add -u   # picks up the fixed AgentConfig literals across crates
git commit -m "feat(aw-runtime): end_conversation host-tool surface + AgentConfig.conversational flag"
```

---

### Task 3: Wire `end_conversation` into the Plan-Act-Observe loop (loop.rs)

**Files:**
- Modify: `crates/greentic-aw-runtime/src/loop.rs` — derive gate (~:176), prompt augment (~:214, after the `kn_active` block), schema advertise (~:290, after the `st_active` push), the `for iter` loop label (~:263), assistant-text capture + tool-call interception (~:352 and before the allow-list check ~:408), return-guard extension (~:519)
- Test: `crates/greentic-aw-runtime/tests/loop_scripted.rs` (new integration tests)

**Interfaces:**
- Consumes: `crate::conversation::{conversational_active, end_conversation_tool_schema, augment_system_prompt, END_CONVERSATION_TOOL}` (Task 2); `TerminationReason::ConversationEnded` (Task 1); `ChatMessage::Tool`, `AgentStep::ToolCall` (existing); `ToolCallRecord.args` (`state.rs:109`).
- Produces: a `step(...)` that returns `AgentOutput { terminated_by: ConversationEnded, reply: <closing>, .. }` when a conversational agent calls `end_conversation`.

- [ ] **Step 1: Write the failing integration tests** in `tests/loop_scripted.rs`. Reuse the existing helpers (`tool_call(call_id, ext, tool)` builds an `LlmResponse` with a tool call; `final_reply(text)`; the builder that takes `llm_script` + `MockLlmBackend::new`; `MockConfigProvider` supplies the `AgentConfig`). The conversational agent's config must set `conversational: true`.

```rust
#[tokio::test]
async fn end_conversation_tool_terminates_with_conversation_ended() {
    // Agent's single turn: assistant text (the goodbye) + an end_conversation call.
    let closing = LlmResponse {
        content: Some("Thanks, all done! Goodbye.".into()),
        tool_calls: vec![ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "host".into(),
            tool_name: "end_conversation".into(),
            args: serde_json::json!({}),
        }],
        tokens_in: 0,
        tokens_out: 0,
    };
    // Build runtime with a conversational AgentConfig (conversational: true).
    // Mirror the existing scripted-test setup fn; insert the config via
    // MockConfigProvider with `conversational: true`.
    let (rt, _llm, tc) = /* setup helper, conversational = true */ ;
    let out = rt.step(tc, "s", "a", AgentInput { text: "hi".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::ConversationEnded);
    assert_eq!(out.reply, "Thanks, all done! Goodbye.");
}

#[tokio::test]
async fn end_conversation_uses_note_when_no_assistant_text() {
    let closing = LlmResponse {
        content: None,
        tool_calls: vec![ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "host".into(),
            tool_name: "end_conversation".into(),
            args: serde_json::json!({ "note": "Take care!" }),
        }],
        tokens_in: 0,
        tokens_out: 0,
    };
    let (rt, _llm, tc) = /* setup, conversational = true, script = [Ok(closing)] */ ;
    let out = rt.step(tc, "s", "a", AgentInput { text: "hi".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::ConversationEnded);
    assert_eq!(out.reply, "Take care!");
}

#[tokio::test]
async fn non_conversational_agent_ignores_end_conversation_variant() {
    // Regression: a normal (conversational: false) agent producing a plain
    // final reply still terminates FinalReply — unchanged behaviour.
    let (rt, _llm, tc) = /* setup, conversational = false, script = [Ok(final_reply("hello"))] */ ;
    let out = rt.step(tc, "s", "a", AgentInput { text: "hi".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(out.reply, "hello");
}
```

Copy the exact runtime-construction boilerplate from the nearest existing test in `tests/loop_scripted.rs` (e.g. the `remember`/`recall` scripted test around `:483-523`), changing only: the inserted `AgentConfig`'s `conversational` flag, and the `llm_script`. Add `use greentic_aw_runtime::error::TerminationReason;` and `use greentic_aw_runtime::state::ToolCallRecord;` if not already imported.

- [ ] **Step 2: Run — expect FAIL.**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 end_conversation`
Expected: FAIL — the agent currently treats `end_conversation` as a disallowed tool (`ToolCallBlocked`) and never terminates `ConversationEnded`.

- [ ] **Step 3: Implement the gate + advertise + prompt augment.** In `loop.rs`:

After `let st_active = ...` (~:176) add:

```rust
    // Whether this node is a conversational segment (opt-in). Drives the
    // `end_conversation` tool + `ConversationEnded` termination.
    let conv_active = crate::conversation::conversational_active(&config);
```

After the `kn_active` system-prompt block (~:214), append:

```rust
    let system_prompt = if conv_active {
        crate::conversation::augment_system_prompt(&system_prompt)
    } else {
        system_prompt
    };
```

After the `st_active` schema pushes (~:287-290) add:

```rust
        if conv_active {
            tools_schema.push(crate::conversation::end_conversation_tool_schema());
        }
```

- [ ] **Step 4: Implement the interception + termination.** Label the loop and capture assistant text:

Change `for iter in 0..config.limits.max_iter {` (~:263) to:

```rust
    'turns: for iter in 0..config.limits.max_iter {
```

At the assistant-message push (~:352), bind the content first:

```rust
            let assistant_text = response.content.clone().unwrap_or_default();
            state.messages.push(ChatMessage::Assistant {
                content: assistant_text.clone(),
                tool_calls: response.tool_calls.clone(),
            });
```

Then, inside `for call in response.tool_calls`, immediately AFTER the short-term `RECALL_TOOL` interception block and BEFORE the `if !is_tool_allowed(&call, &config.tools)` check (~:408), insert:

```rust
                // --- Host built-in: `end_conversation` (conversational exit) ---
                // Intercepted before the allow-list + WASM dispatch. Terminates
                // the segment: the same-turn assistant text (or the tool's `note`)
                // becomes the closing reply; the flow advances (SP2).
                if conv_active && call.tool_name == crate::conversation::END_CONVERSATION_TOOL {
                    observer.on_tool_call(&call.tool_name, &call.call_id);
                    let note = call
                        .args
                        .get("note")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let ack = serde_json::json!({ "status": "conversation_ended" });
                    observer.on_tool_result(&call.tool_name, &call.call_id, &ack);
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: ack.clone(),
                    });
                    trail.push(AgentStep::ToolCall {
                        name: call.tool_name.clone(),
                        call_id: call.call_id.clone(),
                        result: ack,
                    });
                    reply = if assistant_text.is_empty() {
                        note
                    } else {
                        assistant_text.clone()
                    };
                    terminated_by = TerminationReason::ConversationEnded;
                    break 'turns;
                }
```

- [ ] **Step 5: Extend the return guard.** Change the FinalReply gate (~:519) so the closing reply renders through the same outbound-guardrail + save + telemetry + return path:

```rust
    if matches!(
        terminated_by,
        TerminationReason::FinalReply | TerminationReason::ConversationEnded
    ) {
```

(Everything inside that block is unchanged — `reply` is already set; the outbound guardrail, `state.messages` Assistant push, `AgentStep::Reply`, `truncate_history`, `state_store.save`, `telemetry.record_step`, and the `Ok(AgentOutput { reply, trail, terminated_by })` return all apply to `ConversationEnded` too.)

- [ ] **Step 6: Run — expect PASS.**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2 end_conversation`
Expected: PASS (3 new tests).

- [ ] **Step 7: Run the crate's full test set — expect PASS (no regressions).**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2`
Expected: PASS.

- [ ] **Step 8: Commit.**

```bash
git add crates/greentic-aw-runtime/src/loop.rs crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "feat(aw-runtime): end_conversation tool ends the loop with ConversationEnded"
```

---

### Task 4: Gate + PR

**Files:** none (verification + integration only).

- [ ] **Step 1: Format.**

Run: `cargo fmt --all`
Then: `git diff --stat` (expect only touched files reformatted); commit if fmt changed anything (`style(aw-runtime): rustfmt`).

- [ ] **Step 2: Clippy (FOREGROUND, scoped, deny warnings).**

Run: `CARGO_BUILD_JOBS=2 cargo clippy -p greentic-aw-runtime -j2 --all-targets -- -D warnings`
Expected: clean. (New `match`/arm additions must be exhaustive; `conversational` field must not trigger `dead_code`.)

- [ ] **Step 3: Host-crate build sanity** (the `AgentConfig` field fan-out crosses into `greentic-runner-host`):

Run: `CARGO_BUILD_JOBS=2 cargo build -p greentic-runner-host -j2`
Expected: compiles (all `AgentConfig` literals in the host crate carry `conversational: false`).

- [ ] **Step 4: Full crate tests, once more.**

Run: `CARGO_BUILD_JOBS=2 cargo test -p greentic-aw-runtime -j2`
Expected: PASS.

- [ ] **Step 5: PR.** Use superpowers:finishing-a-development-branch. PR `feat/conversational-agent-sp1` → `research`. Body: SP1 of the in-flow conversational chat-segment epic (design `docs/superpowers/specs/2026-07-07-conversational-agent-chat-segment-epic-design.md`). Additive: new `TerminationReason::ConversationEnded`, `end_conversation` host tool, `AgentConfig.conversational` (default false). Non-conversational agents byte-unchanged. Next: SP2 (runner engine park-loop keyed on `terminated_by`). Note the greentic-dw-providers branch-pin risk (spec §Risks) — confirm the runner rev's locked `greentic-dw-providers` is greentic-types-compatible before SP4 productionization. NO Claude co-author trailer.

---

## Self-Review

- **Spec coverage (epic §SP1 lines 43-47):** "Add `TerminationReason::ConversationEnded` (serde `conversation_ended`)" → Task 1. "Register a built-in `end_conversation` tool ... available when the node is conversational ... takes an optional `reason`/closing note" → Task 2 (`end_conversation_tool_schema`, `note` param) + Task 3 (advertise gated on `conv_active`). "when the model calls `end_conversation`, stop the loop with `terminated_by = ConversationEnded` and use the agent's accompanying message (or the tool's closing note) as the final `reply`" → Task 3 Step 4 (interception, `assistant_text` else `note`, `break 'turns`). "Output contract unchanged; only a new `terminated_by` value" → Task 3 Step 5 (reuses the existing return path). Spec §"Tool discoverability" (system-prompt note) → Task 2 `augment_system_prompt` + Task 3 Step 3. Spec §"Backward compatibility" (offered only to conversational nodes; defaults false) → `conv_active` gating + `#[serde(default)]`. §Testing SP1 (tool call ⇒ ConversationEnded + closing; normal ⇒ FinalReply) → Task 3 Steps 1-2 (three tests incl. the regression).
- **Out of scope (correctly deferred):** the park-loop node behaviour (SP2), the flow-doc `conversational` flag + IR threading (SP3), the designer toggle/demo (SP4), and the optional `max_turns` safety cap (spec §Risks — a hardening detail, explicitly not SP1). Who *sets* `AgentConfig.conversational = true` is SP2/SP3; SP1 only consumes it.
- **Placeholder scan:** the only intentionally-parameterized spots are the Task 3 test setup (`/* setup helper, conversational = true */`) — deliberate, because the runtime-construction boilerplate must be copied verbatim from the nearest existing `tests/loop_scripted.rs` test (Step 1 says exactly which one and what to change). All product code is shown in full.
- **Type consistency:** `TerminationReason::ConversationEnded` (Task 1) used identically in Task 3. `conversational_active`/`end_conversation_tool_schema`/`augment_system_prompt`/`END_CONVERSATION_TOOL` (Task 2 signatures) called with matching signatures in Task 3. `ToolCallRecord.args` (verified `state.rs:109`) read in Task 3, not `arguments`. `AgentConfig.conversational: bool` defined in Task 2, read via `conversational_active` in Task 3.
- **Scope:** one crate (product), one downstream compile-fix crate (`greentic-runner-host`), one branch; every task ends with a passing, independently-reviewable test cycle.
