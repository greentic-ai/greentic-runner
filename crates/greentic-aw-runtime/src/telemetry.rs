// placeholder — filled in subsequent tasks

/// Per-step telemetry context attached to spans (Task 1.8).
#[derive(Clone, Debug)]
pub struct StepTelemetryCtx;

/// Observability sink for agent step events (Task 1.8).
///
/// Uses `Pin<Box<dyn Future>>` returns for dyn-safety behind `Arc<dyn Telemetry>`.
pub trait Telemetry: Send + Sync {
    fn record_step(
        &self,
        ctx: &StepTelemetryCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}
