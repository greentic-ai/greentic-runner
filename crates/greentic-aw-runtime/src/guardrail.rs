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

use thiserror::Error;

use crate::llm::LlmResponse;

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
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
