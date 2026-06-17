//! Event bridge: consume `greentic.agentic.request.v1` from NATS, invoke the
//! local agentic-worker runtime via the [`AgentDispatchInvoker`] seam, and
//! publish `greentic.agentic.response.v1` echoing the correlation id.
//!
//! This is the agentic-side counterpart of the runner's `agentic.call` flow
//! node (out-of-process path, "Option C"): the runner publishes a dispatch
//! request; this bridge runs one agentic step and publishes the reply.
//!
//! Unlike `sorx-event-bridge`, this crate lives inside the runner workspace and
//! pins the same `greentic-types` lineage as the runner, so the wire contract is
//! SHARED from [`greentic_types::runtime_dispatch`] rather than mirrored by hand.

pub mod jetstream;

// Re-export so callers can use `aw_event_bridge::run_bridge_jetstream` without
// naming the submodule explicitly.
pub use jetstream::run_bridge_jetstream;

use std::sync::Arc;

use anyhow::Result;
use async_nats::HeaderMap;
use async_trait::async_trait;
use greentic_types::{DispatchError, RuntimeDispatchRequest, RuntimeDispatchResponse};
use serde_json::Value;

// Re-export the shared subject helpers so serve-mode callers can name the
// agentic subjects without an extra greentic-types dependency.
pub use greentic_types::{request_topic, response_topic};

/// Runtime name for the agentic worker; selects the request/response subjects
/// `greentic.agentic.request.v1` / `greentic.agentic.response.v1`.
pub const RUNTIME_NAME: &str = "agentic";

/// Result of invoking the local agentic-worker runtime for one dispatch.
pub struct InvokeOutcome {
    /// Whether the step completed successfully.
    pub ok: bool,
    /// Step output payload (e.g. `{reply, trail, terminated_by}`).
    pub output: Value,
    /// Optional runtime-emitted events (empty for the agentic worker today).
    pub events: Vec<Value>,
}

/// Seam over the actual agentic-worker invocation. The production impl wraps
/// `greentic_aw_runtime::AgentRuntime`; tests use a stub.
///
/// `target` is the agent id (the runner's `agentic.call.<agent_id>` node maps
/// the node target to this field); `operation` is reserved for future
/// multi-operation agents and may be empty. `input` is the opaque node input —
/// the production invoker extracts `user_text` from it exactly as the in-process
/// `agent_node` path does.
#[async_trait]
pub trait AgentDispatchInvoker: Send + Sync {
    /// Run one agentic step.
    ///
    /// * `tenant` / `env` — multi-tenant context echoed from the request headers.
    /// * `target` — the agent id.
    /// * `operation` — reserved; may be empty.
    /// * `input` — opaque node input (expects at least `{"user_text": "..."}`).
    /// * `idempotency_key` — correlation/idempotency hint; doubles as the
    ///   session id when the input carries no explicit `session_id`.
    async fn invoke(
        &self,
        tenant: &str,
        env: &str,
        target: &str,
        operation: &str,
        input: Value,
        idempotency_key: Option<&str>,
    ) -> Result<InvokeOutcome>;
}

/// Invoke and build the response (no NATS I/O). Errors map to an error response.
pub async fn build_response(
    invoker: Arc<dyn AgentDispatchInvoker>,
    tenant: &str,
    env: &str,
    idempotency_key: Option<&str>,
    req: RuntimeDispatchRequest,
) -> RuntimeDispatchResponse {
    match invoker
        .invoke(
            tenant,
            env,
            &req.target,
            &req.operation,
            req.input,
            idempotency_key,
        )
        .await
    {
        Ok(outcome) => RuntimeDispatchResponse {
            ok: outcome.ok,
            output: outcome.output,
            events: outcome.events,
            error: None,
        },
        Err(error) => RuntimeDispatchResponse {
            ok: false,
            output: Value::Null,
            events: vec![],
            error: Some(DispatchError {
                code: "invoke_failed".into(),
                message: error.to_string(),
            }),
        },
    }
}

