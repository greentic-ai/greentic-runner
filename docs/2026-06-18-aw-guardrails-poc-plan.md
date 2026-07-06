# AW External Guardrails (AWS Bedrock) PoC — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a pluggable external-guardrail seam to the agentic worker and ship one working backend (AWS Bedrock Guardrails), covering input, model output (incl. tool-call args), and tool-result checkpoints.

**Architecture:** A `Guardrail` trait in `greentic-aw-runtime` mirrors the existing `LlmBackend` async-trait-object shape. OUTPUT is enforced by a `GuardrailingLlmBackend` decorator at the LLM seam; INPUT and tool-result are enforced in `loop.rs` (so masked content is persisted and an input-Block short-circuits the step). The AWS Bedrock backend (`ApplyGuardrail`) is feature-gated so default builds pull no AWS SDK. Default is a `NoopGuardrail` → zero impact when disabled.

**Tech Stack:** Rust, `tokio`, `thiserror`, `serde_json`, `tracing`; `aws-config` + `aws-sdk-bedrockruntime` (feature `guardrail-bedrock` only).

**Spec:** `docs/2026-06-18-aw-guardrails-context-window-design.md` (this plan implements §4; §5 context-window is NOT in scope).

## Global Constraints

- **Rust 1.94.0, edition 2024** — pinned via `rust-toolchain.toml`; do not edit.
- **Clippy clean:** `cargo clippy --all-targets --all-features -- -D warnings` must pass.
- **No `unwrap()` / `panic!()` / `expect()` in non-test code.** Tests opt in with `#[allow(clippy::unwrap_used, clippy::expect_used)]` at the `mod tests` level (match existing files).
- **Errors:** `thiserror` for domain errors; never leak internal detail into user-facing replies.
- **English only** in source, comments, tests, commit messages.
- **Conventional Commits** (`feat:`, `test:`, `chore:`); **no Claude co-author attribution** (repo rule).
- **Crates touched:** `greentic-aw-runtime` (trait, decorator, loop, Bedrock backend) and `greentic-runner-host` (env wiring, feature passthrough).
- **Run tests with the mock feature where loop code is involved:** `cargo test -p greentic-aw-runtime --features test-mock`.

---

## File Structure

- **Create** `crates/greentic-aw-runtime/src/guardrail.rs` — trait, value types, `NoopGuardrail`, pure helpers (`resolve_action`, `serialize_output_for_scan`, `map_apply_guardrail`, `guard_incoming`), the `GuardrailingLlmBackend` OUTPUT decorator, and `GuardrailRuntimeConfig`. One responsibility: the guardrail seam.
- **Create** `crates/greentic-aw-runtime/src/guardrail_bedrock.rs` — `#[cfg(feature = "guardrail-bedrock")]` `AwsBedrockGuardrail` (SDK glue only; mapping logic stays in `guardrail.rs`).
- **Modify** `crates/greentic-aw-runtime/src/lib.rs` — `pub mod guardrail;` + re-exports; `guardrail` field + `with_guardrail` builder on `AgentRuntime`.
- **Modify** `crates/greentic-aw-runtime/src/loop.rs` — INPUT checkpoint (after `:57-59`) and tool-result checkpoint (after `:284`).
- **Modify** `crates/greentic-aw-runtime/Cargo.toml` — optional AWS deps + `guardrail-bedrock` feature.
- **Modify** `crates/greentic-runner-host/src/runner/agent_node.rs` — env→choice helper, build guardrail, wrap backend (OUTPUT) + `with_guardrail` (INPUT/tool-result).
- **Modify** `crates/greentic-runner-host/Cargo.toml` — `guardrail-bedrock` feature passthrough.

---

### Task 1: Core guardrail types, trait, `NoopGuardrail`, `resolve_action`

