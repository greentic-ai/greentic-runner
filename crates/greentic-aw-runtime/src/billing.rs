//! Billing-metering sink: fire-and-forget emit of live-worker LLM token usage
//! to the cloud-commerce metering API. Mirrors the `TokenMeter` pattern; the
//! default impl is a no-op so the runtime works without billing configured.
//!
//! Activation: set both `GREENTIC_BILLING_BASE_URL` and
//! `GREENTIC_BILLING_SERVICE_SECRET` in the environment; the runner host calls
//! [`HttpBillingMeter::from_env`] and installs it via
//! [`crate::AgentRuntime::with_billing_meter`].
use std::future::Future;
use std::pin::Pin;

use serde::Serialize;

use crate::tenant::TenantContext;

/// Errors the billing sink can return.
///
/// In practice both current implementations always return `Ok(())`; the error
/// variant exists so future implementations can propagate transport failures
/// without a breaking API change.
#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("billing transport error: {0}")]
    Transport(String),
}

/// Dyn-safe billing sink (parallels `TokenMeter`).
///
/// Implementations MUST be fire-and-forget: `emit` should return as quickly as
/// possible and never block the agent step on the billing outcome. The
/// [`HttpBillingMeter`] achieves this by spawning the HTTP POST and returning
/// `Ok(())` immediately.
pub trait BillingMeter: Send + Sync {
    fn emit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        input_tokens: u64,
        output_tokens: u64,
        agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BillingError>> + Send + 'a>>;
}

/// Default: drop everything — billing disabled. Used when
/// `GREENTIC_BILLING_BASE_URL` / `GREENTIC_BILLING_SERVICE_SECRET` are unset.
pub struct NoopBillingMeter;

impl BillingMeter for NoopBillingMeter {
    fn emit<'a>(
        &'a self,
        _tenant: &'a TenantContext,
        _input_tokens: u64,
        _output_tokens: u64,
        _agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BillingError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Batch builder (used by HttpBillingMeter; pub(crate) for unit tests)
// ---------------------------------------------------------------------------

const PRODUCT_ID: &str = "greentic-worker";
type QuantityFn = fn(u64, u64) -> u64;
const METERS: [(&str, QuantityFn); 3] = [
    ("llm_input_tokens", |i, _o| i),
    ("llm_output_tokens", |_i, o| o),
    ("llm_total_tokens", |i, o| i.saturating_add(o)),
];

/// One metering event in the POST body.
#[derive(Debug, Serialize)]
pub(crate) struct WorkerMeteringEvent {
    pub(crate) tenant_id: String,
    pub(crate) product_id: String,
    pub(crate) meter: String,
    pub(crate) quantity: u64,
    pub(crate) unit: String,
    pub(crate) idempotency_key: String,
    pub(crate) timestamp: String,
    pub(crate) metadata: serde_json::Value,
    pub(crate) rating_mode: String,
}

/// Batch payload sent to `/v1/metering/events/batch`.
#[derive(Debug, Serialize)]
pub(crate) struct WorkerMeteringBatch {
    pub(crate) events: Vec<WorkerMeteringEvent>,
}

/// Build the three-event (input / output / total tokens) metering batch for
/// one agent LLM iteration. Pure function; used by [`HttpBillingMeter::emit`]
/// and directly in unit tests.
pub(crate) fn build_worker_batch(
    tenant_id: &str,
    env_id: &str,
    input_tokens: u64,
    output_tokens: u64,
    agent_id: &str,
    idem_base: &str,
) -> WorkerMeteringBatch {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let metadata = serde_json::json!({
        "env_id": env_id,
        "agent_id": agent_id,
        "source": "worker"
    });
    let events = METERS
        .iter()
        .map(|(meter, quantity_fn)| WorkerMeteringEvent {
            tenant_id: tenant_id.to_string(),
            product_id: PRODUCT_ID.to_string(),
            meter: (*meter).to_string(),
            quantity: quantity_fn(input_tokens, output_tokens),
            unit: "token".to_string(),
            idempotency_key: format!("{idem_base}:{meter}"),
            timestamp: timestamp.clone(),
            metadata: metadata.clone(),
            rating_mode: "ingest_only".to_string(),
        })
        .collect();
    WorkerMeteringBatch { events }
}

// ---------------------------------------------------------------------------
// HTTP sink
// ---------------------------------------------------------------------------

/// HTTP sink → cloud-commerce `/v1/metering/events/batch`.
///
/// Fire-and-forget: the POST is spawned on a new Tokio task so `emit` returns
/// `Ok(())` immediately and never adds latency or blocks the agent step on a
/// billing outcome. Transport errors are logged at `WARN` level and swallowed.
pub struct HttpBillingMeter {
    base_url: String,
    bearer: String,
    http: reqwest::Client,
}

impl HttpBillingMeter {
    /// Return `None` (use Noop) when either env var is unset or blank.
    ///
    /// Reads:
    /// - `GREENTIC_BILLING_BASE_URL` — base URL of the cloud-commerce service
    /// - `GREENTIC_BILLING_SERVICE_SECRET` — bearer token for service auth
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("GREENTIC_BILLING_BASE_URL").ok()?;
        let bearer = std::env::var("GREENTIC_BILLING_SERVICE_SECRET").ok()?;
        if base_url.trim().is_empty() || bearer.trim().is_empty() {
            return None;
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        Some(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            bearer,
            http,
        })
    }
}

