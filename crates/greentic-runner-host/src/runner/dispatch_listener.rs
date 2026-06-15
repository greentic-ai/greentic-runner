//! Subscribes to runtime response messages on `greentic.<runtime>.response.v1`
//! and resumes the paused flow session whose correlation id matches.
//!
//! The production `SessionResumer` (built separately) synthesizes an ingress
//! envelope and feeds the runtime's resume path; here we only define the seam.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use greentic_types::{EnvId, RuntimeDispatchResponse, TenantCtx, TenantId, response_topic};
use serde_json::Value;
use std::sync::Arc;

/// Decoded resume input extracted from a response message.
pub struct ResumeInput {
    pub tenant: TenantCtx,
    pub correlation_id: String,
    pub output: Value,
}

/// Resume a waiting flow session identified by the opaque `correlation_id`
/// (= canonical session hint), feeding it `output`.
#[async_trait]
pub trait SessionResumer: Send + Sync {
    async fn resume(&self, tenant: TenantCtx, correlation_id: &str, output: Value) -> Result<()>;
}

/// Pure: turn response headers + JSON body into a `ResumeInput`. No I/O.
///
/// # Arguments
/// * `correlation_id` - value of the `Greentic-Correlation-Id` header
/// * `tenant` - value of the `Greentic-Tenant` header (falls back to `"default"`)
/// * `env` - value of the `Greentic-Env` header (falls back to `"default"`)
/// * `payload` - raw JSON bytes of a [`RuntimeDispatchResponse`]
///
/// # Errors
/// Returns an error if `correlation_id` is missing/empty, if the tenant/env
/// identifiers are invalid, or if the payload cannot be parsed.
pub fn decode_response(
    correlation_id: Option<&str>,
    tenant: Option<&str>,
    env: Option<&str>,
    payload: &[u8],
) -> Result<ResumeInput> {
    let correlation_id = correlation_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("dispatch response missing Greentic-Correlation-Id"))?
        .to_string();

    let tenant_str = tenant.filter(|s| !s.is_empty()).unwrap_or("default");
    let env_str = env.filter(|s| !s.is_empty()).unwrap_or("default");

    let tenant_id = TenantId::try_from(tenant_str)
        .map_err(|error| anyhow!("invalid Greentic-Tenant header value: {error}"))?;
    let env_id = EnvId::try_from(env_str)
        .map_err(|error| anyhow!("invalid Greentic-Env header value: {error}"))?;

    let response: RuntimeDispatchResponse = serde_json::from_slice(payload)?;

    let output = serde_json::json!({
        "ok": response.ok,
        "output": response.output,
        "events": response.events,
        "error": response.error,
    });

    Ok(ResumeInput {
        tenant: TenantCtx::new(env_id, tenant_id),
        correlation_id,
        output,
    })
}

/// Forward a decoded input to the resumer, logging failures.
pub async fn dispatch_to_resumer(resumer: Arc<dyn SessionResumer>, input: ResumeInput) {
    if let Err(error) = resumer
        .resume(input.tenant, &input.correlation_id, input.output)
        .await
    {
        tracing::error!(
            %error,
            correlation_id = %input.correlation_id,
            "failed to resume session on dispatch response"
        );
    }
}

/// Subscribe to the runtime's response subject and resume sessions forever.
///
/// Runs until the NATS subscription is closed (e.g. on server shutdown).
/// Malformed messages are logged and dropped; the listener does not stop.
///
/// # Errors
/// Returns an error only if subscribing to the NATS subject fails.
pub async fn run_response_listener(
    client: async_nats::Client,
    runtime: String,
    resumer: Arc<dyn SessionResumer>,
) -> Result<()> {
    let subject = response_topic(&runtime);
    let mut subscription = client.subscribe(subject).await?;
    while let Some(msg) = subscription.next().await {
        let headers = msg.headers.as_ref();
        let get_header = |name: &str| headers.and_then(|h| h.get(name)).map(|v| v.as_str());
        match decode_response(
            get_header("Greentic-Correlation-Id"),
            get_header("Greentic-Tenant"),
            get_header("Greentic-Env"),
            &msg.payload,
        ) {
            Ok(input) => dispatch_to_resumer(resumer.clone(), input).await,
            Err(error) => tracing::warn!(%error, "bad dispatch response; dropping"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{DispatchError, RuntimeDispatchResponse};
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn decode_response_extracts_tenant_correlation_and_output() {
        let body = serde_json::to_vec(&RuntimeDispatchResponse {
            ok: true,
            output: json!({"reply": "hi"}),
            events: vec![],
            error: None,
        })
        .unwrap();
        let decoded = decode_response(
            Some("t1:web:c:conv:u::pack=p"),
            Some("t1"),
            Some("default"),
            &body,
        )
        .unwrap();
        assert_eq!(decoded.correlation_id, "t1:web:c:conv:u::pack=p");
        assert_eq!(decoded.tenant.tenant_id.as_str(), "t1");
        assert_eq!(decoded.output["ok"], json!(true));
        assert_eq!(decoded.output["output"], json!({"reply": "hi"}));
    }

    #[test]
    fn decode_response_errors_without_correlation_id() {
        let body = serde_json::to_vec(&RuntimeDispatchResponse {
            ok: true,
            output: json!(null),
            events: vec![],
            error: None,
        })
        .unwrap();
        assert!(decode_response(None, Some("t1"), Some("default"), &body).is_err());
    }

    struct RecordingResumer {
        seen: Mutex<Vec<(String, serde_json::Value)>>,
    }

    #[async_trait]
    impl SessionResumer for RecordingResumer {
        async fn resume(
            &self,
            _tenant: TenantCtx,
            correlation_id: &str,
            output: Value,
        ) -> Result<()> {
            self.seen
                .lock()
                .unwrap()
                .push((correlation_id.to_string(), output));
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_to_resumer_forwards_decoded_input() {
        let resumer = Arc::new(RecordingResumer {
            seen: Mutex::new(vec![]),
        });
        let body = serde_json::to_vec(&RuntimeDispatchResponse {
            ok: false,
            output: json!(null),
            events: vec![],
            error: Some(DispatchError {
                code: "timeout".into(),
                message: "x".into(),
            }),
        })
        .unwrap();
        let decoded = decode_response(Some("sess-9"), Some("t1"), Some("default"), &body).unwrap();
        dispatch_to_resumer(resumer.clone(), decoded).await;
        let seen = resumer.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "sess-9");
        assert_eq!(seen[0].1["ok"], json!(false));
        assert_eq!(seen[0].1["error"]["code"], json!("timeout"));
    }
}
