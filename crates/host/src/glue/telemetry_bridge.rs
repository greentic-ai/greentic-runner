use crate::newrunner::error::GResult;
use crate::newrunner::host::{SpanContext, TelemetryHost};
use async_trait::async_trait;
use std::sync::Arc;

pub struct FnTelemetryHost {
    inner: Arc<dyn Fn(&SpanContext, &[(&str, &str)]) -> GResult<()> + Send + Sync>,
}

impl FnTelemetryHost {
    pub fn new<F>(func: F) -> Self
    where
        F: Send + Sync + 'static + Fn(&SpanContext, &[(&str, &str)]) -> GResult<()>,
    {
        Self {
            inner: Arc::new(func),
        }
    }
}

#[async_trait]
impl TelemetryHost for FnTelemetryHost {
    async fn emit(&self, span: &SpanContext, fields: &[(&str, &str)]) -> GResult<()> {
        (self.inner)(span, fields)
    }
}
