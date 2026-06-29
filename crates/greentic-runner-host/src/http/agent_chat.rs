//! `POST /agent/chat` — a loopback HTTP ingress that wraps `RunnerHost::handle_activity`
//! so an external caller (the designer's runner sidecar) can send a chat turn to a
//! loaded agentic-worker pack and receive the reply. Blocking JSON response (v1).

use serde::{Deserialize, Serialize};

use crate::activity::Activity;

/// One chat turn for a loaded worker pack.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatRequest {
    pub text: String,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub flow_id: Option<String>,
}

/// One outbound reply line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyView {
    pub text: String,
}

/// The worker's reply turn.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatResponse {
    pub replies: Vec<ReplyView>,
}

/// Extract a human-readable reply line from an outbound activity.
///
/// Priority order:
/// 1. `payload["text"]` as a string
/// 2. `payload["messages"][0]["text"]` as a string
/// 3. Compact JSON rendering of the whole payload
fn reply_text(activity: &Activity) -> String {
    let payload = activity.payload();
    if let Some(t) = payload.get("text").and_then(|v| v.as_str()) {
        return t.to_string();
    }
    if let Some(t) = payload
        .get("messages")
        .and_then(|m| m.get(0))
        .and_then(|m0| m0.get("text"))
        .and_then(|v| v.as_str())
    {
        return t.to_string();
    }
    serde_json::to_string(payload).unwrap_or_default()
}

/// Map the runtime's outbound activities into the chat response, dropping empties.
pub fn replies_to_response(activities: Vec<Activity>) -> AgentChatResponse {
    let replies = activities
        .iter()
        .map(reply_text)
        .filter(|t| !t.trim().is_empty())
        .map(|text| ReplyView { text })
        .collect();
    AgentChatResponse { replies }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Activity;
    use serde_json::json;

    fn reply_with(payload: serde_json::Value) -> Activity {
        // Build an outbound-style activity carrying `payload`. Use the same
        // constructor the runner uses for replies (Activity::from_output) —
        // read activity.rs and match it; here we assert on the mapping only.
        Activity::from_output(payload, "demo")
    }

    #[test]
    fn maps_text_payload_to_reply() {
        let out = replies_to_response(vec![reply_with(json!({"text": "hello there"}))]);
        assert_eq!(out.replies.len(), 1);
        assert_eq!(out.replies[0].text, "hello there");
    }

    #[test]
    fn maps_nested_messages_text() {
        let out = replies_to_response(vec![reply_with(
            json!({"messages": [{"text": "nested hi"}]}),
        )]);
        assert_eq!(out.replies[0].text, "nested hi");
    }

    #[test]
    fn skips_empty_and_keeps_order() {
        let out = replies_to_response(vec![
            reply_with(json!({"text": ""})),
            reply_with(json!({"text": "second"})),
        ]);
        assert_eq!(out.replies.len(), 1);
        assert_eq!(out.replies[0].text, "second");
    }

    #[test]
    fn request_deserializes_camel_case() {
        let r: AgentChatRequest = serde_json::from_value(json!({
            "text": "hi", "conversationId": "c1", "userId": "u1"
        }))
        .unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(r.conversation_id.as_deref(), Some("c1"));
        assert_eq!(r.user_id.as_deref(), Some("u1"));
    }
}
