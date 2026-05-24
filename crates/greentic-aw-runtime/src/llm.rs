//! LLM backend trait + a retry decorator. Concrete OpenAI / Anthropic
//! impls are added in Phase 3; the trait + decorator are introduced
//! here so the loop can be wired against mocks during Phase 1.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::state::{ChatMessage, ToolCallRecord};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmRequest {
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,
    pub tools: Vec<LlmToolSchema>,
    /// Resolved provider + model — backend selects credentials/endpoint
    /// based on this.
    pub provider: crate::config::LlmProviderRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmToolSchema {
    pub extension_id: String,
    pub tool_name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON schema
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmResponse {
    /// `content` is `Some` when the LLM emits a textual reply. Per
    /// spec Decision 12, if `tool_calls` is non-empty AND `content`
    /// is `Some`, the loop treats `content` as a reasoning trace and
    /// executes `tool_calls` (tool_calls win).
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

pub trait LlmBackend: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>>;
}

/// Wraps any [`LlmBackend`] with exponential-backoff retry on
/// [`LlmError::ServiceUnavailable`]. 4xx-class errors are NOT retried.
pub struct RetryingLlmBackend<B: LlmBackend> {
    inner: B,
    attempts: u32,
    backoff: Duration,
}

impl<B: LlmBackend> RetryingLlmBackend<B> {
    pub fn new(inner: B, attempts: u32, backoff: Duration) -> Self {
        Self {
            inner,
            attempts,
            backoff,
        }
    }
}

impl<B: LlmBackend + Send + Sync> LlmBackend for RetryingLlmBackend<B> {
    fn complete<'a>(
        &'a self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let mut delay = self.backoff;
            let mut last_err = None;
            for attempt in 0..self.attempts.max(1) {
                match self.inner.complete(request.clone()).await {
                    Ok(r) => return Ok(r),
                    Err(LlmError::ServiceUnavailable) => {
                        last_err = Some(LlmError::ServiceUnavailable);
                        if attempt + 1 < self.attempts {
                            tokio::time::sleep(delay).await;
                            delay = delay.saturating_mul(2);
                        }
                    }
                    Err(other) => return Err(other), // 4xx-class: do not retry
                }
            }
            Err(last_err.unwrap_or(LlmError::ServiceUnavailable))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct ScriptedBackend {
        responses: Mutex<Vec<Result<LlmResponse, LlmError>>>,
    }

    impl LlmBackend for ScriptedBackend {
        fn complete<'a>(
            &'a self,
            _r: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
            let next = self.responses.lock().unwrap().remove(0);
            Box::pin(async move { next })
        }
    }

    fn req() -> LlmRequest {
        LlmRequest {
            system_prompt: "".into(),
            history: vec![],
            tools: vec![],
            provider: crate::config::LlmProviderRef {
                provider: "openai".into(),
                model: "x".into(),
            },
        }
    }

    fn ok_resp() -> LlmResponse {
        LlmResponse {
            content: Some("hi".into()),
            tool_calls: vec![],
            tokens_in: 1,
            tokens_out: 1,
        }
    }

    #[tokio::test]
    async fn retries_on_service_unavailable_then_succeeds() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
                Ok(ok_resp()),
            ]),
        };
        let r = RetryingLlmBackend::new(inner, 3, Duration::from_millis(1));
        let out = r.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn does_not_retry_on_bad_request() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![Err(LlmError::BadRequest("nope".into()))]),
        };
        let r = RetryingLlmBackend::new(inner, 5, Duration::from_millis(1));
        let err = r.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::BadRequest(_)));
    }

    #[tokio::test]
    async fn returns_service_unavailable_after_all_attempts() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
            ]),
        };
        let r = RetryingLlmBackend::new(inner, 3, Duration::from_millis(1));
        let err = r.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::ServiceUnavailable));
    }
}
