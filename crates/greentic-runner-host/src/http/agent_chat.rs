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

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, extract::State};

use crate::host::RunnerHost;
use crate::http::auth::AdminGuard;
use crate::runner::ServerState;

/// Default conversation/user identifiers so a caller that omits them still
/// threads a single in-memory conversation across turns.
const DEFAULT_CONVERSATION: &str = "test-chat";
const DEFAULT_USER: &str = "test-chat-user";

/// Extracted core logic — separated so tests can exercise tenant resolution
/// and error mapping without needing the full axum extractor stack.
async fn execute_chat(
    host: &RunnerHost,
    default_tenant: &str,
    req: AgentChatRequest,
) -> Result<AgentChatResponse, (StatusCode, serde_json::Value)> {
    let tenant = req
        .tenant
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| default_tenant.to_string());

    let mut activity = Activity::text(req.text)
        .in_conversation(
            req.conversation_id
                .unwrap_or_else(|| DEFAULT_CONVERSATION.to_string()),
        )
        .from_user(req.user_id.unwrap_or_else(|| DEFAULT_USER.to_string()));
    if let Some(flow) = req.flow_id {
        activity = activity.with_flow(flow);
    }

    match host.handle_activity(&tenant, activity).await {
        Ok(activities) => Ok(replies_to_response(activities)),
        Err(e) => {
            let msg = format!("{e:#}");
            // handle_activity returns "tenant <name> not loaded" when the tenant
            // isn't present in ActivePacks. Surface that as 404.
            let (code, error) = if msg.contains("not loaded") || msg.contains("not registered") {
                (StatusCode::NOT_FOUND, "tenant_not_loaded")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "agent_chat_failed")
            };
            Err((code, serde_json::json!({ "error": error, "message": msg })))
        }
    }
}

/// `POST /agent/chat` — loopback-only (AdminGuard). Sends one chat turn to
/// the loaded worker pack and returns its reply.
pub async fn agent_chat(
    _guard: AdminGuard,
    State(state): State<ServerState>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse {
    match execute_chat(&state.host, state.routing.default_tenant(), req).await {
        Ok(response) => (StatusCode::OK, Json(response)).into_response(),
        Err((code, body)) => (code, Json(body)).into_response(),
    }
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

    /// Route wiring smoke-test: `execute_chat` against a host with no loaded
    /// packs returns 404 with `error = "tenant_not_loaded"`, proving that
    /// `handle_activity`'s "not loaded" error is correctly mapped by the handler
    /// core.
    #[tokio::test]
    async fn agent_chat_unknown_tenant_maps_to_not_found() {
        let host = crate::host::RunnerHost::for_test();
        let req = AgentChatRequest {
            text: "hello".into(),
            tenant: Some("nope".into()),
            conversation_id: None,
            user_id: None,
            flow_id: None,
        };
        let err = execute_chat(&host, "test", req)
            .await
            .expect_err("unknown tenant should fail");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert_eq!(err.1["error"], "tenant_not_loaded");
    }
}
