//! OpenTelemetry span emission. MVP emits exactly one span per step
//! (`aw.step`) with the attributes required by spec §5.2. Per-LLM-call
//! and per-tool-call spans are deferred (spec §4 Decision 11).

use std::time::Duration;

use crate::error::TerminationReason;

pub trait Telemetry: Send + Sync {
    fn record_step(&self, ctx: &StepTelemetryCtx);
}

#[derive(Clone, Debug)]
pub struct StepTelemetryCtx {
    pub tenant_id: String,
    pub env_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub terminated_by: TerminationReason,
    pub iterations: u32,
    pub total_tokens: u64,
    pub duration: Duration,
}

/// Default OTel impl. Emits a `tracing::info_span!` named `aw.step`
/// with the required attributes. `greentic-telemetry` wires this into
/// the OTel collector automatically when its subscriber is active.
pub struct OtelTelemetry;

impl Telemetry for OtelTelemetry {
    fn record_step(&self, ctx: &StepTelemetryCtx) {
        #[allow(clippy::cast_possible_truncation)]
        let span = tracing::info_span!(
            "aw.step",
            tenant_id     = %ctx.tenant_id,
            env_id        = %ctx.env_id,
            session_id    = %ctx.session_id,
            agent_id      = %ctx.agent_id,
            iterations    = ctx.iterations,
            total_tokens  = ctx.total_tokens,
            duration_ms   = ctx.duration.as_millis() as u64,
            terminated_by = ?ctx.terminated_by,
        );
        let _enter = span.enter();
        tracing::info!("aw.step completed");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct CapturingTelemetry(Arc<Mutex<Vec<StepTelemetryCtx>>>);

    impl Telemetry for CapturingTelemetry {
        fn record_step(&self, ctx: &StepTelemetryCtx) {
            self.0.lock().unwrap().push(ctx.clone());
        }
    }

    #[test]
    fn record_step_invokes_telemetry_with_context() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let t = CapturingTelemetry(captured.clone());
        let ctx = StepTelemetryCtx {
            tenant_id: "acme".into(),
            env_id: "prod".into(),
            session_id: "sess".into(),
            agent_id: "a".into(),
            terminated_by: TerminationReason::FinalReply,
            iterations: 3,
            total_tokens: 742,
            duration: Duration::from_millis(1200),
        };
        t.record_step(&ctx);
        let log = captured.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].iterations, 3);
        assert_eq!(log[0].total_tokens, 742);
    }
}
