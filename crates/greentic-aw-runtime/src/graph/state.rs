//! Run-scoped state threaded through the agent-graph executor.
//!
//! Ported from the greentic-designer spike
//! (`src/orchestrate/agent_graph/state.rs`), with `TriageState` renamed to
//! `GraphRunState` and `Role`/`Message` renamed to `GraphRole`/`GraphMessage`
//! for namespace clarity at the runner level.
//!
//! Designer-only helpers intentionally not ported:
//! - `TriageState::new(user_input)` — the runner initialises state via
//!   `Default` and the executor pushes the first user message explicitly;
//!   there is no single-argument constructor needed in this crate yet.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GraphRole
// ---------------------------------------------------------------------------

/// Speaker role for a message in the agent conversation.
///
/// Serialised as lowercase strings (`"user"`, `"assistant"`, `"tool"`) to
/// match the wire format used by the designer spike and OpenAI-compatible
/// providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphRole {
    User,
    Assistant,
    Tool,
}

// ---------------------------------------------------------------------------
// GraphMessage
// ---------------------------------------------------------------------------

/// A single turn in the conversation log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphMessage {
    pub role: GraphRole,
    pub content: String,
}

// ---------------------------------------------------------------------------
// GraphRunState
// ---------------------------------------------------------------------------

/// Run-scoped state threaded through the executor. `messages` is append-only
/// (never replaced) so a resumed run can deterministically reconstruct the
/// conversation. `resolved` is set by the agent turn; `iterations` counts
/// how many times the router has looped back to the agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRunState {
    /// Append-only conversation log.
    pub messages: Vec<GraphMessage>,
    /// Set `true` by the agent when the issue is considered resolved.
    pub resolved: bool,
    /// Number of completed router→agent loop iterations.
    pub iterations: u32,
    /// Free-form agent working memory; opaque to the executor.
    #[serde(default)]
    pub scratchpad: serde_json::Value,
}

impl GraphRunState {
    /// Append a message. Messages are never replaced — this is the reducer.
    pub fn push_message(&mut self, role: GraphRole, content: impl Into<String>) {
        self.messages.push(GraphMessage {
            role,
            content: content.into(),
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn push_message_appends_never_replaces() {
        let mut s = GraphRunState::default();
        s.push_message(GraphRole::User, "hi");
        assert_eq!(s.messages.len(), 1);
        s.push_message(GraphRole::Assistant, "hello");
        s.push_message(GraphRole::Tool, "{\"found\":true}");
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[0].role, GraphRole::User);
        assert_eq!(s.messages[2].content, "{\"found\":true}");
    }

    #[test]
    fn json_round_trips_camel_case() {
        let mut s = GraphRunState::default();
        s.push_message(GraphRole::User, "hi");
        s.resolved = true;
        s.iterations = 2;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"iterations\":2"), "json: {json}");
        let back: GraphRunState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resolved, s.resolved);
        assert_eq!(back.iterations, s.iterations);
        assert_eq!(back.messages.len(), s.messages.len());
    }

    #[test]
    fn role_serialises_lowercase() {
        let json = serde_json::to_string(&GraphRole::Assistant).unwrap();
        assert_eq!(json, r#""assistant""#);
    }
}
