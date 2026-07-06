//! Billing-metering sink: fire-and-forget emit of live-worker LLM token usage
//! to the cloud-commerce metering API. Mirrors the `TokenMeter` pattern; the
//! default impl is a no-op so the runtime works without billing configured.
//!
//! Activation: set both `GREENTIC_BILLING_BASE_URL` and
//! `GREENTIC_BILLING_SERVICE_SECRET` in the environment; the runner host calls
//! [`HttpBillingMeter::from_env`] and installs it via
//! [`crate::AgentRuntime::with_billing_meter`].
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::tenant::TenantContext;

const BUDGET_TTL: Duration = Duration::from_secs(30);

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
///
/// `over_budget` is a synchronous pre-call gate: it returns `true` ONLY when
/// cloud-commerce definitively reports `available <= 0`. Any error, timeout, or
/// parse failure must return `false` (fail-open) so a billing outage never
/// halts real work. Implementations should cache the result per tenant with a
/// short TTL to avoid a round-trip on every LLM call.
pub trait BillingMeter: Send + Sync {
    fn emit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        input_tokens: u64,
        output_tokens: u64,
        agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BillingError>> + Send + 'a>>;

    fn over_budget<'a>(
        &'a self,
        tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// Returns `true` only when cloud-commerce DEFINITIVELY reports no credits.
/// Pure helper so it can be unit-tested without HTTP.
pub(crate) fn decide_over_budget(available: i64) -> bool {
    available <= 0
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

    fn over_budget<'a>(
        &'a self,
        _tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(std::future::ready(false))
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
///
/// `over_budget` performs a synchronous GET to `/v1/tenants/{id}/wallet` and
/// caches the result per tenant for [`BUDGET_TTL`]. Any error returns `false`
/// (fail-open). The same HTTP client (5s timeout) bounds blocking time.
pub struct HttpBillingMeter {
    base_url: String,
    bearer: String,
    http: reqwest::Client,
    /// Per-tenant TTL cache: tenant_id → (checked_at, is_over_budget).
    over_budget_cache: Mutex<HashMap<String, (Instant, bool)>>,
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
            over_budget_cache: Mutex::new(HashMap::new()),
        })
    }

    /// GET `{base}/v1/tenants/{tenant_id}/wallet` and parse the `available`
    /// field (string) to i64. Returns `None` on any error (fail-open).
    async fn fetch_available(&self, tenant_id: &str) -> Option<i64> {
        let url = format!("{}/v1/tenants/{tenant_id}/wallet", self.base_url);
        let res = self
            .http
            .get(&url)
            .header("x-greentic-service-auth", format!("Bearer {}", self.bearer))
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
        }
        let v: serde_json::Value = res.json().await.ok()?;
        v.get("available")?.as_str()?.parse::<i64>().ok()
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

    fn over_budget<'a>(
        &'a self,
        tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let tenant_id = &tenant.tenant_id;

            // Check cache first; guard dropped before any await point.
            {
                if let Ok(guard) = self.over_budget_cache.lock()
                    && let Some(&(at, over)) = guard.get(tenant_id.as_str())
                    && at.elapsed() < BUDGET_TTL
                {
                    return over;
                }
            }

            // Fetch wallet balance; fail-open on any error.
            let over = match self.fetch_available(tenant_id).await {
                Some(available) => decide_over_budget(available),
                None => false,
            };

            if let Ok(mut guard) = self.over_budget_cache.lock() {
                guard.insert(tenant_id.clone(), (Instant::now(), over));
            }
            over
        })
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

    #[tokio::test]
    async fn noop_over_budget_is_always_false() {
        let m = NoopBillingMeter;
        let t = TenantContext::new("acme", "prod");
        assert!(!m.over_budget(&t).await);
    }

    #[test]
    fn decide_over_budget_positive_is_false() {
        assert!(!decide_over_budget(1));
        assert!(!decide_over_budget(100));
    }

    #[test]
    fn decide_over_budget_zero_is_true() {
        assert!(decide_over_budget(0));
    }

    #[test]
    fn decide_over_budget_negative_is_true() {
        assert!(decide_over_budget(-1));
        assert!(decide_over_budget(-999));
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
