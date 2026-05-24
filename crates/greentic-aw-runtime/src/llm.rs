// placeholder — filled in subsequent tasks

use crate::error::LlmError;

/// Request sent to an LLM backend (Task 1.7).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmRequest;

/// Response from an LLM backend (Task 1.7).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmResponse;

/// Abstraction over LLM providers (Task 1.7).
///
/// Uses `Pin<Box<dyn Future>>` for dyn-safety when stored as `Arc<dyn LlmBackend>`.
pub trait LlmBackend: Send + Sync {
    fn complete(
        &self,
        request: LlmRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<LlmResponse, LlmError>> + Send + '_>,
    >;
}

/// Wraps an [`LlmBackend`] with exponential-backoff retry logic (Task 1.7).
pub struct RetryingLlmBackend;
