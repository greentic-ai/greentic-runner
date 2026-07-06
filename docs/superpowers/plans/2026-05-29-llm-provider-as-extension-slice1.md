# LLM Provider as Extension — Slice 1 (`ExtensionLlmBackend`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `ExtensionLlmBackend` to `greentic-aw-runtime` — an `LlmBackend` impl that runs the worker LLM through an installed extension (approach B: an extension tool named `complete` carrying the LLM JSON), so the runtime needs no provider-specific code.

**Architecture:** A new `LlmBackend` impl dispatches via an injectable `LlmExtensionInvoker` seam (prod = `ExtensionRuntime::invoke_tool` wrapped in `spawn_blocking`; tests = a scripted invoker, since `invoke_tool` needs a real loaded WASM component). The backend carries the host-resolved credential and serialises a `BridgeRequest{system_prompt, history, tools, credential}` to the extension, parsing back the existing `LlmResponse`. Standalone in aw-runtime — backend *selection* at call sites is Slice 3.

**Tech Stack:** Rust (greentic-aw-runtime), serde/serde_json, tokio.

**Spec:** `docs/superpowers/specs/2026-05-29-llm-provider-as-extension-design.md`

**Working dir:** `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/.worktrees/feat-llm-extension-arch` (branch `feat/llm-extension-arch`, off `research`). `<WT>` = that path. cargo via `cargo --manifest-path <WT>/Cargo.toml …` (the crate is `crates/greentic-aw-runtime`, package `greentic-aw-runtime`). git via `git -C <WT> …`. Do NOT `cd` the worktree top.

---

## Conventions

- TDD. Conventional commits, NO AI attribution, NEVER skip hooks.
- Gates: `cargo test -p greentic-aw-runtime`, `cargo clippy -p greentic-aw-runtime --all-targets -- -D warnings`, `cargo fmt -p greentic-aw-runtime -- --check` (all `--manifest-path <WT>/Cargo.toml`).

## Reference types (already in the crate — import, don't redefine)
- `crate::llm::{LlmRequest, LlmResponse, LlmToolSchema, LlmBackend}` (`src/llm.rs`) — all serde; `LlmRequest{system_prompt, history: Vec<ChatMessage>, tools: Vec<LlmToolSchema>, provider: LlmProviderRef}`; `LlmResponse{content: Option<String>, tool_calls: Vec<ToolCallRecord>, tokens_in, tokens_out}`.
- `crate::state::{ChatMessage, ToolCallRecord}` (serde).
- `crate::error::LlmError{ServiceUnavailable, BadRequest(String), Transport(String), Decode(String)}`.
- `greentic_ext_runtime::ExtensionRuntime` — `invoke_tool(ext_id, tool_name, args_json) -> Result<String, RuntimeError>` (sync; MUST be wrapped in `spawn_blocking`, per `src/tools.rs`).

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/greentic-aw-runtime/src/llm_extension.rs` | `ExtensionLlmBackend` + invoker seam + wire types | **Create** |
| `crates/greentic-aw-runtime/src/lib.rs` | module decl + re-exports | Modify |

---

### Task 1: `ExtensionLlmBackend` + invoker seam

**Files:**
- Create: `crates/greentic-aw-runtime/src/llm_extension.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs`

- [ ] **Step 1: Write the module with failing tests**

Create `crates/greentic-aw-runtime/src/llm_extension.rs`:

```rust
//! `ExtensionLlmBackend` — runs the worker LLM through an installed extension
//! instead of a hardcoded provider client (spec: LLM provider as an extension).
//!
//! Approach B: the LLM-bridge extension exposes a tool named `complete` whose
//! args JSON is a [`BridgeRequest`] (system prompt + history + tool schemas +
//! the host-resolved credential) and whose result JSON is the existing
//! [`LlmResponse`]. Dispatch goes through the generic
//! `ExtensionRuntime::invoke_tool` (sync → `spawn_blocking`), abstracted behind
//! [`LlmExtensionInvoker`] so the backend is unit-testable without a real WASM
//! component (`invoke_tool` requires a loaded component; `for_test()` has none).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, LlmToolSchema};
use crate::state::ChatMessage;

/// The tool an LLM-bridge extension exposes (approach B).
pub const LLM_COMPLETE_TOOL: &str = "complete";

/// Host-resolved credential passed to the bridge per call (spec Decision 1:
/// the host resolves creds and passes them in the request; the extension is a
/// stateless bridge). `secret_ref` is reserved for a future hardening path.
#[derive(Clone, Debug, Serialize)]
pub struct BridgeCredential {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The wire payload sent to the bridge's `complete` tool.
#[derive(Debug, Serialize)]
struct BridgeRequest<'a> {
    system_prompt: &'a str,
    history: &'a [ChatMessage],
    tools: &'a [LlmToolSchema],
    credential: &'a BridgeCredential,
}