/// Handle one request message end-to-end: decode, invoke, publish response.
///
/// The correlation id is echoed VERBATIM (the runner's `agentic.call` node
/// encodes `::pack=…::flow=…::thread=…::reply=…` resume markers there and parses
/// them back on response, so the bridge must not alter it).
pub async fn handle_message(
    client: &async_nats::Client,
    invoker: Arc<dyn AgentDispatchInvoker>,
    msg: async_nats::Message,
) -> Result<()> {
    let headers = msg.headers.as_ref();
    let get_header = |name: &str| -> Option<String> {
        headers
            .and_then(|header_map| header_map.get(name))
            .map(|value| value.as_str().to_string())
    };

    let correlation = get_header("Greentic-Correlation-Id");
    // The runner sets the idempotency key equal to the correlation id; prefer the
    // explicit header but fall back to the correlation id so the invoker always
    // has a stable session hint.
    let idempotency = get_header("Greentic-Idempotency-Key").or_else(|| correlation.clone());
    let tenant = get_header("Greentic-Tenant").unwrap_or_default();
    let env = get_header("Greentic-Env").unwrap_or_else(|| "default".to_string());

    let req: RuntimeDispatchRequest = serde_json::from_slice(&msg.payload)?;
    let resp = build_response(invoker, &tenant, &env, idempotency.as_deref(), req).await;

    let mut out_headers = HeaderMap::new();
    if let Some(correlation_value) = correlation.as_deref() {
        out_headers.insert("Greentic-Correlation-Id", correlation_value);
    }
    out_headers.insert("Greentic-Tenant", tenant.as_str());
    out_headers.insert("Greentic-Env", env.as_str());

    let response_bytes = serde_json::to_vec(&resp)?;
    client
        .publish_with_headers(
            response_topic(RUNTIME_NAME),
            out_headers,
            response_bytes.into(),
        )
        .await?;
    Ok(())
}

/// Subscribe to `greentic.agentic.request.v1` and serve forever (one spawned
/// task per message).
pub async fn run_bridge(
    client: async_nats::Client,
    invoker: Arc<dyn AgentDispatchInvoker>,
) -> Result<()> {
    use futures_util::StreamExt;
    let mut subscriber = client.subscribe(request_topic(RUNTIME_NAME)).await?;
    while let Some(msg) = subscriber.next().await {
        let client = client.clone();
        let invoker = invoker.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_message(&client, invoker, msg).await {
                tracing::error!(%error, "aw event bridge failed to handle request");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::DispatchMode;
    use serde_json::json;
    use std::sync::Mutex;

    /// (tenant/env elided, target, operation, payload, idempotency_key)
    type SeenCall = (String, String, Value, Option<String>);

    struct StubInvoker {
        seen: Mutex<Vec<SeenCall>>,
    }

    #[async_trait]
    impl AgentDispatchInvoker for StubInvoker {
        async fn invoke(
            &self,
            _tenant: &str,
            _env: &str,
            target: &str,
            operation: &str,
            input: Value,
            idempotency_key: Option<&str>,
        ) -> Result<InvokeOutcome> {
            self.seen.lock().unwrap().push((
                target.to_string(),
                operation.to_string(),
                input.clone(),
                idempotency_key.map(str::to_string),
            ));
            Ok(InvokeOutcome {
                ok: true,
                output: json!({"reply": "pong", "trail": [], "terminated_by": "reply"}),
                events: vec![],
            })
        }
    }

    fn sample_request() -> RuntimeDispatchRequest {
        RuntimeDispatchRequest {
            target: "greeter".into(),
            operation: String::new(),
            mode: DispatchMode::Await,
            input: json!({"user_text": "ping"}),
            deadline_ms: Some(30_000),
        }
    }

    #[test]
    fn subjects_use_agentic_runtime_name() {
        assert_eq!(request_topic(RUNTIME_NAME), "greentic.agentic.request.v1");
        assert_eq!(response_topic(RUNTIME_NAME), "greentic.agentic.response.v1");
    }

    #[tokio::test]
    async fn handle_invokes_and_maps_agent_output_to_response() {
        let invoker = Arc::new(StubInvoker {
            seen: Mutex::new(vec![]),
        });
        let resp = build_response(
            invoker.clone(),
            "acme",
            "prod",
            Some("sess-1::pack=p::flow=f"),
            sample_request(),
        )
        .await;

        assert!(resp.ok);
        assert_eq!(resp.output["reply"], json!("pong"));
        assert_eq!(resp.output["terminated_by"], json!("reply"));
        assert!(resp.error.is_none());

        let seen = invoker.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (target, _operation, input, idempotency) = &seen[0];
        assert_eq!(target, "greeter", "dispatch target maps to agent id");
        assert_eq!(input["user_text"], json!("ping"));
        assert_eq!(
            idempotency.as_deref(),
            Some("sess-1::pack=p::flow=f"),
            "correlation/idempotency hint is forwarded to the invoker"
        );
    }

    #[tokio::test]
    async fn invoke_error_maps_to_error_response() {
        struct FailInvoker;

        #[async_trait]
        impl AgentDispatchInvoker for FailInvoker {
            async fn invoke(
                &self,
                _tenant: &str,
                _env: &str,
                _target: &str,
                _operation: &str,
                _input: Value,
                _idempotency_key: Option<&str>,
            ) -> Result<InvokeOutcome> {
                Err(anyhow::anyhow!("boom"))
            }
        }

        let resp = build_response(
            Arc::new(FailInvoker),
            "acme",
            "prod",
            Some("c"),
            sample_request(),
        )
        .await;

        assert!(!resp.ok);
        assert_eq!(resp.output, Value::Null);
        let error = resp.error.expect("error response must carry details");
        assert_eq!(error.code, "invoke_failed");
        assert_eq!(error.message, "boom");
    }
}
