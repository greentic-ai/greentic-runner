//! External-guardrail seam for the agentic worker.
//!
//! A [`Guardrail`] inspects untrusted text at three checkpoints: user input
//! and tool results (both enforced in `loop.rs`, where masked content can be
//! persisted), and model output (the [`GuardrailingLlmBackend`] decorator).
//! The trait mirrors [`crate::llm::LlmBackend`]'s async-trait-object shape so a
//! future WASM `guardrail` extension can plug in as a second implementation,
//! exactly like `ExtensionLlmBackend`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use thiserror::Error;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, OnDelta};

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
        return GuardrailVerdict {
            action: GuardrailAction::Block { message: block_fallback.to_string() },
            assessments,
        };
    }
    let message = output_text.unwrap_or_else(|| block_fallback.to_string());
    GuardrailVerdict { action: GuardrailAction::Block { message }, assessments }
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::llm::{LlmBackend, LlmRequest, LlmResponse};
    use crate::config::LlmProviderRef;
    use crate::state::ToolCallRecord;

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