/// Synchronous dispatch seam. Prod impl calls `ExtensionRuntime::invoke_tool`;
/// tests script the JSON response without a WASM component.
pub trait LlmExtensionInvoker: Send + Sync {
    fn invoke(&self, extension_id: &str, tool: &str, args_json: &str) -> Result<String, String>;
}

/// Production invoker over a loaded `ExtensionRuntime`.
pub struct RuntimeInvoker {
    pub ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
}

impl LlmExtensionInvoker for RuntimeInvoker {
    fn invoke(&self, extension_id: &str, tool: &str, args_json: &str) -> Result<String, String> {
        self.ext_runtime
            .invoke_tool(extension_id, tool, args_json)
            .map_err(|e| e.to_string())
    }
}

/// `LlmBackend` that delegates to an LLM-bridge extension.
pub struct ExtensionLlmBackend {
    invoker: Arc<dyn LlmExtensionInvoker>,
    extension_id: String,
    credential: BridgeCredential,
}

impl ExtensionLlmBackend {
    /// Build over a real `ExtensionRuntime`.
    pub fn new(
        ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
        extension_id: impl Into<String>,
        credential: BridgeCredential,
    ) -> Self {
        Self {
            invoker: Arc::new(RuntimeInvoker { ext_runtime }),
            extension_id: extension_id.into(),
            credential,
        }
    }

    /// Build over an arbitrary invoker (tests).
    pub fn with_invoker(
        invoker: Arc<dyn LlmExtensionInvoker>,
        extension_id: impl Into<String>,
        credential: BridgeCredential,
    ) -> Self {
        Self {
            invoker,
            extension_id: extension_id.into(),
            credential,
        }
    }
}

