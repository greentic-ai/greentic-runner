use anyhow::Result;
use once_cell::sync::OnceCell;
use rand::{rng, Rng};
use tracing::Span;

use greentic_telemetry::{CloudCtx, TelemetryInit};

use crate::config::HostConfig;

const CONTEXT_KEYS: &[&str] = &["action", "tool", "node_id"];

pub fn init(_config: &HostConfig) -> Result<()> {
    let init = TelemetryInit {
        service_name: "greentic-runner",
        service_version: env!("CARGO_PKG_VERSION"),
        deployment_env: deployment_env(),
    };

    greentic_telemetry::init(init, CONTEXT_KEYS)?;
    Ok(())
}

fn deployment_env() -> &'static str {
    static ENV: OnceCell<String> = OnceCell::new();
    ENV.get_or_init(|| std::env::var("DEPLOYMENT_ENV").unwrap_or_else(|_| "dev".to_owned()))
        .as_str()
}

#[derive(Debug, Clone)]
pub struct FlowSpanAttributes<'a> {
    pub tenant: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub action: Option<&'a str>,
}

pub fn annotate_span(span: &Span, attrs: &FlowSpanAttributes<'_>) {
    span.record("tenant", attrs.tenant);
    span.record("flow_id", attrs.flow_id);
    if let Some(node) = attrs.node_id {
        span.record("node_id", node);
    }
    if let Some(tool) = attrs.tool {
        span.record("tool", tool);
    }
    if let Some(action) = attrs.action {
        span.record("action", action);
    }
}

pub fn set_flow_context(tenant: &str, flow_id: Option<&str>) {
    greentic_telemetry::set_context(CloudCtx {
        tenant: Some(tenant),
        team: None,
        flow: flow_id,
        run_id: None,
    });
}

pub fn backoff_delay_ms(base: u64, attempt: u32) -> u64 {
    let multiplier = 1_u64 << attempt.min(10);
    let exp = base.saturating_mul(multiplier);
    let mut rng = rng();
    let jitter = rng.random_range(0..=exp.min(1000));
    exp + jitter
}
