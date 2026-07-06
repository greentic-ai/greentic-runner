//! Event bridge: consume `greentic.telco-x.request.v1` from NATS, invoke a
//! Telco-X runtime via the [`TelcoXDispatchInvoker`] seam, and publish
//! `greentic.telco-x.response.v1` echoing the correlation id.
//!
//! This is the Telco-X-side counterpart of the runner's `telco-x.call` flow
//! node (out-of-process dispatch). The runner publishes a dispatch request; this
//! bridge runs one Telco-X operation and publishes the reply. It mirrors
//! `aw-event-bridge` (the `agentic.call` side) exactly, so the wire contract is
//! SHARED from [`greentic_types::runtime_dispatch`] rather than mirrored by hand.
//!
//! **Phase 2 transport scaffold.** The transport is complete; the Telco-X
//! *business operations* are not. A real deployment provides a
//! [`TelcoXDispatchInvoker`] that maps `(target, operation, input)` onto the
//! Telco-X solution layer. [`EchoInvoker`] is a credit-free placeholder used by
//! the `telco-x-serve` binary and the round-trip test until that impl exists.
//! See `docs/superpowers/specs/2026-06-21-telco-x-runtime-dispatch-design.md`.

use std::sync::Arc;

use anyhow::Result;
use async_nats::HeaderMap;
use async_trait::async_trait;
use greentic_types::{DispatchError, RuntimeDispatchRequest, RuntimeDispatchResponse};
use serde_json::{Value, json};

// Re-export the shared subject helpers so callers can name the telco-x subjects
// without an extra greentic-types dependency.
pub use greentic_types::{request_topic, response_topic};

/// Runtime name for Telco-X; selects the request/response subjects
/// `greentic.telco-x.request.v1` / `greentic.telco-x.response.v1`.
pub const RUNTIME_NAME: &str = "telco-x";

/// Result of invoking the local Telco-X runtime for one dispatch.
pub struct InvokeOutcome {
    /// Whether the operation completed successfully.
    pub ok: bool,
    /// Operation output payload (operation-defined).
    pub output: Value,
    /// Optional runtime-emitted events (empty today).
    pub events: Vec<Value>,
}

/// Seam over the actual Telco-X invocation. A production impl wraps the Telco-X
/// solution layer; the round-trip test and the `telco-x-serve` placeholder use
/// [`EchoInvoker`].
///
/// `target` is the Telco-X resource/target the `telco-x.call.<target>` node
/// addresses; `operation` selects the behaviour (e.g. `provision`, `lookup`,
/// `playbook.step` — the catalogue is Telco-X-owned and out of scope here);
/// `input` is the opaque node input.
#[async_trait]
pub trait TelcoXDispatchInvoker: Send + Sync {
    /// Run one Telco-X operation.
    ///
    /// * `tenant` / `env` — multi-tenant context echoed from the request headers.
    /// * `target` — the Telco-X target/resource id.
    /// * `operation` — the operation name (may be empty for a default).
    /// * `input` — opaque node input.
    /// * `idempotency_key` — correlation/idempotency hint; doubles as the session
    ///   id when the input carries no explicit one.
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

/// Credit-free placeholder invoker: echoes the request back as a successful
/// outcome. Lets the bridge be exercised end-to-end (round-trip test,
/// `telco-x-serve`) before a real Telco-X runtime is wired through the seam.
pub struct EchoInvoker;

#[async_trait]
impl TelcoXDispatchInvoker for EchoInvoker {
    async fn invoke(
        &self,
        _tenant: &str,
        _env: &str,
        target: &str,
        operation: &str,
        input: Value,
        _idempotency_key: Option<&str>,
    ) -> Result<InvokeOutcome> {
        Ok(InvokeOutcome {
            ok: true,
            output: json!({
                "echo": true,
                "target": target,
                "operation": operation,
                "input": input,
            }),
            events: vec![],
        })
    }
}

/// Invoke and build the response (no NATS I/O). Errors map to an error response.
pub async fn build_response(
    invoker: Arc<dyn TelcoXDispatchInvoker>,
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
/// The correlation id is echoed VERBATIM (the runner's `telco-x.call` node
/// encodes `::pack=…::flow=…::thread=…::reply=…` resume markers there and parses
/// them back on response, so the bridge must not alter it).
pub async fn handle_message(
    client: &async_nats::Client,
    invoker: Arc<dyn TelcoXDispatchInvoker>,
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

/// Subscribe to `greentic.telco-x.request.v1` and serve forever (one spawned
/// task per message).
pub async fn run_bridge(
    client: async_nats::Client,
    invoker: Arc<dyn TelcoXDispatchInvoker>,
) -> Result<()> {
    use futures_util::StreamExt;
    let mut subscriber = client.subscribe(request_topic(RUNTIME_NAME)).await?;
    tracing::info!(
        subject = %request_topic(RUNTIME_NAME),
        "telco-x event bridge listening"
    );
    while let Some(msg) = subscriber.next().await {
        let client = client.clone();
        let invoker = invoker.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_message(&client, invoker, msg).await {
                tracing::error!(%error, "telco-x event bridge failed to handle request");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::DispatchMode;
    use std::sync::Mutex;

    type SeenCall = (String, String, Value, Option<String>);

    struct StubInvoker {
        seen: Mutex<Vec<SeenCall>>,
    }

    #[async_trait]
    impl TelcoXDispatchInvoker for StubInvoker {
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
                output: json!({"done": true}),
                events: vec![],
            })
        }
    }

    fn sample_request() -> RuntimeDispatchRequest {
        RuntimeDispatchRequest {
            target: "line-1".into(),
            operation: "provision".into(),
            mode: DispatchMode::Await,
            input: json!({"msisdn": "+100"}),
            deadline_ms: Some(30_000),
        }
    }

    #[test]
    fn subjects_use_telco_x_runtime_name() {
        assert_eq!(request_topic(RUNTIME_NAME), "greentic.telco-x.request.v1");
        assert_eq!(response_topic(RUNTIME_NAME), "greentic.telco-x.response.v1");
    }

    #[tokio::test]
    async fn handle_invokes_and_forwards_target_operation_and_idempotency() {
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
        assert_eq!(resp.output["done"], json!(true));
        assert!(resp.error.is_none());

        let seen = invoker.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (target, operation, input, idempotency) = &seen[0];
        assert_eq!(target, "line-1");
        assert_eq!(operation, "provision");
        assert_eq!(input["msisdn"], json!("+100"));
        assert_eq!(idempotency.as_deref(), Some("sess-1::pack=p::flow=f"));
    }

    #[tokio::test]
    async fn echo_invoker_returns_request_shape() {
        let resp = build_response(
            Arc::new(EchoInvoker),
            "acme",
            "prod",
            Some("c"),
            sample_request(),
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.output["echo"], json!(true));
        assert_eq!(resp.output["target"], json!("line-1"));
        assert_eq!(resp.output["operation"], json!("provision"));
    }

    #[tokio::test]
    async fn invoke_error_maps_to_error_response() {
        struct FailInvoker;

        #[async_trait]
        impl TelcoXDispatchInvoker for FailInvoker {
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
