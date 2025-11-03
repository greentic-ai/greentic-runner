use anyhow::Result;
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use rand::{rng, Rng};
use std::sync::Arc;
use tracing::Span;
use tracing_subscriber::prelude::*;

use greentic_telemetry::{init_telemetry, CtxLayer, TelemetryConfig, TelemetryCtx};

use crate::config::HostConfig;

static FLOW_CONTEXT: OnceCell<Arc<RwLock<TelemetryCtx>>> = OnceCell::new();

pub fn init(_config: &HostConfig) -> Result<()> {
    init_telemetry(TelemetryConfig {
        service_name: "greentic-runner".to_string(),
    })?;

    let store = FLOW_CONTEXT
        .get_or_init(|| Arc::new(RwLock::new(TelemetryCtx::default())))
        .clone();
    let ctx_layer = CtxLayer::new({
        let store = Arc::clone(&store);
        move || store.read().clone()
    });

    if !tracing::dispatcher::has_been_set() {
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer())
            .with(ctx_layer)
            .try_init();
    } else {
        tracing::trace!(
            "tracing subscriber already configured; skipping telemetry subscriber init"
        );
    }

    Ok(())
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
    let store = FLOW_CONTEXT.get_or_init(|| Arc::new(RwLock::new(TelemetryCtx::default())));
    let mut guard = store.write();
    let mut ctx = TelemetryCtx::default().with_tenant(tenant.to_string());
    ctx = ctx.with_flow_opt(flow_id.map(|id| id.to_string()));
    *guard = ctx;
}

pub fn backoff_delay_ms(base: u64, attempt: u32) -> u64 {
    let multiplier = 1_u64 << attempt.min(10);
    let exp = base.saturating_mul(multiplier);
    let mut rng = rng();
    let jitter = rng.random_range(0..=exp.min(1000));
    exp + jitter
}