impl LlmBackend for ExtensionLlmBackend {
    fn complete<'a>(
        &'a self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        let invoker = self.invoker.clone();
        let ext_id = self.extension_id.clone();
        let credential = self.credential.clone();
        Box::pin(async move {
            let payload = BridgeRequest {
                system_prompt: &request.system_prompt,
                history: &request.history,
                tools: &request.tools,
                credential: &credential,
            };
            let args_json = serde_json::to_string(&payload)
                .map_err(|e| LlmError::BadRequest(format!("encode bridge request: {e}")))?;
            let raw = tokio::task::spawn_blocking(move || {
                invoker.invoke(&ext_id, LLM_COMPLETE_TOOL, &args_json)
            })
            .await
            .map_err(|e| LlmError::Transport(format!("llm bridge join: {e}")))?
            // A failed dispatch (extension missing / WASM trap) is a config-class
            // error, not a transient 5xx — surface as BadRequest so the retry
            // decorator does NOT loop on it.
            .map_err(LlmError::BadRequest)?;
            serde_json::from_str::<LlmResponse>(&raw)
                .map_err(|e| LlmError::Decode(format!("decode bridge response: {e}")))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmProviderRef;
    use crate::state::ToolCallRecord;
    use std::sync::Mutex;

    fn cred() -> BridgeCredential {
        BridgeCredential {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_key: "sk-test".into(),
            base_url: None,
        }
    }

    fn req() -> LlmRequest {
        LlmRequest {
            system_prompt: "be helpful".into(),
            history: vec![ChatMessage::User { content: "hi".into() }],
            tools: vec![LlmToolSchema {
                extension_id: "http".into(),
                tool_name: "fetch".into(),
                description: "fetch".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            provider: LlmProviderRef { provider: "openai".into(), model: "gpt-4o".into() },
        }
    }

    /// Captures the args JSON it was handed and returns a scripted reply.
    struct ScriptInvoker {
        seen: Mutex<Option<(String, String, String)>>,
        reply: Result<String, String>,
    }
    impl LlmExtensionInvoker for ScriptInvoker {
        fn invoke(&self, ext: &str, tool: &str, args: &str) -> Result<String, String> {
            *self.seen.lock().unwrap() = Some((ext.into(), tool.into(), args.into()));
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn sends_complete_tool_with_credential_and_parses_response() {
        let reply = serde_json::json!({
            "content": "done",
            "tool_calls": [{
                "call_id": "c1", "extension_id": "http", "tool_name": "fetch",
                "args": { "url": "x" }
            }],
            "tokens_in": 3, "tokens_out": 5
        })
        .to_string();
        let inv = Arc::new(ScriptInvoker { seen: Mutex::new(None), reply: Ok(reply) });
        let backend = ExtensionLlmBackend::with_invoker(inv.clone(), "llm-openai-bridge", cred());

        let resp = backend.complete(req()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("done"));
        assert_eq!(resp.tool_calls.len(), 1);
        let tc: &ToolCallRecord = &resp.tool_calls[0];
        assert_eq!(tc.extension_id, "http");
        assert_eq!(tc.tool_name, "fetch");

        // Dispatched to the right extension + tool, and the credential + tools
        // schema rode along in the args.
        let (ext, tool, args) = inv.seen.lock().unwrap().clone().unwrap();
        assert_eq!(ext, "llm-openai-bridge");
        assert_eq!(tool, "complete");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["credential"]["api_key"], "sk-test");
        assert_eq!(v["credential"]["provider"], "openai");
        assert_eq!(v["system_prompt"], "be helpful");
        assert_eq!(v["tools"][0]["tool_name"], "fetch");
        // base_url omitted when None.
        assert!(v["credential"].get("base_url").is_none());
    }

    #[tokio::test]
    async fn invoker_error_maps_to_bad_request() {
        let inv = Arc::new(ScriptInvoker {
            seen: Mutex::new(None),
            reply: Err("NotFound(llm-openai-bridge)".into()),
        });
        let backend = ExtensionLlmBackend::with_invoker(inv, "llm-openai-bridge", cred());
        let err = backend.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn malformed_reply_maps_to_decode() {
        let inv = Arc::new(ScriptInvoker {
            seen: Mutex::new(None),
            reply: Ok("not json".into()),
        });
        let backend = ExtensionLlmBackend::with_invoker(inv, "llm-openai-bridge", cred());
        let err = backend.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::Decode(_)), "got {err:?}");
    }
}
```

- [ ] **Step 2: Register + export in `lib.rs`**

In `crates/greentic-aw-runtime/src/lib.rs`: add `mod llm_extension;` (next to the other `mod` decls) and re-export the public surface alongside the existing `pub use llm::{…}` line:

```rust
pub use llm_extension::{BridgeCredential, ExtensionLlmBackend, LlmExtensionInvoker, RuntimeInvoker};
```

Verify `crate::config::LlmProviderRef` and `crate::state::{ChatMessage, ToolCallRecord, …}` paths used by the tests/code are correct (`cargo build` confirms). If `LlmToolSchema` is re-exported only from `llm`, import `crate::llm::LlmToolSchema` (already done above).

- [ ] **Step 3: Run the tests**

Run: `cargo test --manifest-path <WT>/Cargo.toml -p greentic-aw-runtime llm_extension`
Expected: PASS (3 tests). First run before the module compiles will fail — implement, then green.

- [ ] **Step 4: Commit**

```bash
git -C <WT> add crates/greentic-aw-runtime/src/llm_extension.rs crates/greentic-aw-runtime/src/lib.rs
git -C <WT> commit -m "feat(aw-runtime): ExtensionLlmBackend (LLM provider via extension)"
```

---

### Task 2: Gates + PR

- [ ] **Step 1: Full gates**

```
cargo fmt    --manifest-path <WT>/Cargo.toml -p greentic-aw-runtime -- --check
cargo clippy --manifest-path <WT>/Cargo.toml -p greentic-aw-runtime --all-targets -- -D warnings
cargo test   --manifest-path <WT>/Cargo.toml -p greentic-aw-runtime
```
All exit 0. (If the repo's pre-commit/pre-push hooks run a wider `--workspace` build, let them; report any PRE-EXISTING unrelated failure rather than fixing it.)

- [ ] **Step 2: Push + PR**

```bash
git -C <WT> push -u origin feat/llm-extension-arch
gh pr create --base research --head feat/llm-extension-arch \
  --title "feat(aw-runtime): ExtensionLlmBackend — LLM provider via extension (Slice 1)" \
  --body "Slice 1 of docs/superpowers/specs/2026-05-29-llm-provider-as-extension-design.md. Adds ExtensionLlmBackend: an LlmBackend impl that runs the worker LLM through an installed extension (approach B — a 'complete' tool carrying the LLM JSON + host-resolved credential), dispatching via the generic ExtensionRuntime::invoke_tool behind an injectable LlmExtensionInvoker seam (unit-tested without a WASM component). No provider-specific code added to the runtime; backend selection at call sites is Slice 3; the OpenAI bridge component is Slice 2. The hardcoded OpenAiLlmBackend remains until the extension path is proven."
```
Do NOT merge (await user).

---

## Self-Review

**Spec coverage:** spec component "`ExtensionLlmBackend` … dispatching through the ExtensionRuntime it already holds" + Decision 1 (credential in request) → Task 1. Backend *selection*/factory + designer/runner wiring are Slice 3 (explicitly out of this plan); the bridge component is Slice 2.

**Placeholder scan:** the lib.rs path-verification note has a concrete fallback; no stub/TODO code.

**Type consistency:** `BridgeCredential{provider, model, api_key, base_url?}` + `BridgeRequest{system_prompt, history, tools, credential}` serialise to the exact JSON the Slice-2 bridge will consume; `ExtensionLlmBackend` returns the crate's existing `LlmResponse`; `LlmError` arms used (`BadRequest`/`Transport`/`Decode`) all exist. `LLM_COMPLETE_TOOL = "complete"` is the shared tool name the bridge (Slice 2) must export.
