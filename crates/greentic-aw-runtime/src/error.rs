// placeholder — filled in subsequent tasks

/// Why the Plan-Act-Observe loop terminated (Task 1.3).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum TerminationReason {
    /// Agent emitted a final reply.
    Reply,
    /// Step limit reached.
    StepLimitReached,
    /// Token budget exhausted.
    TokenBudgetExhausted,
    /// Caller requested cancellation.
    Cancelled,
}

/// Errors in agent configuration loading (Task 1.3).
#[derive(Debug, thiserror::Error)]
#[error("config error: {0}")]
pub struct ConfigError(pub String);

/// Errors from LLM backend calls (Task 1.3).
#[derive(Debug, thiserror::Error)]
#[error("llm error: {0}")]
pub struct LlmError(pub String);

/// Errors from agent state store operations (Task 1.3).
#[derive(Debug, thiserror::Error)]
#[error("state error: {0}")]
pub struct StateError(pub String);

/// Top-level error type returned by [`crate::AgentRuntime::step`] (Task 1.3).
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    #[error("llm: {0}")]
    Llm(#[from] LlmError),
    #[error("state: {0}")]
    State(#[from] StateError),
    #[error("internal: {0}")]
    Internal(String),
}
