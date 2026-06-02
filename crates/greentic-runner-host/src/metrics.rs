//! Lightweight wrappers around the global OpenTelemetry meter so engine code
//! can record metrics without each call site having to know about
//! [`opentelemetry`] types.
//!
//! When no meter provider is installed (e.g. unit tests, or
//! `gtc start` without a `telemetry:` block) the no-op global meter
//! short-circuits each call, so these helpers are safe to invoke
//! unconditionally.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

static METER: OnceLock<Meter> = OnceLock::new();
static FLOW_EXEC_COUNT: OnceLock<Counter<u64>> = OnceLock::new();
static FLOW_EXEC_DURATION: OnceLock<Histogram<f64>> = OnceLock::new();
static PROVIDER_INVOKE_COUNT: OnceLock<Counter<u64>> = OnceLock::new();
static PROVIDER_OP_DURATION: OnceLock<Histogram<f64>> = OnceLock::new();

fn meter() -> &'static Meter {
    METER.get_or_init(|| global::meter("greentic-runner-host"))
}

fn flow_count() -> &'static Counter<u64> {
    FLOW_EXEC_COUNT.get_or_init(|| {
        meter()
            .u64_counter("greentic.flow.executions")
            .with_description("Total greentic flow executions")
            .build()
    })
}

fn flow_duration() -> &'static Histogram<f64> {
    FLOW_EXEC_DURATION.get_or_init(|| {
        meter()
            .f64_histogram("greentic.flow.duration_ms")
            .with_description("Duration of greentic flow executions")
            .with_unit("ms")
            .build()
    })
}

fn provider_count() -> &'static Counter<u64> {
    PROVIDER_INVOKE_COUNT.get_or_init(|| {
        meter()
            .u64_counter("greentic.provider.invocations")
            .with_description("Total greentic provider invocations")
            .build()
    })
}

fn provider_op_duration() -> &'static Histogram<f64> {
    PROVIDER_OP_DURATION.get_or_init(|| {
        meter()
            .f64_histogram("greentic.provider.op_duration_ms")
            .with_description("Duration of greentic provider operations")
            .with_unit("ms")
            .build()
    })
}

/// Record one flow execution: increments the executions counter and emits a
/// duration sample. `status` is `"ok"` or `"err"`.
pub fn record_flow_execution(tenant: &str, flow_id: &str, status: &str, duration_ms: f64) {
    let counter_attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("flow_id", flow_id.to_string()),
        KeyValue::new("status", status.to_string()),
    ];
    flow_count().add(1, &counter_attrs);

    let hist_attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("flow_id", flow_id.to_string()),
        KeyValue::new("status", status.to_string()),
    ];
    flow_duration().record(duration_ms, &hist_attrs);
}

/// Record one provider invocation along with its duration. `status` is
/// `"ok"` or `"err"`.
pub fn record_provider_invocation(
    tenant: &str,
    provider: &str,
    op: &str,
    status: &str,
    duration_ms: f64,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("provider", provider.to_string()),
        KeyValue::new("op", op.to_string()),
        KeyValue::new("status", status.to_string()),
    ];
    provider_count().add(1, &attrs);
    provider_op_duration().record(duration_ms, &attrs);
}
