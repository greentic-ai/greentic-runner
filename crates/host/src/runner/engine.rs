use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::config::McpRetryConfig;
use crate::pack::{FlowDescriptor, PackRuntime};
use crate::telemetry::{annotate_span, backoff_delay_ms, set_flow_context, FlowSpanAttributes};

pub struct FlowEngine {
    pack: Arc<PackRuntime>,
    flows: Vec<FlowDescriptor>,
}

impl FlowEngine {
    pub async fn new(pack: Arc<PackRuntime>) -> Result<Self> {
        let flows = pack.list_flows().await?;
        for flow in &flows {
            tracing::info!(flow_id = %flow.id, flow_type = %flow.flow_type, "registered flow");
        }
        Ok(Self { pack, flows })
    }

    pub async fn execute(&self, ctx: FlowContext<'_>, input: Value) -> Result<Value> {
        let span = tracing::info_span!(
            "flow.execute",
            tenant = tracing::field::Empty,
            flow_id = tracing::field::Empty,
            node_id = tracing::field::Empty,
            tool = tracing::field::Empty,
            action = tracing::field::Empty
        );
        annotate_span(
            &span,
            &FlowSpanAttributes {
                tenant: ctx.tenant,
                flow_id: ctx.flow_id,
                node_id: ctx.node_id,
                tool: ctx.tool,
                action: ctx.action,
            },
        );
        set_flow_context(ctx.tenant, Some(ctx.flow_id));
        let retry_config = ctx.retry_config;
        async move {
            let payload = input;
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self.pack.run_flow(ctx.flow_id, payload.clone()).await {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        if attempt >= retry_config.max_attempts || !should_retry(&err) {
                            return Err(err);
                        }

                        let delay = backoff_delay_ms(retry_config.base_delay_ms, attempt - 1);
                        tracing::warn!(
                            tenant = ctx.tenant,
                            flow_id = ctx.flow_id,
                            attempt,
                            max_attempts = retry_config.max_attempts,
                            delay_ms = delay,
                            error = %err,
                            "transient flow execution failure, backing off"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    pub fn flows(&self) -> &[FlowDescriptor] {
        &self.flows
    }

    pub fn flow_by_type(&self, flow_type: &str) -> Option<&FlowDescriptor> {
        self.flows
            .iter()
            .find(|descriptor| descriptor.flow_type == flow_type)
    }

    pub fn flow_by_id(&self, flow_id: &str) -> Option<&FlowDescriptor> {
        self.flows
            .iter()
            .find(|descriptor| descriptor.id == flow_id)
    }
}

use tracing::Instrument;

pub struct FlowContext<'a> {
    pub tenant: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub action: Option<&'a str>,
    pub retry_config: RetryConfig,
}

#[derive(Copy, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
}

fn should_retry(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    lower.contains("transient") || lower.contains("unavailable") || lower.contains("internal")
}

impl From<McpRetryConfig> for RetryConfig {
    fn from(value: McpRetryConfig) -> Self {
        Self {
            max_attempts: value.max_attempts.max(1),
            base_delay_ms: value.base_delay_ms.max(50),
        }
    }
}
