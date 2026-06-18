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
    Guardrail, GuardrailError, GuardrailStage, GuardrailVerdict, PiiMode, map_apply_guardrail,
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
                let cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>,
    > {
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

            // In aws-sdk-bedrockruntime 1.x, `action()` returns `&BedrockAction`
            // (not `Option<&BedrockAction>`), so we match directly without `Some`.
            let intervened = matches!(out.action(), BedrockAction::GuardrailIntervened);
            let output_text = out
                .outputs()
                .first()
                .and_then(|o| o.text())
                .map(str::to_string);
            // PII-only when there is at least one sensitive-information
            // assessment and no other-policy intervention (topic, content,
            // word, contextual-grounding, or automated-reasoning).  Any
            // non-sensitive-information policy firing forces a Block (the safe
            // direction) rather than mask-and-continue.
            let mut has_pii = false;
            let mut has_other = false;
            for a in out.assessments() {
                if a.sensitive_information_policy().is_some() {
                    has_pii = true;
                }
                if a.topic_policy().is_some()
                    || a.content_policy().is_some()
                    || a.word_policy().is_some()
                    || a.contextual_grounding_policy().is_some()
                    || a.automated_reasoning_policy().is_some()
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
        let v = g
            .check(GuardrailStage::Input, "Hello, how are you?")
            .await
            .unwrap();
        assert_eq!(v.action, crate::guardrail::GuardrailAction::Allow);
    }
}
