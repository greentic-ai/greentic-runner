use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Response as AxumResponse, StatusCode, Uri,
};
use axum::response::IntoResponse;
use axum::BoxError;
use serde_json::{json, Map, Value};

use super::{engine::FlowContext, ServerState};

pub async fn dispatch(
    Path(flow_id): Path<String>,
    State(state): State<Arc<ServerState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<AxumResponse<Body>, AxumResponse<Body>> {
    let flow = state
        .engine
        .flow_by_id(&flow_id)
        .ok_or_else(|| build_error(StatusCode::NOT_FOUND, "flow not found"))?;

    if flow.flow_type != "webhook" {
        return Err(build_error(
            StatusCode::CONFLICT,
            "flow is not registered as a webhook",
        ));
    }

    if !state.config.webhook_policy.is_allowed(uri.path()) {
        return Err(build_error(
            StatusCode::FORBIDDEN,
            "path not permitted by policy",
        ));
    }

    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());

    if let Some(key) = idempotency_key.as_ref() {
        if let Some(cached) = {
            let mut cache = state.webhook_cache.lock();
            cache.get(key).cloned()
        } {
            tracing::debug!(flow_id = %flow.id, idempotency_key = key, "webhook cache hit");
            return build_response(cached).map_err(|_| {
                build_error(StatusCode::INTERNAL_SERVER_ERROR, "cached response invalid")
            });
        }
    }

    let normalized = normalize_request(&method, &uri, &headers, &body);
    match state
        .engine
        .execute(
            FlowContext {
                tenant: &state.config.tenant,
                flow_id: &flow.id,
                node_id: None,
                tool: None,
                action: Some("webhook"),
                retry_config: state.config.mcp_retry_config().into(),
            },
            normalized,
        )
        .await
    {
        Ok(value) => {
            if let Some(key) = idempotency_key {
                let mut cache = state.webhook_cache.lock();
                cache.put(key.clone(), value.clone());
            }
            Ok(build_response(value).unwrap_or_else(|err| {
                tracing::error!(flow_id = %flow.id, error = %err, "failed to render webhook response");
                build_error(StatusCode::INTERNAL_SERVER_ERROR, "malformed flow response")
            }))
        }
        Err(err) => {
            let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
            tracing::error!(
                flow_id = %flow.id,
                error.cause_chain = ?chain,
                "webhook flow execution failed"
            );
            Err(build_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "webhook flow failed",
            ))
        }
    }
}

fn normalize_request(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Value {
    let headers_json = headers.iter().fold(Map::new(), |mut acc, (name, value)| {
        acc.insert(
            name.as_str().to_string(),
            Value::String(value.to_str().unwrap_or_default().to_string()),
        );
        acc
    });

    let body_value = if body.is_empty() {
        Value::Null
    } else if let Ok(text) = std::str::from_utf8(body) {
        json!({ "text": text })
    } else {
        json!({ "base16": hex::encode(body) })
    };

    json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "headers": headers_json,
        "body": body_value,
    })
}

fn build_response(value: Value) -> Result<AxumResponse<Body>, BoxError> {
    let mut builder = AxumResponse::builder().status(StatusCode::OK);
    let mut headers = HeaderMap::new();
    let body;

    match value {
        Value::String(text) => {
            body = Body::from(text);
        }
        Value::Object(map) => {
            if let Some(status) = map
                .get("status")
                .and_then(|status| status.as_u64())
                .and_then(|status| u16::try_from(status).ok())
            {
                builder = builder.status(StatusCode::from_u16(status)?);
            }
            if let Some(headers_value) = map.get("headers").and_then(|h| h.as_object()) {
                for (key, value) in headers_value {
                    if let Some(value_str) = value.as_str() {
                        if let Ok(header_name) = key.parse::<HeaderName>() {
                            if let Ok(header_value) = HeaderValue::from_str(value_str) {
                                headers.insert(header_name, header_value);
                            }
                        }
                    }
                }
            }
            if let Some(body_value) = map.get("body") {
                body = serialize_body(body_value);
            } else if let Some(text_value) = map.get("text") {
                body = serialize_body(text_value);
            } else {
                body = Body::from(Value::Object(map).to_string());
            }
        }
        other => {
            body = Body::from(other.to_string());
        }
    }

    let mut final_response = builder.body(body)?;
    *final_response.headers_mut() = headers;
    Ok(final_response)
}

fn serialize_body(value: &Value) -> Body {
    match value {
        Value::String(text) => Body::from(text.clone()),
        Value::Object(obj) if obj.contains_key("text") => obj
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Body::from(text.to_owned()))
            .unwrap_or_else(|| Body::from(value.to_string())),
        Value::Object(obj) if obj.contains_key("base16") => obj
            .get("base16")
            .and_then(Value::as_str)
            .and_then(|hex_value| hex::decode(hex_value).ok())
            .map(Body::from)
            .unwrap_or_else(|| Body::from(value.to_string())),
        _ => Body::from(value.to_string()),
    }
}

fn build_error(status: StatusCode, message: &'static str) -> AxumResponse<Body> {
    let payload = json!({ "error": message });
    (status, axum::Json(payload)).into_response()
}