**Files:**
- Create: `crates/greentic-aw-runtime/src/guardrail.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (add `pub mod guardrail;` near the other `pub mod` lines ~16-40, and a re-export line near `:62`)

**Interfaces:**
- Produces: `GuardrailStage { Input, Output }`; `GuardrailAction { Allow, Block { message: String }, Mask { text: String } }`; `GuardrailVerdict { action, assessments }` + `GuardrailVerdict::allow()`; `GuardrailError`; `trait Guardrail` with `check(&self, GuardrailStage, &str) -> Pin<Box<dyn Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send>>`; `NoopGuardrail`; `resolve_action(Result<GuardrailVerdict, GuardrailError>, fail_closed: bool, block_message: &str) -> GuardrailAction`.

- [ ] **Step 1: Write the failing tests**

Create `crates/greentic-aw-runtime/src/guardrail.rs` with only the test module first:

```rust
//! External-guardrail seam for the agentic worker.
//!
//! A [`Guardrail`] inspects untrusted text at three checkpoints: user input
//! and tool results (both enforced in `loop.rs`, where masked content can be
//! persisted), and model output (the [`GuardrailingLlmBackend`] decorator).
//! The trait mirrors [`crate::llm::LlmBackend`]'s async-trait-object shape so a
//! future WASM `guardrail` extension can plug in as a second implementation,
//! exactly like `ExtensionLlmBackend`.

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_allows_both_stages() {
        let g = NoopGuardrail;
        for stage in [GuardrailStage::Input, GuardrailStage::Output] {
            let v = g.check(stage, "anything").await.unwrap();
            assert_eq!(v.action, GuardrailAction::Allow);
        }
    }

    #[test]
    fn resolve_passes_ok_action_through() {
        let v = GuardrailVerdict {
            action: GuardrailAction::Block { message: "no".into() },
            assessments: serde_json::Value::Null,
        };
        assert_eq!(
            resolve_action(Ok(v), false, "fallback"),
            GuardrailAction::Block { message: "no".into() }
        );
    }

    #[test]
    fn resolve_fail_closed_blocks_on_error() {
        let action = resolve_action(Err(GuardrailError::Backend("boom".into())), true, "fallback");
        assert_eq!(action, GuardrailAction::Block { message: "fallback".into() });
    }

    #[test]
    fn resolve_fail_open_allows_on_error() {
        let action = resolve_action(Err(GuardrailError::Backend("boom".into())), false, "fallback");
        assert_eq!(action, GuardrailAction::Allow);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: FAIL — `cannot find type NoopGuardrail` / `resolve_action` not found.

- [ ] **Step 3: Write the implementation**

Prepend above the test module in `guardrail.rs`:

```rust
use std::future::Future;
use std::pin::Pin;

use serde::Serialize;
use thiserror::Error;

/// Which checkpoint a guardrail call covers. Maps to the Bedrock
/// `ApplyGuardrail` `source` field (`Input` / `Output`). Tool results use
/// `Input`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardrailStage {
    Input,
    Output,
}

/// What the guardrail decided should happen to the inspected text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardrailAction {
    /// Content is fine; pass through unchanged.
    Allow,
    /// Block: replace with `message` (denied-topic / content-filter / word).
    Block { message: String },
    /// Mask: continue with `text` (sensitive-information redaction).
    Mask { text: String },
}

/// A guardrail verdict plus a compact, telemetry-friendly assessment blob.
#[derive(Clone, Debug)]
pub struct GuardrailVerdict {
    pub action: GuardrailAction,
    pub assessments: serde_json::Value,
}

impl GuardrailVerdict {
    /// An `Allow` verdict with empty assessments.
    pub fn allow() -> Self {
        Self { action: GuardrailAction::Allow, assessments: serde_json::Value::Null }
    }
}

#[derive(Debug, Error)]
pub enum GuardrailError {
    #[error("guardrail backend error: {0}")]
    Backend(String),
    #[error("guardrail misconfigured: {0}")]
    Config(String),
}

/// Inspects untrusted text and returns a verdict. Mirrors `LlmBackend`'s
/// async-trait-object signature (no `async_trait` dependency).
pub trait Guardrail: Send + Sync {
    fn check<'a>(
        &'a self,
        stage: GuardrailStage,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>>;
}

/// Default guardrail: allows everything. Used when no external guardrail is
/// configured, so the feature is zero-impact when disabled.
pub struct NoopGuardrail;

impl Guardrail for NoopGuardrail {
    fn check<'a>(
        &'a self,
        _stage: GuardrailStage,
        _text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>> {
        Box::pin(async { Ok(GuardrailVerdict::allow()) })
    }
}

/// Resolve a guardrail result into an action, applying the fail mode on error.
/// `fail_closed` → an errored check Blocks with `block_message`; otherwise it
/// Allows. Pure, so the security-sensitive fail behavior is unit-tested.
pub fn resolve_action(
    result: Result<GuardrailVerdict, GuardrailError>,
    fail_closed: bool,
    block_message: &str,
) -> GuardrailAction {
    match result {
        Ok(verdict) => verdict.action,
        Err(err) => {
            tracing::warn!(error = %err, fail_closed, "guardrail check failed");
            if fail_closed {
                GuardrailAction::Block { message: block_message.to_string() }
            } else {
                GuardrailAction::Allow
            }
        }
    }
}
```

Add the module + re-exports to `crates/greentic-aw-runtime/src/lib.rs`: a `pub mod guardrail;` line alongside the other `pub mod` declarations, and:

```rust
pub use guardrail::{
    Guardrail, GuardrailAction, GuardrailError, GuardrailStage, GuardrailVerdict, NoopGuardrail,
};
```

(The `Serialize` import is used by later tasks in this file; if Task 1 builds with an unused-import warning, drop `Serialize` here and re-add it in Task 2.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw): add Guardrail trait, NoopGuardrail, and fail-mode resolver"
```

---

### Task 2: Pure helpers — `serialize_output_for_scan`, `map_apply_guardrail`

**Files:**
- Modify: `crates/greentic-aw-runtime/src/guardrail.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (extend re-export)

**Interfaces:**
- Consumes: `crate::llm::LlmResponse`, `crate::state::ToolCallRecord` (already in the crate).
- Produces: `PiiMode { Mask, Block }`; `serialize_output_for_scan(&LlmResponse) -> String`; `map_apply_guardrail(intervened: bool, only_pii_anonymized: bool, output_text: Option<String>, pii_mode: PiiMode, block_fallback: &str, assessments: serde_json::Value) -> GuardrailVerdict`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `guardrail.rs`:

```rust
    use crate::llm::LlmResponse;
    use crate::state::ToolCallRecord;

    fn resp(content: Option<&str>, calls: Vec<ToolCallRecord>) -> LlmResponse {
        LlmResponse {
            content: content.map(str::to_string),
            tool_calls: calls,
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    #[test]
    fn serialize_includes_content_and_tool_args() {
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "component:acme".into(),
            tool_name: "send".into(),
            args: serde_json::json!({ "to": "SECRET" }),
        };
        let scanned = serialize_output_for_scan(&resp(Some("hello"), vec![call]));
        assert!(scanned.contains("hello"));
        assert!(scanned.contains("send"));
        assert!(scanned.contains("SECRET"), "tool args must be in the scanned text");
    }

    #[test]
    fn map_allow_when_not_intervened() {
        let v = map_apply_guardrail(false, false, None, PiiMode::Mask, "fb", serde_json::Value::Null);
        assert_eq!(v.action, GuardrailAction::Allow);
    }

    #[test]
    fn map_masks_pii_when_mode_mask() {
        let v = map_apply_guardrail(true, true, Some("redacted".into()), PiiMode::Mask, "fb", serde_json::Value::Null);
        assert_eq!(v.action, GuardrailAction::Mask { text: "redacted".into() });
    }

    #[test]
    fn map_blocks_pii_when_mode_block() {
        let v = map_apply_guardrail(true, true, Some("redacted".into()), PiiMode::Block, "fb", serde_json::Value::Null);
        assert_eq!(v.action, GuardrailAction::Block { message: "redacted".into() });
    }

    #[test]
    fn map_blocks_non_pii_intervention_with_fallback() {
        let v = map_apply_guardrail(true, false, None, PiiMode::Mask, "fallback msg", serde_json::Value::Null);
        assert_eq!(v.action, GuardrailAction::Block { message: "fallback msg".into() });
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: FAIL — `serialize_output_for_scan` / `map_apply_guardrail` / `PiiMode` not found.

- [ ] **Step 3: Write the implementation**

Add to `guardrail.rs` (after `resolve_action`). Ensure `use crate::llm::LlmResponse;` and `use crate::state::ToolCallRecord;` are present at the top (ToolCallRecord only needed if referenced; `serialize_output_for_scan` iterates `response.tool_calls`):

```rust
use crate::llm::LlmResponse;

/// Whether sensitive-information (PII) findings mask-and-continue or hard-block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiiMode {
    Mask,
    Block,
}

