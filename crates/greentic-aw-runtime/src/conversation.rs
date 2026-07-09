//! Conversational-segment exit tool for the agentic-worker loop.
//!
//! Mirrors [`crate::short_term`]'s host-tool pattern: a reserved `"host"`
//! extension id with a built-in `end_conversation` tool, advertised +
//! intercepted only when the node opts in via `AgentConfig.conversational`.

use crate::config::AgentConfig;
use crate::llm::LlmToolSchema;

/// Reserved extension id for host-provided built-in tools (shared with memory tiers).
pub(crate) const CONVERSATION_EXTENSION_ID: &str = "host";
/// Host built-in tool: end the current conversational segment.
pub(crate) const END_CONVERSATION_TOOL: &str = "end_conversation";

/// One-line system-prompt note appended for conversational nodes so the model
/// knows it MAY end the conversation (spec §"Tool discoverability").
pub(crate) const END_CONVERSATION_SYSTEM_NOTE: &str =
    "When the user's goal for this conversation has been met, call the \
     `end_conversation` tool to finish. Put your closing message to the user in \
     your assistant reply on that same turn (or pass it as the tool's `note`).";

/// Conversational mode is active when the agent's config opts in.
pub(crate) fn conversational_active(config: &AgentConfig) -> bool {
    config.conversational
}

/// LLM-facing schema for the host built-in `end_conversation` tool.
pub(crate) fn end_conversation_tool_schema() -> LlmToolSchema {
    LlmToolSchema {
        extension_id: CONVERSATION_EXTENSION_ID.to_string(),
        tool_name: END_CONVERSATION_TOOL.to_string(),
        description: "End the current conversation once the user's goal has been \
             met. Provide your closing message as your assistant reply this turn, \
             or pass it as `note`. After this the flow advances to the next step."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": "Optional closing message to show the user."
                }
            }
        }),
    }
}

/// Append the end-conversation note to a system prompt (call only when active).
pub(crate) fn augment_system_prompt(base: &str) -> String {
    format!("{base}\n\n{END_CONVERSATION_SYSTEM_NOTE}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};

    fn cfg(conversational: bool) -> AgentConfig {
        AgentConfig {
            agent_id: "a".into(),
            system_prompt: "sys".into(),
            tools: vec![],
            guardrails: vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "gpt-4".into(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
            memory: None,
            knowledge: None,
            conversational,
        }
    }

    #[test]
    fn active_follows_config_flag() {
        assert!(conversational_active(&cfg(true)));
        assert!(!conversational_active(&cfg(false)));
    }

    #[test]
    fn schema_names_the_host_builtin() {
        let s = end_conversation_tool_schema();
        assert_eq!(s.extension_id, "host");
        assert_eq!(s.tool_name, "end_conversation");
        assert!(s.parameters["properties"]["note"].is_object());
    }

    #[test]
    fn augment_appends_the_note_once() {
        let out = augment_system_prompt("BASE");
        assert!(out.starts_with("BASE"));
        assert!(out.contains("end_conversation"));
    }
}
