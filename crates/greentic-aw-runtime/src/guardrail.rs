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
