use std::collections::HashMap;
use std::env;
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use greentic_flow::ir::{FlowIR, NodeIR};
use greentic_mcp::{ExecConfig, ExecRequest};
use greentic_types::TenantCtx as TypesTenantCtx;
use handlebars::Handlebars;
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value, json};
use tokio::task;

use super::mocks::MockLayer;
use crate::config::{HostConfig, McpRetryConfig};
use crate::pack::{FlowDescriptor, PackRuntime};
use crate::telemetry::{
    FlowSpanAttributes, annotate_span, backoff_delay_ms, set_flow_context, tenant_context,
};

pub struct FlowEngine {
    pack: Arc<PackRuntime>,
    flows: Vec<FlowDescriptor>,
    flow_ir: RwLock<HashMap<String, FlowIR>>,
    exec_config: ExecConfig,
    template_engine: Arc<Handlebars<'static>>,
    default_env: String,
}

impl FlowEngine {
    pub async fn new(pack: Arc<PackRuntime>, config: Arc<HostConfig>) -> Result<Self> {
        let flows = pack.list_flows().await?;
        for flow in &flows {
            tracing::info!(flow_id = %flow.id, flow_type = %flow.flow_type, "registered flow");
        }

        let exec_config = config
            .mcp_exec_config()
            .context("failed to build MCP executor config")?;

        let mut ir_map = HashMap::new();
        for flow in &flows {
            let pack_clone = Arc::clone(&pack);
            let flow_id = flow.id.clone();
            let task_flow_id = flow_id.clone();
            match task::spawn_blocking(move || pack_clone.load_flow_ir(&task_flow_id)).await {
                Ok(Ok(ir)) => {
                    ir_map.insert(flow_id, ir);
                }
                Ok(Err(err)) => {
                    tracing::warn!(flow_id = %flow.id, error = %err, "failed to load flow metadata");
                }
                Err(err) => {
                    tracing::warn!(flow_id = %flow.id, error = %err, "join error loading flow metadata");
                }
            }
        }

        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(false);

        Ok(Self {
            pack,
            flows,
            flow_ir: RwLock::new(ir_map),
            exec_config,
            template_engine: Arc::new(handlebars),
            default_env: env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string()),
        })
    }

    async fn get_or_load_flow_ir(&self, flow_id: &str) -> Result<FlowIR> {
        if let Some(ir) = self.flow_ir.read().get(flow_id).cloned() {
            return Ok(ir);
        }

        let pack = Arc::clone(&self.pack);
        let flow_id_owned = flow_id.to_string();
        let task_flow_id = flow_id_owned.clone();
        let ir = task::spawn_blocking(move || pack.load_flow_ir(&task_flow_id))
            .await
            .context("failed to join flow metadata task")??;
        self.flow_ir
            .write()
            .insert(flow_id_owned.clone(), ir.clone());
        Ok(ir)
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
        set_flow_context(
            &self.default_env,
            ctx.tenant,
            ctx.flow_id,
            ctx.node_id,
            ctx.provider_id,
            ctx.session_id,
        );
        let retry_config = ctx.retry_config;
        let original_input = input;
        async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                match self.execute_once(&ctx, original_input.clone()).await {
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
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
            }
        }
        .instrument(span)
        .await
    }

    async fn execute_once(&self, ctx: &FlowContext<'_>, input: Value) -> Result<Value> {
        let flow_ir = self.get_or_load_flow_ir(ctx.flow_id).await?;

        let mut state = ExecutionState::new(input);
        let mut current = flow_ir
            .start
            .clone()
            .or_else(|| flow_ir.nodes.keys().next().cloned())
            .with_context(|| format!("flow {} has no start node", flow_ir.id))?;

        loop {
            let node = flow_ir
                .nodes
                .get(&current)
                .with_context(|| format!("node {current} not found"))?;

            let context_value = state.context();
            let payload = resolve_template_value(
                self.template_engine.as_ref(),
                &node.payload_expr,
                &context_value,
            )?;
            let observed_payload = payload.clone();
            let node_id = current.clone();
            let event = NodeEvent {
                context: ctx,
                node_id: &node_id,
                node,
                payload: &observed_payload,
            };
            if let Some(observer) = ctx.observer {
                observer.on_node_start(&event);
            }
            let dispatch = self
                .dispatch_node(ctx, &current, node, &state, payload)
                .await;
            let output = match dispatch {
                Ok(output) => {
                    if let Some(observer) = ctx.observer {
                        observer.on_node_end(&event, &output.payload);
                    }
                    output
                }
                Err(err) => {
                    if let Some(observer) = ctx.observer {
                        observer.on_node_error(&event, err.as_ref());
                    }
                    return Err(err);
                }
            };

            state.nodes.insert(current.clone(), output.clone());

            let mut next = None;
            for route in &node.routes {
                if route.out || matches!(route.to.as_deref(), Some("out")) {
                    return Ok(output.payload);
                }
                if let Some(to) = &route.to {
                    next = Some(to.clone());
                    break;
                }
            }

            match next {
                Some(n) => current = n,
                None => return Ok(output.payload),
            }
        }
    }

    async fn dispatch_node(
        &self,
        ctx: &FlowContext<'_>,
        _node_id: &str,
        node: &NodeIR,
        state: &ExecutionState,
        payload: Value,
    ) -> Result<NodeOutput> {
        match node.component.as_str() {
            "qa.process" => Ok(NodeOutput::new(payload)),
            "mcp.exec" => self.execute_mcp(ctx, payload).await,
            "templating.handlebars" => self.execute_template(state, payload),
            component if component.starts_with("emit") => Ok(NodeOutput::new(payload)),
            other => bail!("unsupported node component: {other}"),
        }
    }

    async fn execute_mcp(&self, ctx: &FlowContext<'_>, payload: Value) -> Result<NodeOutput> {
        #[derive(Deserialize)]
        struct McpPayload {
            component: String,
            action: String,
            #[serde(default)]
            args: Value,
        }

        let payload: McpPayload =
            serde_json::from_value(payload).context("invalid payload for mcp.exec node")?;

        if let Some(mocks) = ctx.mocks
            && let Some(result) = mocks.tool_short_circuit(&payload.component, &payload.action)
        {
            let value = result.map_err(|err| anyhow!(err))?;
            return Ok(NodeOutput::new(value));
        }

        let request = ExecRequest {
            component: payload.component,
            action: payload.action,
            args: payload.args,
            tenant: Some(types_tenant_ctx(ctx, &self.default_env)),
        };

        let exec_config = self.exec_config.clone();
        let exec_result = task::spawn_blocking(move || greentic_mcp::exec(request, &exec_config))
            .await
            .context("failed to join mcp.exec")?;
        let value = exec_result.map_err(|err| anyhow!(err))?;

        Ok(NodeOutput::new(value))
    }

    fn execute_template(&self, state: &ExecutionState, payload: Value) -> Result<NodeOutput> {
        let payload: TemplatePayload = serde_json::from_value(payload)
            .context("invalid payload for templating.handlebars node")?;

        let mut context = state.context();
        if !payload.data.is_null() {
            let data =
                resolve_template_value(self.template_engine.as_ref(), &payload.data, &context)?;
            merge_values(&mut context, data);
        }

        let rendered = render_template(
            self.template_engine.as_ref(),
            &payload.template,
            &payload.partials,
            &context,
        )?;

        Ok(NodeOutput::new(json!({ "text": rendered })))
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

pub trait ExecutionObserver: Send + Sync {
    fn on_node_start(&self, event: &NodeEvent<'_>);
    fn on_node_end(&self, event: &NodeEvent<'_>, output: &Value);
    fn on_node_error(&self, event: &NodeEvent<'_>, error: &dyn StdError);
}

pub struct NodeEvent<'a> {
    pub context: &'a FlowContext<'a>,
    pub node_id: &'a str,
    pub node: &'a NodeIR,
    pub payload: &'a Value,
}

struct ExecutionState {
    input: Value,
    nodes: HashMap<String, NodeOutput>,
}

impl ExecutionState {
    fn new(input: Value) -> Self {
        Self {
            input,
            nodes: HashMap::new(),
        }
    }

    fn context(&self) -> Value {
        let mut nodes = JsonMap::new();
        for (id, output) in &self.nodes {
            nodes.insert(
                id.clone(),
                json!({
                    "ok": output.ok,
                    "payload": output.payload.clone(),
                    "meta": output.meta.clone(),
                }),
            );
        }
        json!({
            "input": self.input.clone(),
            "nodes": nodes,
        })
    }
}

#[derive(Clone)]
struct NodeOutput {
    ok: bool,
    payload: Value,
    meta: Value,
}

impl NodeOutput {
    fn new(payload: Value) -> Self {
        Self {
            ok: true,
            payload,
            meta: Value::Null,
        }
    }
}

#[derive(Deserialize)]
struct TemplatePayload {
    template: String,
    #[serde(default)]
    partials: HashMap<String, String>,
    #[serde(default)]
    data: Value,
}

fn resolve_template_value(
    engine: &Handlebars<'static>,
    value: &Value,
    context: &Value,
) -> Result<Value> {
    match value {
        Value::String(s) => {
            if s.contains("{{") {
                let rendered = engine
                    .render_template(s, context)
                    .with_context(|| format!("failed to render template: {s}"))?;
                Ok(Value::String(rendered))
            } else {
                Ok(Value::String(s.clone()))
            }
        }
        Value::Array(items) => {
            let values = items
                .iter()
                .map(|v| resolve_template_value(engine, v, context))
                .collect::<Result<Vec<_>>>()?;
            Ok(Value::Array(values))
        }
        Value::Object(map) => {
            let mut resolved = JsonMap::new();
            for (key, v) in map {
                resolved.insert(key.clone(), resolve_template_value(engine, v, context)?);
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other.clone()),
    }
}

fn merge_values(target: &mut Value, addition: Value) {
    match (target, addition) {
        (Value::Object(target_map), Value::Object(add_map)) => {
            for (key, value) in add_map {
                merge_values(target_map.entry(key).or_insert(Value::Null), value);
            }
        }
        (slot, value) => {
            *slot = value;
        }
    }
}

fn render_template(
    base: &Handlebars<'static>,
    template: &str,
    partials: &HashMap<String, String>,
    context: &Value,
) -> Result<String> {
    let mut engine = base.clone();
    for (name, body) in partials {
        engine
            .register_template_string(name, body)
            .with_context(|| format!("failed to register partial {name}"))?;
    }
    engine
        .render_template(template, context)
        .with_context(|| "failed to render template")
}

fn types_tenant_ctx(ctx: &FlowContext<'_>, default_env: &str) -> TypesTenantCtx {
    tenant_context(
        default_env,
        ctx.tenant,
        Some(ctx.flow_id),
        ctx.node_id,
        ctx.provider_id,
        ctx.session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn templating_renders_with_partials_and_data() {
        let mut state = ExecutionState::new(json!({ "city": "London" }));
        state.nodes.insert(
            "forecast".to_string(),
            NodeOutput::new(json!({ "temp": "20C" })),
        );

        let mut partials = HashMap::new();
        partials.insert(
            "line".to_string(),
            "Weather in {{input.city}}: {{nodes.forecast.payload.temp}} {{extra.note}}".to_string(),
        );

        let payload = TemplatePayload {
            template: "{{> line}}".to_string(),
            partials,
            data: json!({ "extra": { "note": "today" } }),
        };

        let mut base = Handlebars::new();
        base.set_strict_mode(false);

        let mut context = state.context();
        let data = resolve_template_value(&base, &payload.data, &context).unwrap();
        merge_values(&mut context, data);
        let rendered =
            render_template(&base, &payload.template, &payload.partials, &context).unwrap();

        assert_eq!(rendered, "Weather in London: 20C today");
    }
}

use tracing::Instrument;

pub struct FlowContext<'a> {
    pub tenant: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub action: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub retry_config: RetryConfig,
    pub observer: Option<&'a dyn ExecutionObserver>,
    pub mocks: Option<&'a MockLayer>,
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
