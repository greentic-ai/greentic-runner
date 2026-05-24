// placeholder — filled in subsequent tasks

/// Reference to an LLM provider (Task 1.4).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LlmProviderRef;

/// Reference to a tool available to the agent (Task 1.4).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolRef;

/// Per-agent limits (max_steps, token budgets, etc.) (Task 1.4).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentLimits;

/// Full agent configuration loaded by [`crate::ConfigProvider`] (Task 1.4).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentConfig;
