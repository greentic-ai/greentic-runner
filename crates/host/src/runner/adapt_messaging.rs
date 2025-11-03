use std::sync::Arc;

use anyhow::{Result, bail};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{ServerState, engine::FlowContext};

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramMessage {
    #[serde(default)]
    text: Option<String>,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TelegramUser {
    id: i64,
}

pub async fn telegram_webhook(
    State(state): State<Arc<ServerState>>,
    Json(update): Json<TelegramUpdate>,
) -> StatusCode {
    if let Some(status) = {
        let mut cache = state.telegram_cache.lock();
        cache.get(&update.update_id).copied()
    } {
        tracing::debug!(
            update_id = update.update_id,
            status = %status,
            "duplicate telegram update skipped"
        );
        return status;
    }

    let message = match update.message {
        Some(msg) => msg,
        None => {
            tracing::debug!(update_id = update.update_id, "no message payload in update");
            return remember_status(state.as_ref(), update.update_id, StatusCode::NO_CONTENT);
        }
    };

    let text = match message.text {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            tracing::debug!(update_id = update.update_id, "ignoring non-text message");
            return remember_status(state.as_ref(), update.update_id, StatusCode::NO_CONTENT);
        }
    };

    let flow = match state.engine.flow_by_type("messaging") {
        Some(flow) => flow,
        None => {
            tracing::error!("no messaging flow registered in pack");
            return StatusCode::NOT_FOUND;
        }
    };

    let payload = json!({
        "chat_id": message.chat.id,
        "text": text,
        "user_id": message.from.as_ref().map(|user| user.id),
        "update_id": update.update_id,
    });

    match state
        .engine
        .execute(
            FlowContext {
                tenant: &state.config.tenant,
                flow_id: &flow.id,
                node_id: None,
                tool: None,
                action: Some("messaging"),
                session_id: None,
                provider_id: None,
                retry_config: state.config.mcp_retry_config().into(),
            },
            payload,
        )
        .await
    {
        Ok(response) => {
            if let Some(outgoing_text) = extract_text_response(&response)
                && let Err(err) =
                    send_telegram_message(state.as_ref(), message.chat.id, &outgoing_text).await
            {
                tracing::error!(
                    flow_id = %flow.id,
                    update_id = update.update_id,
                    error = %err,
                    "failed to send telegram message"
                );
                return remember_status(state.as_ref(), update.update_id, StatusCode::BAD_GATEWAY);
            }
            tracing::info!(
                flow_id = %flow.id,
                update_id = update.update_id,
                response = %response,
                "flow completed"
            );
            remember_status(state.as_ref(), update.update_id, StatusCode::OK)
        }
        Err(err) => {
            let chained = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
            tracing::error!(
                flow_id = %flow.id,
                update_id = update.update_id,
                error.cause_chain = ?chained,
                "flow execution failed"
            );
            remember_status(
                state.as_ref(),
                update.update_id,
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        }
    }
}

fn extract_text_response(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_owned());
    }
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    None
}

async fn send_telegram_message(state: &ServerState, chat_id: i64, text: &str) -> Result<()> {
    if !state.messaging_rate.lock().try_acquire() {
        bail!("messaging send rate exceeded");
    }

    let token = state.get_secret("TELEGRAM_BOT_TOKEN")?;
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let body = json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "MarkdownV2",
    });
    state
        .http_client
        .post(url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

fn remember_status(state: &ServerState, update_id: i64, status: StatusCode) -> StatusCode {
    let mut cache = state.telegram_cache.lock();
    cache.put(update_id, status);
    status
}