impl BillingMeter for HttpBillingMeter {
    fn emit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        input_tokens: u64,
        output_tokens: u64,
        agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BillingError>> + Send + 'a>> {
        // Build idempotency key and batch synchronously so we own all data
        // before spawning — the spawned future must be 'static.
        let idem = uuid::Uuid::new_v4().to_string();
        let batch = build_worker_batch(
            &tenant.tenant_id,
            &tenant.env_id,
            input_tokens,
            output_tokens,
            agent_id,
            &idem,
        );
        let url = format!("{}/v1/metering/events/batch", self.base_url);
        let http = self.http.clone();
        let bearer = self.bearer.clone();

        // Detach: spawn independently; never block the agent step on billing.
        tokio::spawn(async move {
            let res = http
                .post(&url)
                .header("x-greentic-service-auth", format!("Bearer {bearer}"))
                .header("x-greentic-metering-source", "worker")
                .json(&batch)
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => tracing::warn!(
                    status = %r.status(),
                    "worker metering rejected; continuing"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "worker metering unreachable; continuing"
                ),
            }
        });

        // Return immediately — the agent step is never blocked on billing.
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tenant::TenantContext;

    #[tokio::test]
    async fn noop_meter_is_ok() {
        let m = NoopBillingMeter;
        let t = TenantContext::new("acme", "prod");
        assert!(m.emit(&t, 10, 5, "agent-1").await.is_ok());
    }

    #[test]
    fn builds_three_worker_token_events() {
        let batch = build_worker_batch("acme", "prod", 10, 5, "agent-1", "idem-1");
        assert_eq!(batch.events.len(), 3);
        let by = |m: &str| batch.events.iter().find(|e| e.meter == m).unwrap();
        assert_eq!(by("llm_input_tokens").quantity, 10);
        assert_eq!(by("llm_output_tokens").quantity, 5);
        assert_eq!(by("llm_total_tokens").quantity, 15);
        assert_eq!(by("llm_input_tokens").product_id, "greentic-worker");
        assert_eq!(
            by("llm_input_tokens").idempotency_key,
            "idem-1:llm_input_tokens"
        );
        assert_eq!(by("llm_input_tokens").metadata["agent_id"], "agent-1");
        assert_eq!(by("llm_input_tokens").metadata["env_id"], "prod");
    }
}