/// Serialize an [`LlmResponse`] into the text scanned by the OUTPUT checkpoint:
/// the reply content plus every tool call's name and JSON args, so a payload
/// hidden in an argument is still inspected.
pub fn serialize_output_for_scan(response: &LlmResponse) -> String {
    let mut buf = response.content.clone().unwrap_or_default();
    for call in &response.tool_calls {
        buf.push('\n');
        buf.push_str(&call.tool_name);
        buf.push(' ');
        buf.push_str(&call.args.to_string());
    }
    buf
}

/// Pure mapping from a Bedrock `ApplyGuardrail` outcome to a verdict, so it is
/// unit-testable without the AWS SDK (the SDK glue only computes its inputs).
///
/// - `intervened`: action was `GUARDRAIL_INTERVENED`.
/// - `only_pii_anonymized`: the *only* intervention was a sensitive-information
///   policy that masked content (so redacted text is available).
/// - `output_text`: `outputs[0].text` if present.
pub fn map_apply_guardrail(
    intervened: bool,
    only_pii_anonymized: bool,
    output_text: Option<String>,
    pii_mode: PiiMode,
    block_fallback: &str,
    assessments: serde_json::Value,
) -> GuardrailVerdict {
    if !intervened {
        return GuardrailVerdict { action: GuardrailAction::Allow, assessments };
    }
    if only_pii_anonymized && pii_mode == PiiMode::Mask {
        if let Some(text) = output_text {
            return GuardrailVerdict { action: GuardrailAction::Mask { text }, assessments };
        }
    }
    let message = output_text.unwrap_or_else(|| block_fallback.to_string());
    GuardrailVerdict { action: GuardrailAction::Block { message }, assessments }
}
```

Extend the `lib.rs` re-export to add `PiiMode, map_apply_guardrail, serialize_output_for_scan`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: PASS (9 tests total).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw): add output-scan serialization and Bedrock verdict mapping"
```

---

### Task 3: `GuardrailingLlmBackend` OUTPUT decorator

