// Minimal stubs — Task 1.4 replaces this with the full implementation.
// These types satisfy the compile-time contract for `error.rs` tests and
// the `lib.rs` re-exports (`AgentConfig`, `AgentLimits`, `LlmProviderRef`,
// `ToolRef`).

/// Reference to an LLM provider and the model to use.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmProviderRef {
    /// Provider name (e.g. `"openai"`, `"anthropic"`).
    pub provider: String,
    /// Model identifier (e.g. `"gpt-4o"`, `"claude-3-5-sonnet"`).
    pub model: String,
}

/// Reference to a tool available to the agent.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolRef {
    /// Tool name as registered with the extension runtime.
    pub name: String,
}

/// Per-agent runtime limits (token budgets, iteration caps, etc.).
///
/// All fields are optional so callers can use `..AgentLimits::default()` to
/// fill in only the fields they care about.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentLimits {
    /// Tenant-supplied override for the LLM-unavailability user message.
    /// When `None` the built-in default from [`crate::error::AgentError::user_facing_message`]
    /// is used.
    pub provider_failure_message: Option<String>,

    /// Maximum Plan-Act-Observe iterations per step. `None` = runtime default.
    pub max_iterations: Option<u32>,

    /// Per-step wall-clock timeout in seconds. `None` = runtime default.
    pub step_timeout_secs: Option<u64>,

    /// Daily token budget across all sessions for this agent. `None` = unlimited.
    pub daily_token_budget: Option<u64>,
}

/// Full agent configuration loaded by [`crate::ConfigProvider`].
///
/// Task 1.4 will extend this struct with additional fields (e.g. persona,
/// memory profile, guardrails). The shape here is intentionally minimal so
/// the error-module tests compile today.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentConfig {
    /// Stable agent identifier scoped to the owning tenant.
    pub agent_id: String,

    /// System prompt injected as the first message in every LLM call.
    pub system_prompt: String,

    /// Tools available to this agent.
    pub tools: Vec<ToolRef>,

    /// LLM provider and model selection.
    pub llm: LlmProviderRef,

    /// Runtime limits for this agent.
    pub limits: AgentLimits,
}
