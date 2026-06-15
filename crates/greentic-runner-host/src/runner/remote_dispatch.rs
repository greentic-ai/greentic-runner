//! Generic async dispatch of flow work to a separate runtime over NATS pub/sub.
//! The `RemoteDispatchHandler` trait isolates the transport so it can be swapped
//! (e.g. to greentic-events EventBus) without touching the flow engine.

use anyhow::Result;
use async_nats::HeaderMap;
use async_trait::async_trait;
use greentic_types::{DispatchMode, RuntimeDispatchRequest, request_topic};
use serde_json::Value;

/// Everything needed to dispatch one unit of work to a runtime.
pub struct RemoteDispatch {
    pub tenant: String,
    pub env: String,
    pub runtime: String,
    pub target: String,
    pub operation: String,
    pub mode: DispatchMode,
    /// Opaque correlation id (= canonical session hint); echoed by the response.
    pub correlation_id: String,
    pub input: Value,
    pub deadline_ms: Option<u64>,
}

/// Outcome of a dispatch: the flow waits for a response, or it was fire-and-forget.
#[derive(Debug)]
pub enum RemoteDispatchAction {
    AwaitingResponse { correlation_id: String },
    Dispatched,
}

/// A built NATS message (broker-free; unit-testable).
pub struct BuiltRequest {
    pub subject: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub action: RemoteDispatchAction,
}

/// Pure builder: turn a [`RemoteDispatch`] into the subject/headers/body to
/// publish plus the resulting action. No I/O — unit-testable without a broker.
pub fn build_request(req: &RemoteDispatch) -> Result<BuiltRequest> {
    let body_struct = RuntimeDispatchRequest {
        target: req.target.clone(),
        operation: req.operation.clone(),
        mode: req.mode,
        input: req.input.clone(),
        deadline_ms: req.deadline_ms,
    };
    let body = serde_json::to_vec(&body_struct)?;

    let mut headers = HeaderMap::new();
    headers.insert("Greentic-Correlation-Id", req.correlation_id.as_str());
    headers.insert("Greentic-Tenant", req.tenant.as_str());
    headers.insert("Greentic-Env", req.env.as_str());
    headers.insert("Greentic-Idempotency-Key", req.correlation_id.as_str());

    let action = match req.mode {
        DispatchMode::Await => RemoteDispatchAction::AwaitingResponse {
            correlation_id: req.correlation_id.clone(),
        },
        DispatchMode::FireAndForget => RemoteDispatchAction::Dispatched,
    };

    Ok(BuiltRequest {
        subject: request_topic(&req.runtime),
        headers,
        body,
        action,
    })
}

/// Transport-agnostic publisher for runtime dispatch requests. Implemented over
/// raw `async-nats` today; the trait lets a greentic-events EventBus be swapped
/// in later without changing the flow engine.
#[async_trait]
pub trait RemoteDispatchHandler: Send + Sync {
    async fn dispatch(&self, request: RemoteDispatch) -> Result<RemoteDispatchAction>;
}

/// Publishes dispatch requests to NATS via raw `async-nats`.
pub struct NatsDispatcher {
    client: async_nats::Client,
}

impl NatsDispatcher {
    pub fn new(client: async_nats::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl RemoteDispatchHandler for NatsDispatcher {
    async fn dispatch(&self, request: RemoteDispatch) -> Result<RemoteDispatchAction> {
        let built = build_request(&request)?;
        self.client
            .publish_with_headers(built.subject, built.headers, built.body.into())
            .await?;
        Ok(built.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{DispatchMode, RuntimeDispatchRequest};
    use serde_json::json;

    fn sample(mode: DispatchMode) -> RemoteDispatch {
        RemoteDispatch {
            tenant: "t1".into(),
            env: "default".into(),
            runtime: "sorla".into(),
            target: "dep-1".into(),
            operation: "create".into(),
            mode,
            correlation_id: "t1:web:chan:conv:user::pack=p".into(), // opaque hint, contains ':'
            input: json!({"a": 1}),
            deadline_ms: Some(1000),
        }
    }

    #[test]
    fn build_request_targets_request_topic_with_headers_and_body() {
        let built = build_request(&sample(DispatchMode::Await)).unwrap();
        assert_eq!(built.subject, "greentic.sorla.request.v1");
        assert_eq!(
            built
                .headers
                .get("Greentic-Correlation-Id")
                .map(|v| v.as_str()),
            Some("t1:web:chan:conv:user::pack=p")
        );
        assert_eq!(
            built.headers.get("Greentic-Tenant").map(|v| v.as_str()),
            Some("t1")
        );
        let body: RuntimeDispatchRequest = serde_json::from_slice(&built.body).unwrap();
        assert_eq!(body.operation, "create");
        assert_eq!(body.mode, DispatchMode::Await);
        assert!(matches!(
            built.action,
            RemoteDispatchAction::AwaitingResponse { .. }
        ));
    }

    #[test]
    fn fire_and_forget_action_is_dispatched() {
        let built = build_request(&sample(DispatchMode::FireAndForget)).unwrap();
        assert!(matches!(built.action, RemoteDispatchAction::Dispatched));
    }
}