**Files:**
- Modify: `crates/greentic-aw-runtime/src/guardrail.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (extend re-export)

**Interfaces:**
- Consumes: `crate::llm::{LlmBackend, LlmRequest, LlmResponse, OnDelta}`, `crate::error::LlmError`, `resolve_action`, `serialize_output_for_scan`.
- Produces: `struct GuardrailingLlmBackend` with `new(inner: Arc<dyn LlmBackend>, guardrail: Arc<dyn Guardrail>, fail_closed: bool, block_message: String) -> Self`, implementing `LlmBackend`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `guardrail.rs`:

```rust
    use std::sync::Arc;
    use crate::llm::{LlmBackend, LlmRequest, LlmResponse};
    use crate::config::LlmProviderRef;

    // A guardrail that blocks when the scanned text contains `needle`.
    struct KeywordBlock { needle: String }
    impl Guardrail for KeywordBlock {
        fn check<'a>(&'a self, _s: GuardrailStage, text: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>> {
            let hit = text.contains(&self.needle);
            Box::pin(async move {
                Ok(GuardrailVerdict {
                    action: if hit { GuardrailAction::Block { message: "blocked".into() } } else { GuardrailAction::Allow },
                    assessments: serde_json::Value::Null,
                })
            })
        }
    }
    struct AlwaysMask;
    impl Guardrail for AlwaysMask {
        fn check<'a>(&'a self, _s: GuardrailStage, _t: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>> {
            Box::pin(async { Ok(GuardrailVerdict { action: GuardrailAction::Mask { text: "MASKED".into() }, assessments: serde_json::Value::Null }) })
        }
    }
    struct AlwaysErr;
    impl Guardrail for AlwaysErr {
        fn check<'a>(&'a self, _s: GuardrailStage, _t: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>> {
            Box::pin(async { Err(GuardrailError::Backend("down".into())) })
        }
    }
    // An inner backend returning a fixed response.
    struct Fixed { resp: LlmResponse }
    impl LlmBackend for Fixed {
        fn complete<'a>(&'a self, _r: LlmRequest)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<LlmResponse, crate::error::LlmError>> + Send + 'a>> {
            let r = self.resp.clone();
            Box::pin(async move { Ok(r) })
        }
    }
    fn req() -> LlmRequest {
        LlmRequest { system_prompt: String::new(), history: vec![], tools: vec![],
            provider: LlmProviderRef { provider: "openai".into(), model: "m".into(), credential_ref: None } }
    }

    #[tokio::test]
    async fn output_block_replaces_content_and_drops_tool_calls() {
        let call = ToolCallRecord { call_id: "c".into(), extension_id: "x".into(), tool_name: "t".into(), args: serde_json::json!({}) };
        let inner = Arc::new(Fixed { resp: resp(Some("here is the BADWORD"), vec![call]) });
        let g = GuardrailingLlmBackend::new(inner, Arc::new(KeywordBlock { needle: "BADWORD".into() }), false, "safe".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("blocked"));
        assert!(out.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn output_block_detects_keyword_in_tool_args_only() {
        let call = ToolCallRecord { call_id: "c".into(), extension_id: "x".into(), tool_name: "send".into(), args: serde_json::json!({ "body": "BADWORD" }) };
        let inner = Arc::new(Fixed { resp: resp(Some("ok"), vec![call]) }); // clean content
        let g = GuardrailingLlmBackend::new(inner, Arc::new(KeywordBlock { needle: "BADWORD".into() }), false, "safe".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("blocked"));
        assert!(out.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn output_mask_replaces_content() {
        let inner = Arc::new(Fixed { resp: resp(Some("my ssn is 123"), vec![]) });
        let g = GuardrailingLlmBackend::new(inner, Arc::new(AlwaysMask), false, "safe".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("MASKED"));
    }

    #[tokio::test]
    async fn output_allow_passes_through() {
        let inner = Arc::new(Fixed { resp: resp(Some("totally fine"), vec![]) });
        let g = GuardrailingLlmBackend::new(inner, Arc::new(NoopGuardrail), false, "safe".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("totally fine"));
    }

    #[tokio::test]
    async fn fail_closed_blocks_on_guardrail_error() {
        let inner = Arc::new(Fixed { resp: resp(Some("fine"), vec![]) });
        let g = GuardrailingLlmBackend::new(inner, Arc::new(AlwaysErr), true, "safe-fallback".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("safe-fallback"));
    }

    #[tokio::test]
    async fn fail_open_passes_through_on_guardrail_error() {
        let inner = Arc::new(Fixed { resp: resp(Some("fine"), vec![]) });
        let g = GuardrailingLlmBackend::new(inner, Arc::new(AlwaysErr), false, "safe-fallback".into());
        let out = g.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("fine"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: FAIL — `GuardrailingLlmBackend` not found.

- [ ] **Step 3: Write the implementation**

Add to `guardrail.rs`. Extend the top-of-file imports with `use std::sync::Arc;`, `use crate::error::LlmError;`, `use crate::llm::{LlmBackend, LlmRequest, OnDelta};`:

```rust
/// OUTPUT-stage decorator: wraps any [`LlmBackend`] and runs the model reply
/// (content + tool-call args) through a [`Guardrail`] before returning it.
/// Compose as `GuardrailingLlmBackend( RetryingLlmBackend( <backend> ) )` so it
/// judges the final text after retries settle. INPUT and tool-result
/// checkpoints are enforced in `loop.rs`, not here.
pub struct GuardrailingLlmBackend {
    inner: Arc<dyn LlmBackend>,
    guardrail: Arc<dyn Guardrail>,
    fail_closed: bool,
    block_message: String,
}

impl GuardrailingLlmBackend {
    pub fn new(
        inner: Arc<dyn LlmBackend>,
        guardrail: Arc<dyn Guardrail>,
        fail_closed: bool,
        block_message: String,
    ) -> Self {
        Self { inner, guardrail, fail_closed, block_message }
    }

    /// Apply the OUTPUT verdict to a completed response.
    async fn guard_output(&self, response: LlmResponse) -> LlmResponse {
        let scanned = serialize_output_for_scan(&response);
        let action = resolve_action(
            self.guardrail.check(GuardrailStage::Output, &scanned).await,
            self.fail_closed,
            &self.block_message,
        );
        match action {
            GuardrailAction::Allow => response,
            GuardrailAction::Block { message } => LlmResponse {
                content: Some(message),
                tool_calls: vec![],
                tokens_in: response.tokens_in,
                tokens_out: response.tokens_out,
            },
            GuardrailAction::Mask { text } => LlmResponse { content: Some(text), ..response },
        }
    }
}

impl LlmBackend for GuardrailingLlmBackend {
    fn complete<'a>(
        &'a self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let response = self.inner.complete(request).await?;
            Ok(self.guard_output(response).await)
        })
    }

    fn complete_streaming<'a>(
        &'a self,
        request: LlmRequest,
        on_delta: OnDelta,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            // Stream-then-redact (PoC default): deltas already reached the
            // consumer; the verdict applies to the accumulated reply.
            let response = self.inner.complete_streaming(request, on_delta).await?;
            Ok(self.guard_output(response).await)
        })
    }
}
```

Extend the `lib.rs` re-export to add `GuardrailingLlmBackend`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime guardrail`
Expected: PASS (15 tests total).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw): add GuardrailingLlmBackend output decorator with fail-mode"
```

---

### Task 4: `guard_incoming` helper + `loop.rs` INPUT and tool-result checkpoints

**Files:**
- Modify: `crates/greentic-aw-runtime/src/guardrail.rs` (add `IncomingDecision` + `guard_incoming` + `GuardrailRuntimeConfig`)
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (`guardrail` field + `with_guardrail` builder; extend re-export)
- Modify: `crates/greentic-aw-runtime/src/loop.rs` (two insertion points)

**Interfaces:**
- Produces: `enum IncomingDecision { Allow, Block { message: String }, Mask { text: String } }`; `async fn guard_incoming(&dyn Guardrail, GuardrailStage, &str, fail_closed: bool, block_fallback: &str) -> IncomingDecision`; `struct GuardrailRuntimeConfig { guardrail: Arc<dyn Guardrail>, fail_closed_ingress: bool, block_message: String, tool_block_placeholder: String }`; `AgentRuntime::with_guardrail(self, GuardrailRuntimeConfig) -> Self` and a public `guardrail: Option<GuardrailRuntimeConfig>` field.
- Consumes: `crate::state::ChatMessage`.

- [ ] **Step 1: Write the failing test for the helper**

Add to the `tests` module in `guardrail.rs`:

```rust
    #[tokio::test]
    async fn guard_incoming_maps_actions() {
        let block = KeywordBlock { needle: "PII".into() };
        match guard_incoming(&block, GuardrailStage::Input, "contains PII here", false, "fb").await {
            IncomingDecision::Block { message } => assert_eq!(message, "blocked"),
            other => panic!("expected Block, got {other:?}"),
        }
        match guard_incoming(&block, GuardrailStage::Input, "clean text", false, "fb").await {
            IncomingDecision::Allow => {}
            other => panic!("expected Allow, got {other:?}"),
        }
        match guard_incoming(&AlwaysMask, GuardrailStage::Input, "x", false, "fb").await {
            IncomingDecision::Mask { text } => assert_eq!(text, "MASKED"),
            other => panic!("expected Mask, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p greentic-aw-runtime guardrail::tests::guard_incoming_maps_actions`
Expected: FAIL — `IncomingDecision` / `guard_incoming` not found.

- [ ] **Step 3: Implement the helper + runtime config**

Add to `guardrail.rs`:

```rust
/// Decision for untrusted text about to enter conversation history. Returned by
/// the `loop.rs` INPUT and tool-result checkpoints, which apply it
/// stage-specifically (input Block short-circuits the step; tool Block swaps in
/// a withheld-result placeholder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IncomingDecision {
    Allow,
    Block { message: String },
    Mask { text: String },
}

/// Guard untrusted text destined for `state.messages`.
pub async fn guard_incoming(
    guardrail: &dyn Guardrail,
    stage: GuardrailStage,
    text: &str,
    fail_closed: bool,
    block_fallback: &str,
) -> IncomingDecision {
    match resolve_action(guardrail.check(stage, text).await, fail_closed, block_fallback) {
        GuardrailAction::Allow => IncomingDecision::Allow,
        GuardrailAction::Block { message } => IncomingDecision::Block { message },
        GuardrailAction::Mask { text } => IncomingDecision::Mask { text },
    }
}

/// Guardrail configuration carried on `AgentRuntime` for the INPUT and
/// tool-result checkpoints (the OUTPUT checkpoint lives in the decorator).
#[derive(Clone)]
pub struct GuardrailRuntimeConfig {
    pub guardrail: Arc<dyn Guardrail>,
    /// Fail-closed on the ingress stages (input + tool-result) when the
    /// guardrail backend errors. Production should set this true.
    pub fail_closed_ingress: bool,
    /// Safe reply used when an input message is blocked.
    pub block_message: String,
    /// Placeholder JSON-string used when a tool result is blocked.
    pub tool_block_placeholder: String,
}
```

Extend the `lib.rs` re-export to add `GuardrailRuntimeConfig, IncomingDecision, guard_incoming`.

- [ ] **Step 4: Run helper test to verify it passes**

Run: `cargo test -p greentic-aw-runtime guardrail::tests::guard_incoming_maps_actions`
Expected: PASS.

- [ ] **Step 5: Add the `guardrail` field + builder to `AgentRuntime`**

In `crates/greentic-aw-runtime/src/lib.rs`, read the `AgentRuntime` struct and the existing `with_knowledge` builder, then mirror them:
- Add field `pub guardrail: Option<crate::guardrail::GuardrailRuntimeConfig>,` to the struct.
- Initialize it to `None` in `AgentRuntime::new(...)` (and any other constructor).
- Add the builder following the `with_knowledge` shape:

```rust
/// Attach a guardrail for the INPUT and tool-result checkpoints. The OUTPUT
/// checkpoint is wired separately by wrapping the `LlmBackend` with
/// `GuardrailingLlmBackend`.
pub fn with_guardrail(mut self, cfg: crate::guardrail::GuardrailRuntimeConfig) -> Self {
    self.guardrail = Some(cfg);
    self
}
```

- [ ] **Step 6: Wire the INPUT checkpoint into `loop.rs`**

In `crates/greentic-aw-runtime/src/loop.rs`, immediately after the user message is pushed (`:57-59`, the `state.messages.push(ChatMessage::User { ... })` block) and before the long-term-recall block, insert:

```rust
    // --- INPUT guardrail (spec §4.2): scan the user message before it reaches
    // the LLM. Block short-circuits the step; Mask rewrites the persisted text
    // so PII does not re-enter context next turn. `user_message` is updated so
    // recall/ingest use the masked text too.
    let mut user_message = user_message;
    if let Some(g) = &runtime.guardrail {
        match crate::guardrail::guard_incoming(
            g.guardrail.as_ref(),
            crate::guardrail::GuardrailStage::Input,
            &user_message,
            g.fail_closed_ingress,
            &g.block_message,
        )
        .await
        {
            crate::guardrail::IncomingDecision::Allow => {}
            crate::guardrail::IncomingDecision::Mask { text } => {
                if let Some(ChatMessage::User { content }) = state.messages.last_mut() {
                    *content = text.clone();
                }
                user_message = text;
            }
            crate::guardrail::IncomingDecision::Block { message } => {
                state.messages.push(ChatMessage::Assistant {
                    content: message.clone(),
                    tool_calls: vec![],
                });
                state.truncate_history(config.limits.max_history_turns);
                if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
                    warn!(error = %e, "state save failed after input block");
                }
                runtime.telemetry.record_step(&StepTelemetryCtx {
                    tenant_id: tenant.tenant_id.clone(),
                    env_id: tenant.env_id.clone(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    terminated_by: TerminationReason::FinalReply,
                    iterations: 0,
                    total_tokens: 0,
                    duration: started.elapsed(),
                });
                return Ok(AgentOutput {
                    reply: message.clone(),
                    trail: vec![AgentStep::Reply { text: message }],
                    terminated_by: TerminationReason::FinalReply,
                });
            }
        }
    }
```

Note: the existing code binds `let user_message = message.text.clone();` at `:56`. Change that line to keep it immutable there and introduce the `let mut user_message = user_message;` shadow shown above (or change `:56` to `let mut`). Pick one; do not leave two conflicting bindings. The held `lock` guard drops on the early return, releasing the session lock.

- [ ] **Step 7: Wire the tool-result checkpoint into `loop.rs`**

In the successful-dispatch path, after the `let result = match dispatch_tool_call(...) { ... };` block ends (`:284`) and before `observer.on_tool_result(...)` (`:286`), insert a shadowing guard so the guarded value flows into telemetry, the ledger, and history:

```rust
                // --- Tool-result guardrail (spec §4.2): the external tool
                // output is the top prompt-injection / PII vector. Guard it
                // before it is observed, recorded, or appended to history.
                let result = if let Some(g) = &runtime.guardrail {
                    let text = result.to_string();
                    match crate::guardrail::guard_incoming(
                        g.guardrail.as_ref(),
                        crate::guardrail::GuardrailStage::Input,
                        &text,
                        g.fail_closed_ingress,
                        &g.tool_block_placeholder,
                    )
                    .await
                    {
                        crate::guardrail::IncomingDecision::Allow => result,
                        crate::guardrail::IncomingDecision::Block { .. } => {
                            serde_json::json!({ "error": "blocked by guardrail policy, result withheld" })
                        }
                        crate::guardrail::IncomingDecision::Mask { text } => {
                            serde_json::Value::String(text)
                        }
                    }
                } else {
                    result
                };
```

This intentionally does **not** guard the `recall_memory` built-in (`:210`), allow-list rejections (`:222`), idempotency cache hits (`:236`), or dispatch errors (`:272`) — those are internal/synthetic, not untrusted external content. Per-tool policy is a documented follow-up (spec §5).

- [ ] **Step 8: Run the loop tests to verify no regression**

Run: `cargo test -p greentic-aw-runtime --features test-mock`
Expected: PASS — all existing `r#loop` tests still green (the guardrail field defaults to `None`, so behavior is unchanged when unset).

- [ ] **Step 9: Add a loop-level guardrail test**

Add a test to the `tests` module in `loop.rs` (it has `feature = "test-mock"` gating and the `AgentRuntime::new(... None)` + builder pattern already). Use a blocking guardrail and assert the input is short-circuited:

```rust
    struct BlockAll;
    impl crate::guardrail::Guardrail for BlockAll {
        fn check<'a>(&'a self, _s: crate::guardrail::GuardrailStage, _t: &'a str)
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<crate::guardrail::GuardrailVerdict, crate::guardrail::GuardrailError>> + Send + 'a>> {
            Box::pin(async {
                Ok(crate::guardrail::GuardrailVerdict {
                    action: crate::guardrail::GuardrailAction::Block { message: "policy says no".into() },
                    assessments: serde_json::Value::Null,
                })
            })
        }
    }

    #[tokio::test]
    async fn input_block_short_circuits_step() {
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("should never be returned".into()),
            tool_calls: vec![],
            tokens_in: 1,
            tokens_out: 1,
        })]));
        let store = Arc::new(MockAgentStateStore::new());
        let telemetry = Arc::new(MockTelemetry::new());
        let cp = MockConfigProvider::new();
        let tc = TenantContext::new("acme", "prod");
        cp.insert(&tc, "a", cfg());
        let cp = Arc::new(cp);
        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let token_meter = Arc::new(crate::cost::MockTokenMeter::new(0));
        let ledger = Arc::new(crate::mock::NoopToolLedger);
        let runtime = AgentRuntime::new(cp, store, ext, llm, telemetry, token_meter, ledger, None)
            .with_guardrail(crate::guardrail::GuardrailRuntimeConfig {
                guardrail: Arc::new(BlockAll),
                fail_closed_ingress: true,
                block_message: "policy says no".into(),
                tool_block_placeholder: "withheld".into(),
            });

        let out = runtime
            .step(tc, "sess-block", "a", AgentInput { text: "leak my SSN 123-45-6789".into() })
            .await
            .unwrap();
        assert_eq!(out.reply, "policy says no");
    }
```

- [ ] **Step 10: Run the new test**

Run: `cargo test -p greentic-aw-runtime --features test-mock input_block_short_circuits_step`
Expected: PASS.

- [ ] **Step 11: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail.rs crates/greentic-aw-runtime/src/lib.rs crates/greentic-aw-runtime/src/loop.rs
git commit -m "feat(aw): enforce input and tool-result guardrails in the agent loop"
```

---

### Task 5: AWS Bedrock backend (`AwsBedrockGuardrail`, feature-gated)

**Files:**
- Create: `crates/greentic-aw-runtime/src/guardrail_bedrock.rs`
- Modify: `crates/greentic-aw-runtime/Cargo.toml` (optional deps + feature)
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (feature-gated module + re-export)

**Interfaces:**
- Consumes: `Guardrail`, `GuardrailStage`, `GuardrailVerdict`, `GuardrailError`, `PiiMode`, `map_apply_guardrail`.
- Produces: `#[cfg(feature = "guardrail-bedrock")] struct AwsBedrockGuardrail` with `new(guardrail_id: String, guardrail_version: String, pii_mode: PiiMode, block_fallback: String) -> Self`, implementing `Guardrail`.

- [ ] **Step 1: Add optional deps + feature to `Cargo.toml`**

In `crates/greentic-aw-runtime/Cargo.toml`, under `[dependencies]`:

```toml
aws-config = { version = "1", optional = true }
aws-sdk-bedrockruntime = { version = "1", optional = true }
```

Under `[features]`:

```toml
guardrail-bedrock = ["dep:aws-config", "dep:aws-sdk-bedrockruntime"]
```

- [ ] **Step 2: Create the backend module (compile gate via mapping reuse)**

Create `crates/greentic-aw-runtime/src/guardrail_bedrock.rs`. The credential/client init is deferred to first use via `tokio::sync::OnceCell`, so construction stays synchronous and `build_agent_runtime` need not be async:

```rust
//! AWS Bedrock Guardrails backend (`ApplyGuardrail`). Feature-gated behind
//! `guardrail-bedrock` so default builds pull no AWS SDK. Mapping logic lives
//! in `crate::guardrail::map_apply_guardrail` (unit-tested without AWS); this
//! file is SDK glue only, covered by an ignored integration test.

use std::pin::Pin;

use aws_sdk_bedrockruntime::types::{
    GuardrailAction as BedrockAction, GuardrailContentBlock, GuardrailContentSource,
    GuardrailTextBlock,
};
use tokio::sync::OnceCell;

use crate::guardrail::{
    map_apply_guardrail, Guardrail, GuardrailError, GuardrailStage, GuardrailVerdict, PiiMode,
};

pub struct AwsBedrockGuardrail {
    guardrail_id: String,
    guardrail_version: String,
    pii_mode: PiiMode,
    block_fallback: String,
    client: OnceCell<aws_sdk_bedrockruntime::Client>,
}

impl AwsBedrockGuardrail {
    pub fn new(
        guardrail_id: String,
        guardrail_version: String,
        pii_mode: PiiMode,
        block_fallback: String,
    ) -> Self {
        Self {
            guardrail_id,
            guardrail_version,
            pii_mode,
            block_fallback,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> &aws_sdk_bedrockruntime::Client {
        self.client
            .get_or_init(|| async {
                let cfg =
                    aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
                aws_sdk_bedrockruntime::Client::new(&cfg)
            })
            .await
    }
}

impl Guardrail for AwsBedrockGuardrail {
    fn check<'a>(
        &'a self,
        stage: GuardrailStage,
        text: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>>
    {
        Box::pin(async move {
            let source = match stage {
                GuardrailStage::Input => GuardrailContentSource::Input,
                GuardrailStage::Output => GuardrailContentSource::Output,
            };
            let text_block = GuardrailTextBlock::builder()
                .text(text)
                .build()
                .map_err(|e| GuardrailError::Config(e.to_string()))?;
            let out = self
                .client()
                .await
                .apply_guardrail()
                .guardrail_identifier(&self.guardrail_id)
                .guardrail_version(&self.guardrail_version)
                .source(source)
                .content(GuardrailContentBlock::Text(text_block))
                .send()
                .await
                .map_err(|e| GuardrailError::Backend(e.to_string()))?;

            let intervened = matches!(out.action(), Some(&BedrockAction::GuardrailIntervened));
            let output_text = out
                .outputs()
                .first()
                .and_then(|o| o.text())
                .map(str::to_string);
            // PII-only when there is at least one sensitive-information
            // assessment and no topic/content/word-policy intervention.
            let mut has_pii = false;
            let mut has_other = false;
            for a in out.assessments() {
                if a.sensitive_information_policy().is_some() {
                    has_pii = true;
                }
                if a.topic_policy().is_some()
                    || a.content_policy().is_some()
                    || a.word_policy().is_some()
                {
                    has_other = true;
                }
            }
            let only_pii_anonymized = intervened && has_pii && !has_other;
            let assessments = serde_json::json!({
                "intervened": intervened,
                "has_pii": has_pii,
                "has_other": has_other,
                "assessment_count": out.assessments().len(),
            });

            Ok(map_apply_guardrail(
                intervened,
                only_pii_anonymized,
                output_text,
                self.pii_mode,
                &self.block_fallback,
                assessments,
            ))
        })
    }
}
```

Add to `lib.rs`:

```rust
#[cfg(feature = "guardrail-bedrock")]
pub mod guardrail_bedrock;
#[cfg(feature = "guardrail-bedrock")]
pub use guardrail_bedrock::AwsBedrockGuardrail;
```

> **SDK note:** accessor names (`outputs().first().text()`, `assessments()`, the `sensitive_information_policy`/`topic_policy`/etc. getters, `GuardrailAction::GuardrailIntervened`) track `aws-sdk-bedrockruntime` 1.x. If a pinned minor version renames one, the compiler will point at it — adjust the accessor, not the logic. The verdict mapping is already locked by Task 2's unit tests.

- [ ] **Step 3: Verify the feature compiles**

Run: `cargo build -p greentic-aw-runtime --features guardrail-bedrock`
Expected: builds (first build downloads the AWS SDK). If accessor names differ, fix per the SDK note and rebuild.

- [ ] **Step 4: Verify default build is unaffected**

Run: `cargo build -p greentic-aw-runtime`
Expected: builds with no AWS SDK in the dependency tree (`cargo tree -p greentic-aw-runtime | grep -c aws-sdk` → `0`).

- [ ] **Step 5: Add an ignored integration test**

Add to `guardrail_bedrock.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // Requires real AWS creds + a provisioned guardrail. Run explicitly:
    //   GREENTIC_AW_GUARDRAIL_ID=... GREENTIC_AW_GUARDRAIL_VERSION=DRAFT \
    //   cargo test -p greentic-aw-runtime --features guardrail-bedrock -- --ignored
    #[tokio::test]
    #[ignore = "needs AWS credentials and a provisioned Bedrock guardrail"]
    async fn live_apply_guardrail_allows_benign_text() {
        let id = std::env::var("GREENTIC_AW_GUARDRAIL_ID").unwrap();
        let ver = std::env::var("GREENTIC_AW_GUARDRAIL_VERSION").unwrap_or_else(|_| "DRAFT".into());
        let g = AwsBedrockGuardrail::new(id, ver, PiiMode::Mask, "blocked".into());
        let v = g.check(GuardrailStage::Input, "Hello, how are you?").await.unwrap();
        assert_eq!(v.action, crate::guardrail::GuardrailAction::Allow);
    }
}
```

- [ ] **Step 6: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail_bedrock.rs crates/greentic-aw-runtime/src/lib.rs crates/greentic-aw-runtime/Cargo.toml crates/greentic-aw-runtime/Cargo.lock
git commit -m "feat(aw): add feature-gated AWS Bedrock Guardrails backend"
```

---

### Task 6: Runner-host wiring (env → guardrail, wrap output + set runtime)

**Files:**
- Modify: `crates/greentic-runner-host/Cargo.toml` (feature passthrough)
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs` (env helper + build + wire)

**Interfaces:**
- Consumes: `greentic_aw_runtime::{Guardrail, GuardrailingLlmBackend, GuardrailRuntimeConfig, NoopGuardrail, PiiMode, LlmBackend}`; `AgentRuntime::with_guardrail`.
- Produces: `pub(crate) enum GuardrailChoice { Disabled, Bedrock }`; `pub(crate) fn guardrail_choice(var: Option<&str>) -> GuardrailChoice`.

- [ ] **Step 1: Add the feature passthrough to `Cargo.toml`**

In `crates/greentic-runner-host/Cargo.toml`, under `[features]`:

```toml
guardrail-bedrock = ["greentic-aw-runtime/guardrail-bedrock"]
```

- [ ] **Step 2: Write the failing test for the env helper**

In `crates/greentic-runner-host/src/runner/agent_node.rs`, add to its `#[cfg(test)] mod tests` (or create one matching the file's existing test style):

```rust
    #[test]
    fn guardrail_choice_maps_env() {
        assert_eq!(guardrail_choice(Some("bedrock")), GuardrailChoice::Bedrock);
        assert_eq!(guardrail_choice(Some("  bedrock  ")), GuardrailChoice::Bedrock);
        assert_eq!(guardrail_choice(Some("nope")), GuardrailChoice::Disabled);
        assert_eq!(guardrail_choice(None), GuardrailChoice::Disabled);
    }
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p greentic-runner-host guardrail_choice_maps_env`
Expected: FAIL — `guardrail_choice` / `GuardrailChoice` not found.

- [ ] **Step 4: Implement the env helper**

Add to `agent_node.rs` (module scope, near `build_llm_backend`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GuardrailChoice {
    Disabled,
    Bedrock,
}

/// Map `GREENTIC_AW_GUARDRAIL` to a choice. Pure (no env) so it is unit-testable.
pub(crate) fn guardrail_choice(var: Option<&str>) -> GuardrailChoice {
    match var.map(str::trim) {
        Some("bedrock") => GuardrailChoice::Bedrock,
        _ => GuardrailChoice::Disabled,
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p greentic-runner-host guardrail_choice_maps_env`
Expected: PASS.

- [ ] **Step 6: Build the guardrail and wire both seams**

Add a constructor that builds the shared `Arc<dyn Guardrail>` and the `GuardrailRuntimeConfig` from env. Bedrock is only reachable when the feature is on; otherwise the choice degrades to disabled with a warning.

```rust
use std::sync::Arc;
use greentic_aw_runtime::{
    Guardrail, GuardrailRuntimeConfig, GuardrailingLlmBackend, LlmBackend, PiiMode,
};

const DEFAULT_BLOCK_MESSAGE: &str =
    "I can't help with that — it was blocked by a safety policy.";
const DEFAULT_TOOL_PLACEHOLDER: &str =
    "{\"error\":\"blocked by guardrail policy, result withheld\"}";

/// Build the configured guardrail (shared `Arc`) + its runtime config, or
/// `None` when no guardrail is configured. Reads `GREENTIC_AW_GUARDRAIL*`.
pub(crate) fn build_guardrail() -> Option<(Arc<dyn Guardrail>, GuardrailRuntimeConfig)> {
    let choice = guardrail_choice(std::env::var("GREENTIC_AW_GUARDRAIL").ok().as_deref());
    let guardrail: Arc<dyn Guardrail> = match choice {
        GuardrailChoice::Disabled => return None,
        GuardrailChoice::Bedrock => {
            #[cfg(feature = "guardrail-bedrock")]
            {
                let id = std::env::var("GREENTIC_AW_GUARDRAIL_ID").unwrap_or_default();
                let version =
                    std::env::var("GREENTIC_AW_GUARDRAIL_VERSION").unwrap_or_else(|_| "DRAFT".into());
                if id.trim().is_empty() {
                    tracing::warn!("GREENTIC_AW_GUARDRAIL=bedrock but GREENTIC_AW_GUARDRAIL_ID is empty; guardrail disabled");
                    return None;
                }
                let pii_mode = match std::env::var("GREENTIC_AW_GUARDRAIL_PII").ok().as_deref() {
                    Some("block") => PiiMode::Block,
                    _ => PiiMode::Mask,
                };
                tracing::info!(guardrail_id = %id, version = %version, "AW guardrail: AWS Bedrock");
                Arc::new(greentic_aw_runtime::AwsBedrockGuardrail::new(
                    id,
                    version,
                    pii_mode,
                    DEFAULT_BLOCK_MESSAGE.to_string(),
                ))
            }
            #[cfg(not(feature = "guardrail-bedrock"))]
            {
                tracing::warn!("GREENTIC_AW_GUARDRAIL=bedrock but built without the `guardrail-bedrock` feature; guardrail disabled");
                return None;
            }
        }
    };
    let fail_closed_ingress = matches!(
        std::env::var("GREENTIC_AW_GUARDRAIL_FAILMODE").ok().as_deref(),
        Some("closed")
    );
    let cfg = GuardrailRuntimeConfig {
        guardrail: guardrail.clone(),
        fail_closed_ingress,
        block_message: DEFAULT_BLOCK_MESSAGE.to_string(),
        tool_block_placeholder: DEFAULT_TOOL_PLACEHOLDER.to_string(),
    };
    Some((guardrail, cfg))
}
```

Then, where the `AgentRuntime` is assembled (read `build_agent_runtime` in this file — it is the shared path used by both single-agent and graph nodes), apply both seams:

```rust
    let backend = Self::build_llm_backend(&ext_runtime); // existing Arc<dyn LlmBackend>
    let (backend, guardrail_cfg) = match build_guardrail() {
        Some((guardrail, cfg)) => {
            // OUTPUT seam: wrap the backend (outside retry, which is already
            // inside `backend`).
            let wrapped: Arc<dyn LlmBackend> = Arc::new(GuardrailingLlmBackend::new(
                backend,
                guardrail,
                cfg.fail_closed_ingress, // OUTPUT shares the configured fail mode
                cfg.block_message.clone(),
            ));
            (wrapped, Some(cfg))
        }
        None => (backend, None),
    };

    // ... build AgentRuntime with `backend` as the LLM ...
    let runtime = AgentRuntime::new(/* ..., */ backend, /* ... */);
    let runtime = match guardrail_cfg {
        Some(cfg) => runtime.with_guardrail(cfg), // INPUT + tool-result seam
        None => runtime,
    };
```

Adjust the exact `AgentRuntime::new(...)` argument list and the `backend` parameter position to match the real `build_agent_runtime` signature (do not guess — read it). If `build_llm_backend` is called in more than one construction path, wrap at each, or refactor the wrap into a shared helper.

- [ ] **Step 7: Verify both build configurations**

Run: `cargo build -p greentic-runner-host`
Run: `cargo build -p greentic-runner-host --features guardrail-bedrock`
Expected: both build.

- [ ] **Step 8: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs crates/greentic-runner-host/Cargo.toml crates/greentic-runner-host/Cargo.lock
git commit -m "feat(runner-host): wire env-configured guardrail into the agent runtime"
```

---

### Task 7: Final verification (fmt, clippy, full tests, docs)

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 2: Clippy (default + feature)**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. (`--all-features` includes `guardrail-bedrock`.)

- [ ] **Step 3: Tests**

Run: `cargo test -p greentic-aw-runtime --features test-mock`
Run: `cargo test -p greentic-runner-host`
Expected: PASS. The ignored Bedrock integration test stays ignored.

- [ ] **Step 4: Local CI mirror (best-effort)**

Run: `bash ci/local_check.sh`
Expected: pass; if a failure is outside this change's scope, document it in the PR summary rather than hiding it (repo convention).

- [ ] **Step 5: Update repo docs in the same change**

Add the new env vars (`GREENTIC_AW_GUARDRAIL`, `GREENTIC_AW_GUARDRAIL_ID`, `GREENTIC_AW_GUARDRAIL_VERSION`, `GREENTIC_AW_GUARDRAIL_PII`, `GREENTIC_AW_GUARDRAIL_FAILMODE`) and the `guardrail-bedrock` feature to `crates/greentic-runner/CLAUDE.md` (Key Environment Variables + Workspace Features tables) so the canonical docs match the code.

- [ ] **Step 6: Commit docs**

```bash
git add crates/greentic-runner/CLAUDE.md
git commit -m "docs: document AW guardrail env vars and guardrail-bedrock feature"
```

---

## Self-Review

**Spec coverage (§4):**
- §4.1 trait/types/NoopGuardrail/fail-mode → Task 1. `map_apply_guardrail`/PII → Task 2.
- §4.2 OUTPUT decorator (content + tool-call args, streaming stream-then-redact) → Task 3. INPUT + tool-result in `loop.rs`, mask persistence (write-back to `state.messages`), input-block short-circuit → Task 4.
- §4.3 Bedrock `ApplyGuardrail` backend, feature-gated, mask vs block, assessment summary → Task 5.
- §4.4 env wiring (`GREENTIC_AW_GUARDRAIL*`), compose order `Guardrailing(Retrying(..))`, fail mode → Task 6.
- §4.6 tests: noop, output block, tool-arg detection, mask, fail-mode (Task 3); input short-circuit (Task 4); `map_apply_guardrail` cases (Task 2); ignored Bedrock integration (Task 5). **Mask-persistence test:** covered by the Task 4 `input_block_short_circuits_step` shape; add a sibling `input_mask_persists_masked_text` test asserting `state.messages` holds the masked text if a mask-mock is wired — optional but recommended.
- §4.5 telemetry: guardrail verdict assessments flow into `GuardrailVerdict.assessments`; full StepObserver verdict events are a follow-up, not PoC-blocking.

**Type consistency:** `GuardrailAction`/`GuardrailVerdict`/`GuardrailStage`/`IncomingDecision`/`PiiMode`/`GuardrailRuntimeConfig` names are identical across Tasks 1–6. `guard_incoming` and `resolve_action` signatures match their call sites in `loop.rs` and the decorator.

**Placeholder scan:** the two wiring tasks (4, 6) reference exact `loop.rs` line numbers and the real `with_knowledge`/`build_agent_runtime` patterns, with code; they instruct reading the surrounding signature before editing rather than guessing argument order. This is a real constraint of editing a 405-line loop and a shared constructor, not an unfilled placeholder.

**Out of scope (do not implement):** context-window work (spec §5), per-tool guardrail policy, WASM guardrail kind, buffer-until-verdict streaming, raw-assessment audit sink, Cisco/Azure backends.
