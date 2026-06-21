use std::collections::HashMap;
use std::env;
use std::error::Error as StdError;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::component_api::node::{ExecCtx as ComponentExecCtx, TenantCtx as ComponentTenantCtx};
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use tokio::task;

use super::mocks::MockLayer;
use super::templating::{TemplateOptions, render_template_value};
use crate::config::{FlowRetryConfig, HostConfig};
use crate::pack::{FlowDescriptor, PackRuntime};
use crate::runner::invocation::{InvocationMeta, build_invocation_envelope};
use crate::telemetry::{FlowSpanAttributes, annotate_span, backoff_delay_ms, set_flow_context};
#[cfg(feature = "fault-injection")]
use crate::testing::fault_injection::{FaultContext, FaultPoint, maybe_fail};
use crate::validate::{
    ValidationConfig, ValidationIssue, ValidationMode, validate_component_envelope,
    validate_tool_envelope,
};
use greentic_types::{Flow, Node, NodeId, Routing};

/// Callback trait for resolving cross-pack provider invocations.
///
/// When a `provider.invoke` node references a provider that is not in the
/// current pack, the flow engine calls this resolver as a fallback.
/// Implementations typically delegate to a capability registry that knows
/// about all packs in the bundle.
pub trait CrossPackResolver: Send + Sync {
    fn invoke(
        &self,
        provider_id: &str,
        provider_type: Option<&str>,
        op: &str,
        input: &[u8],
        tenant: &str,
        team: Option<&str>,
    ) -> Result<Value>;
}

pub struct FlowEngine {
    packs: Vec<Arc<PackRuntime>>,
    flows: Vec<FlowDescriptor>,
    flow_sources: HashMap<FlowKey, usize>,
    flow_cache: RwLock<HashMap<FlowKey, HostFlow>>,
    default_env: String,
    validation: ValidationConfig,
    cross_pack_resolver: Option<Arc<dyn CrossPackResolver>>,
    /// Bridges `sorla.call` flow nodes into a separate runtime over pub/sub.
    /// Not feature-gated: `sorla.call` is a core runtime-dispatch node.
    remote_dispatch_handler: Option<Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>>,
    /// Controls whether `dw.agent` nodes run in-process (default) or are
    /// rerouted over the durable agentic NATS path (`GREENTIC_AW_DISPATCH=nats`).
    #[cfg(feature = "agentic-worker")]
    dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch,
    #[cfg(feature = "agentic-worker")]
    agent_node_handler: Option<Arc<dyn crate::runner::agent_node::AgentNodeHandler>>,
    #[cfg(feature = "agentic-worker")]
    graph_node_handler: Option<Arc<dyn crate::runner::graph_node::GraphNodeHandler>>,
    /// Per-tenant MCP tool source for `component == "mcp"` flow nodes
    /// (role `flow_editor`). Built once from env so the TTL catalog cache is
    /// shared across nodes/flows. `None` when MCP is unconfigured/opted-out,
    /// in which case MCP nodes fail gracefully with a clear node error.
    #[cfg(feature = "agentic-worker")]
    mcp_tool_source: Option<Arc<greentic_aw_runtime::McpToolSource>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FlowKey {
    pack_id: String,
    flow_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowSnapshot {
    pub pack_id: String,
    pub flow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_flow: Option<String>,
    pub next_node: String,
    pub state: ExecutionState,
}

#[derive(Clone, Debug)]
pub struct FlowWait {
    pub reason: Option<String>,
    pub snapshot: FlowSnapshot,
}

#[derive(Clone, Debug)]
pub enum FlowStatus {
    Completed,
    Waiting(Box<FlowWait>),
}

#[derive(Clone, Debug)]
pub struct FlowExecution {
    pub output: Value,
    pub status: FlowStatus,
}

#[derive(Clone, Debug)]
struct HostFlow {
    id: String,
    start: Option<NodeId>,
    nodes: IndexMap<NodeId, HostNode>,
}

#[derive(Clone, Debug)]
pub struct HostNode {
    kind: NodeKind,
    /// Backwards-compatible component label for observers/transcript.
    pub component: String,
    component_id: String,
    operation_name: Option<String>,
    operation_in_mapping: Option<String>,
    payload_expr: Value,
    routing: Routing,
}

impl HostNode {
    pub fn component_id(&self) -> &str {
        &self.component_id
    }

    pub fn operation_name(&self) -> Option<&str> {
        self.operation_name.as_deref()
    }

    pub fn operation_in_mapping(&self) -> Option<&str> {
        self.operation_in_mapping.as_deref()
    }
}

#[derive(Clone, Debug)]
enum NodeKind {
    Exec {
        target_component: String,
    },
    PackComponent {
        component_ref: String,
    },
    ProviderInvoke,
    FlowCall,
    BuiltinEmit {
        kind: EmitKind,
    },
    BuiltinStateGet,
    BuiltinStateSet,
    Wait,
    DwAgent {
        agent_id: String,
    },
    DwAgentGraph {
        graph_id: String,
    },
    /// Native runtime-dispatch node: publishes work to a separate runtime
    /// (e.g. sorx) via the injected [`RemoteDispatchHandler`]. `target` is the
    /// node operation (the logical runtime target).
    SorlaCall {
        target: String,
    },
    /// Native runtime-dispatch node for the Operala runtime. Mirrors
    /// [`SorlaCall`] but routes to the `"operala"` runtime name.
    OperalaCall {
        target: String,
    },
    /// Native runtime-dispatch node for an out-of-process agentic runtime.
    /// Mirrors [`SorlaCall`] but routes to the `"agentic"` runtime name.
    /// This is an ADDITIONAL path: the in-process `dw.agent` node is
    /// completely separate and untouched.
    AgenticCall {
        target: String,
    },
    /// Native runtime-dispatch node for the Telco-X runtime. Mirrors
    /// [`SorlaCall`] but routes to the `"telco-x"` runtime name. Wire-ready: the
    /// runtime side (a telco-x NATS dispatch service) is not built yet, so an
    /// `await: true` node pauses until that runtime exists.
    TelcoXCall {
        target: String,
    },
    /// Flow-execution MCP node (LOCKED ENCODING v2): `component == "mcp"` with
    /// `server`/`tool` carried in the node payload/config. Invokes the named
    /// MCP tool through the tenant's `flow_editor` MCP catalog (reusing
    /// `greentic-aw-runtime`'s `McpToolSource`). Completely separate from the
    /// agent-loop MCP path (role `agentic_worker`).
    ///
    /// `server_id`/`tool` here are the values resolved at flow-load time;
    /// `execute_mcp` re-reads them from the rendered payload (source of truth)
    /// and only uses these as a fallback for the legacy `operation` encoding.
    Mcp {
        server_id: String,
        tool: String,
    },
}

#[derive(Clone, Debug)]
enum EmitKind {
    Log,
    Response,
    Other(String),
}

struct ComponentOverrides<'a> {
    component: Option<&'a str>,
    operation: Option<&'a str>,
}

struct ComponentCall {
    component_ref: String,
    operation: String,
    input: Value,
    config: Value,
}

impl FlowExecution {
    fn completed(output: Value) -> Self {
        Self {
            output,
            status: FlowStatus::Completed,
        }
    }

    fn waiting(output: Value, wait: FlowWait) -> Self {
        Self {
            output,
            status: FlowStatus::Waiting(Box::new(wait)),
        }
    }
}

impl FlowEngine {
    pub async fn new(packs: Vec<Arc<PackRuntime>>, config: Arc<HostConfig>) -> Result<Self> {
        let mut flow_sources: HashMap<FlowKey, usize> = HashMap::new();
        let mut descriptors = Vec::new();
        let mut bindings = HashMap::new();
        for pack in &config.pack_bindings {
            bindings.insert(pack.pack_id.clone(), pack.flows.clone());
        }
        let enforce_bindings = !bindings.is_empty();
        for (idx, pack) in packs.iter().enumerate() {
            let pack_id = pack.metadata().pack_id.clone();
            if enforce_bindings && !bindings.contains_key(&pack_id) {
                bail!("no gtbind entries found for pack {}", pack_id);
            }
            let flows = pack.list_flows().await?;
            let allowed = bindings.get(&pack_id).map(|flows| {
                flows
                    .iter()
                    .cloned()
                    .collect::<std::collections::HashSet<_>>()
            });
            let mut seen = std::collections::HashSet::new();
            for flow in flows {
                if let Some(ref allow) = allowed
                    && !allow.contains(&flow.id)
                {
                    continue;
                }
                seen.insert(flow.id.clone());
                tracing::info!(
                    flow_id = %flow.id,
                    flow_type = %flow.flow_type,
                    pack_id = %flow.pack_id,
                    pack_index = idx,
                    "registered flow"
                );
                if let Ok(flow_ir) = pack.load_flow(&flow.id) {
                    for node in flow_ir.nodes.values() {
                        config
                            .secrets_policy
                            .register_flow_secret_refs(&node.input.mapping);
                        config
                            .secrets_policy
                            .register_flow_secret_refs(&node.output.mapping);
                    }
                }
                flow_sources.insert(
                    FlowKey {
                        pack_id: flow.pack_id.clone(),
                        flow_id: flow.id.clone(),
                    },
                    idx,
                );
                descriptors.retain(|existing: &FlowDescriptor| {
                    !(existing.id == flow.id && existing.pack_id == flow.pack_id)
                });
                descriptors.push(flow);
            }
            if let Some(allow) = allowed {
                let missing = allow.difference(&seen).cloned().collect::<Vec<_>>();
                if !missing.is_empty() {
                    bail!(
                        "gtbind flow ids missing in pack {}: {}",
                        pack_id,
                        missing.join(", ")
                    );
                }
            }
        }

        let mut flow_map = HashMap::new();
        for flow in &descriptors {
            let pack_id = flow.pack_id.clone();
            if let Some(&pack_idx) = flow_sources.get(&FlowKey {
                pack_id: pack_id.clone(),
                flow_id: flow.id.clone(),
            }) {
                let pack_clone = Arc::clone(&packs[pack_idx]);
                let flow_id = flow.id.clone();
                let task_flow_id = flow_id.clone();
                match task::spawn_blocking(move || pack_clone.load_flow(&task_flow_id)).await {
                    Ok(Ok(loaded_flow)) => {
                        flow_map.insert(
                            FlowKey {
                                pack_id: pack_id.clone(),
                                flow_id,
                            },
                            HostFlow::from(loaded_flow),
                        );
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(flow_id = %flow.id, error = %err, "failed to load flow metadata");
                    }
                    Err(err) => {
                        tracing::warn!(flow_id = %flow.id, error = %err, "join error loading flow metadata");
                    }
                }
            }
        }

        Ok(Self {
            packs,
            flows: descriptors,
            flow_sources,
            flow_cache: RwLock::new(flow_map),
            default_env: env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string()),
            validation: config.validation.clone(),
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: crate::runner::mcp_node::source_from_env(),
        })
    }

    /// Set an optional cross-pack resolver for `provider.invoke` nodes that
    /// reference providers in other packs (resolved via capability registry).
    pub fn set_cross_pack_resolver(&mut self, resolver: Arc<dyn CrossPackResolver>) {
        self.cross_pack_resolver = Some(resolver);
    }

    /// Set the handler that bridges `sorla.call` flow nodes into a separate
    /// runtime over pub/sub. Constructed by the runner binary when a transport
    /// (e.g. NATS) is configured.
    pub fn set_remote_dispatch_handler(
        &mut self,
        handler: Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>,
    ) {
        self.remote_dispatch_handler = Some(handler);
    }

    /// Set the handler that bridges `DwAgent` flow nodes into the agentic-worker
    /// runtime. Constructed by the runner binary (Task 4.3).
    #[cfg(feature = "agentic-worker")]
    pub fn set_agent_node_handler(
        &mut self,
        handler: Arc<dyn crate::runner::agent_node::AgentNodeHandler>,
    ) {
        self.agent_node_handler = Some(handler);
    }

    /// Set the handler that bridges `DwAgentGraph` flow nodes into the durable
    /// graph executor. Constructed by the pack loader (Task 8). Mirrors
    /// [`set_agent_node_handler`].
    ///
    /// [`set_agent_node_handler`]: FlowEngine::set_agent_node_handler
    #[cfg(feature = "agentic-worker")]
    pub fn set_graph_node_handler(
        &mut self,
        handler: Arc<dyn crate::runner::graph_node::GraphNodeHandler>,
    ) {
        self.graph_node_handler = Some(handler);
    }

    /// Set the dispatch mode for `dw.agent` nodes.
    ///
    /// - [`DwAgentDispatch::InProcess`] (default): runs the agent in-process via
    ///   [`AgentNodeHandler`]. Zero configuration overhead; today's behaviour.
    /// - [`DwAgentDispatch::Nats`]: reroutes the node over the durable agentic
    ///   NATS path (`greentic.agentic.request.v1`), identical to an `agentic.call`
    ///   node. Requires [`set_remote_dispatch_handler`] to also be set.
    ///
    /// Called by `runtime.rs` when `GREENTIC_AW_DISPATCH=nats`.
    ///
    /// [`AgentNodeHandler`]: crate::runner::agent_node::AgentNodeHandler
    /// [`set_remote_dispatch_handler`]: FlowEngine::set_remote_dispatch_handler
    #[cfg(feature = "agentic-worker")]
    pub fn set_dw_agent_dispatch(&mut self, mode: crate::runner::agent_node::DwAgentDispatch) {
        self.dw_agent_dispatch = mode;
    }

    async fn get_or_load_flow(&self, pack_id: &str, flow_id: &str) -> Result<HostFlow> {
        let key = FlowKey {
            pack_id: pack_id.to_string(),
            flow_id: flow_id.to_string(),
        };
        if let Some(flow) = self.flow_cache.read().get(&key).cloned() {
            return Ok(flow);
        }

        let pack_idx = *self
            .flow_sources
            .get(&key)
            .with_context(|| format!("flow {pack_id}:{flow_id} not registered"))?;
        let pack = Arc::clone(&self.packs[pack_idx]);
        let flow_id_owned = flow_id.to_string();
        let task_flow_id = flow_id_owned.clone();
        let flow = task::spawn_blocking(move || pack.load_flow(&task_flow_id))
            .await
            .context("failed to join flow metadata task")??;
        let host_flow = HostFlow::from(flow);
        self.flow_cache.write().insert(
            FlowKey {
                pack_id: pack_id.to_string(),
                flow_id: flow_id_owned.clone(),
            },
            host_flow.clone(),
        );
        Ok(host_flow)
    }

    pub async fn execute(&self, ctx: FlowContext<'_>, input: Value) -> Result<FlowExecution> {
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
        let mut ctx = ctx;
        async move {
            let mut attempt = 0u32;
            loop {
                attempt += 1;
                ctx.attempt = attempt;
                #[cfg(feature = "fault-injection")]
                {
                    let fault_ctx = FaultContext {
                        pack_id: ctx.pack_id,
                        flow_id: ctx.flow_id,
                        node_id: ctx.node_id,
                        attempt: ctx.attempt,
                    };
                    maybe_fail(FaultPoint::Timeout, fault_ctx)
                        .map_err(|err| anyhow!(err.to_string()))?;
                }
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

    pub async fn resume(
        &self,
        ctx: FlowContext<'_>,
        snapshot: FlowSnapshot,
        input: Value,
    ) -> Result<FlowExecution> {
        if snapshot.pack_id != ctx.pack_id {
            bail!(
                "snapshot pack {} does not match requested {}",
                snapshot.pack_id,
                ctx.pack_id
            );
        }
        let resume_flow = snapshot
            .next_flow
            .clone()
            .unwrap_or_else(|| snapshot.flow_id.clone());
        let flow_ir = self.get_or_load_flow(ctx.pack_id, &resume_flow).await?;
        let mut state = snapshot.state;
        // Replace BOTH `input` AND `entry` with the new activity. The
        // routing context (built by `build_routing_context`) reads
        // `entry.input.metadata.*` for the synthesised `response.*` fields
        // that conditional routes test against — keeping the snapshot's
        // stale entry would make `response.action` perpetually empty and
        // every condition fail, looping the user back to the wait point
        // forever. `replace_input` only touches `state.input`, so we have
        // to refresh `entry` ourselves; `ensure_entry` is a no-op once
        // entry is non-null.
        state.replace_input(input.clone());
        state.entry = input;
        self.drive_flow(&ctx, flow_ir, state, Some(snapshot.next_node), resume_flow)
            .await
    }

    async fn execute_once(&self, ctx: &FlowContext<'_>, input: Value) -> Result<FlowExecution> {
        let flow_ir = self.get_or_load_flow(ctx.pack_id, ctx.flow_id).await?;
        let state = ExecutionState::new(input);
        self.drive_flow(ctx, flow_ir, state, None, ctx.flow_id.to_string())
            .await
    }

    async fn drive_flow(
        &self,
        ctx: &FlowContext<'_>,
        mut flow_ir: HostFlow,
        mut state: ExecutionState,
        resume_from: Option<String>,
        mut current_flow_id: String,
    ) -> Result<FlowExecution> {
        let mut current = match resume_from {
            Some(node) => NodeId::from_str(&node)
                .with_context(|| format!("invalid resume node id `{node}`"))?,
            None => flow_ir
                .start
                .clone()
                .or_else(|| flow_ir.nodes.keys().next().cloned())
                .with_context(|| format!("flow {} has no start node", flow_ir.id))?,
        };

        loop {
            let step_ctx = FlowContext {
                tenant: ctx.tenant,
                pack_id: ctx.pack_id,
                flow_id: current_flow_id.as_str(),
                node_id: ctx.node_id,
                tool: ctx.tool,
                action: ctx.action,
                session_id: ctx.session_id,
                provider_id: ctx.provider_id,
                reply_scope: ctx.reply_scope,
                retry_config: ctx.retry_config,
                attempt: ctx.attempt,
                observer: ctx.observer,
                mocks: ctx.mocks,
            };
            let node = flow_ir
                .nodes
                .get(&current)
                .with_context(|| format!("node {} not found", current.as_str()))?;

            let payload_template = node.payload_expr.clone();
            let prev = state
                .last_output
                .as_ref()
                .cloned()
                .unwrap_or_else(|| Value::Object(JsonMap::new()));
            let ctx_value = template_context(&state, prev);
            #[cfg(feature = "fault-injection")]
            {
                let fault_ctx = FaultContext {
                    pack_id: ctx.pack_id,
                    flow_id: ctx.flow_id,
                    node_id: Some(current.as_str()),
                    attempt: ctx.attempt,
                };
                maybe_fail(FaultPoint::TemplateRender, fault_ctx)
                    .map_err(|err| anyhow!(err.to_string()))?;
            }
            let payload =
                render_template_value(&payload_template, &ctx_value, TemplateOptions::default())
                    .context("failed to render node input template")?;
            let observed_payload = payload.clone();
            let node_id = current.clone();
            let event = NodeEvent {
                context: &step_ctx,
                node_id: node_id.as_str(),
                node,
                payload: &observed_payload,
            };
            if let Some(observer) = step_ctx.observer {
                observer.on_node_start(&event);
            }
            let dispatch = self
                .dispatch_node(
                    &step_ctx,
                    node_id.as_str(),
                    node,
                    &mut state,
                    payload,
                    &event,
                )
                .await;
            let DispatchOutcome { output, control } = match dispatch {
                Ok(outcome) => outcome,
                Err(err) => {
                    if let Some(observer) = step_ctx.observer {
                        observer.on_node_error(&event, err.as_ref());
                    }
                    return Err(err);
                }
            };

            state.nodes.insert(node_id.clone().into(), output.clone());
            state.last_output = Some(output.payload.clone());
            if let Some(observer) = step_ctx.observer {
                observer.on_node_end(&event, &output.payload);
            }

            match control {
                NodeControl::Continue => {
                    enum NextDecision {
                        Next(NodeId),
                        End,
                        Wait,
                    }
                    let decision = match &node.routing {
                        Routing::Next { node_id } => NextDecision::Next(node_id.clone()),
                        Routing::End | Routing::Reply => NextDecision::End,
                        Routing::Branch { default, .. } => match default {
                            Some(target) => NextDecision::Next(target.clone()),
                            None => NextDecision::End,
                        },
                        Routing::Custom(raw) => {
                            match evaluate_custom_routing(raw, &output, &state, &flow_ir, &node_id)
                            {
                                CustomRoutingDecision::Next(nid) => NextDecision::Next(nid),
                                CustomRoutingDecision::End => NextDecision::End,
                                CustomRoutingDecision::Wait => NextDecision::Wait,
                            }
                        }
                    };

                    match decision {
                        NextDecision::Next(n) => current = n,
                        NextDecision::End => {
                            return Ok(FlowExecution::completed(
                                state.finalize_with(Some(output.payload.clone())),
                            ));
                        }
                        NextDecision::Wait => {
                            // Conditional routing fell through. Pause at the
                            // current node so the next inbound activity
                            // resumes here and re-evaluates this node's
                            // routing with the user's new submit payload.
                            let mut snapshot_state = state.clone();
                            snapshot_state.clear_egress();
                            let snapshot = FlowSnapshot {
                                pack_id: step_ctx.pack_id.to_string(),
                                flow_id: step_ctx.flow_id.to_string(),
                                next_flow: (current_flow_id != step_ctx.flow_id)
                                    .then_some(current_flow_id.clone()),
                                next_node: node_id.as_str().to_string(),
                                state: snapshot_state,
                            };
                            let output_value = state.finalize_with(Some(output.payload.clone()));
                            return Ok(FlowExecution::waiting(
                                output_value,
                                FlowWait {
                                    reason: Some(format!(
                                        "awaiting user submit at node `{}`",
                                        node_id.as_str()
                                    )),
                                    snapshot,
                                },
                            ));
                        }
                    }
                }
                NodeControl::Wait { reason } => {
                    let next: Option<NodeId> = match &node.routing {
                        Routing::Next { node_id } => Some(node_id.clone()),
                        Routing::End | Routing::Reply => None,
                        Routing::Branch { default, .. } => default.clone(),
                        Routing::Custom(raw) => {
                            match evaluate_custom_routing(raw, &output, &state, &flow_ir, &node_id)
                            {
                                CustomRoutingDecision::Next(nid) => Some(nid),
                                // session.wait operator must have an
                                // explicit forward target — both End and
                                // Wait decisions collapse to "no next" and
                                // surface the same error below.
                                CustomRoutingDecision::End | CustomRoutingDecision::Wait => None,
                            }
                        }
                    };
                    let resume_target = next.ok_or_else(|| {
                        anyhow!(
                            "session.wait node {} requires a non-empty route",
                            current.as_str()
                        )
                    })?;
                    let mut snapshot_state = state.clone();
                    snapshot_state.clear_egress();
                    let snapshot = FlowSnapshot {
                        pack_id: step_ctx.pack_id.to_string(),
                        flow_id: step_ctx.flow_id.to_string(),
                        next_flow: (current_flow_id != step_ctx.flow_id)
                            .then_some(current_flow_id.clone()),
                        next_node: resume_target.as_str().to_string(),
                        state: snapshot_state,
                    };
                    let output_value = state.clone().finalize_with(None);
                    return Ok(FlowExecution::waiting(
                        output_value,
                        FlowWait { reason, snapshot },
                    ));
                }
                NodeControl::Jump(jump) => {
                    let jump_target = self.apply_jump(&step_ctx, &mut state, jump).await?;
                    flow_ir = jump_target.flow;
                    current_flow_id = jump_target.flow_id;
                    current = jump_target.node_id;
                }
                NodeControl::Respond {
                    text,
                    card_cbor,
                    needs_user,
                } => {
                    let response = json!({
                        "text": text,
                        "card_cbor": card_cbor,
                        "needs_user": needs_user,
                    });
                    state.push_egress(response);
                    return Ok(FlowExecution::completed(state.finalize_with(None)));
                }
            }
        }
    }

    async fn dispatch_node(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        node: &HostNode,
        state: &mut ExecutionState,
        mut payload: Value,
        event: &NodeEvent<'_>,
    ) -> Result<DispatchOutcome> {
        inject_card_locale(&mut payload, &state.entry);
        match &node.kind {
            NodeKind::Exec { target_component } => self
                .execute_component_exec(
                    ctx,
                    node_id,
                    node,
                    payload,
                    event,
                    ComponentOverrides {
                        component: Some(target_component.as_str()),
                        operation: node.operation_name.as_deref(),
                    },
                )
                .await
                .and_then(component_dispatch_outcome),
            NodeKind::PackComponent { component_ref } => self
                .execute_component_call(ctx, node_id, node, payload, component_ref.as_str(), event)
                .await
                .and_then(component_dispatch_outcome),
            NodeKind::FlowCall => self
                .execute_flow_call(ctx, payload)
                .await
                .map(DispatchOutcome::complete),
            NodeKind::ProviderInvoke => self
                .execute_provider_invoke(ctx, node_id, state, payload, event)
                .await
                .map(DispatchOutcome::complete),
            NodeKind::BuiltinEmit { kind } => {
                match kind {
                    EmitKind::Log | EmitKind::Response => {}
                    EmitKind::Other(component) => {
                        tracing::debug!(%component, "handling emit.* as builtin");
                    }
                }
                state.push_egress(payload.clone());
                Ok(DispatchOutcome::complete(NodeOutput::new(payload)))
            }
            NodeKind::BuiltinStateGet => self
                .execute_state_get(ctx, payload)
                .await
                .map(DispatchOutcome::complete),
            NodeKind::BuiltinStateSet => self
                .execute_state_set(ctx, payload)
                .await
                .map(DispatchOutcome::complete),
            NodeKind::Wait => {
                let reason = extract_wait_reason(&payload);
                Ok(DispatchOutcome::wait(NodeOutput::new(payload), reason))
            }
            NodeKind::DwAgent { agent_id } => {
                #[cfg(feature = "agentic-worker")]
                match self.dw_agent_dispatch {
                    crate::runner::agent_node::DwAgentDispatch::Nats => {
                        // Reroute to the durable out-of-process agentic path.
                        // Wrap the raw node payload as the dispatch `input` (the
                        // serve invoker reads `input.user_text`); `await=true` →
                        // pause+resume, identical to `agentic.call`.
                        let remote_payload = serde_json::json!({ "await": true, "input": payload });
                        self.execute_remote_dispatch(ctx, "agentic", agent_id, remote_payload)
                            .await
                    }
                    crate::runner::agent_node::DwAgentDispatch::InProcess => self
                        .execute_dw_agent(ctx, agent_id, payload)
                        .await
                        .map(DispatchOutcome::complete),
                }
                #[cfg(not(feature = "agentic-worker"))]
                self.execute_dw_agent(ctx, agent_id, payload)
                    .await
                    .map(DispatchOutcome::complete)
            }
            NodeKind::DwAgentGraph { graph_id } => self
                .execute_dw_agent_graph(ctx, graph_id, payload)
                .await
                .map(DispatchOutcome::complete),
            NodeKind::SorlaCall { target } => self.execute_sorla_call(ctx, target, payload).await,
            NodeKind::OperalaCall { target } => {
                self.execute_operala_call(ctx, target, payload).await
            }
            NodeKind::AgenticCall { target } => {
                self.execute_agentic_call(ctx, target, payload).await
            }
            NodeKind::TelcoXCall { target } => {
                self.execute_telco_x_call(ctx, target, payload).await
            }
            NodeKind::Mcp { server_id, tool } => self
                .execute_mcp(ctx, server_id, tool, payload)
                .await
                .map(DispatchOutcome::complete),
        }
    }

    #[cfg(feature = "agentic-worker")]
    async fn execute_dw_agent(
        &self,
        ctx: &FlowContext<'_>,
        agent_id: &str,
        payload: Value,
    ) -> Result<NodeOutput> {
        let handler = self
            .agent_node_handler
            .as_ref()
            .context("DwAgent node dispatched but no AgentNodeHandler configured on FlowEngine")?;
        let session_id = ctx.session_id.unwrap_or("");
        let result = handler
            .execute(
                ctx.tenant,
                &self.default_env,
                agent_id,
                session_id,
                &payload,
            )
            .await?;
        Ok(NodeOutput::new(result))
    }

    #[cfg(not(feature = "agentic-worker"))]
    async fn execute_dw_agent(
        &self,
        _ctx: &FlowContext<'_>,
        agent_id: &str,
        _payload: Value,
    ) -> Result<NodeOutput> {
        anyhow::bail!(
            "DwAgent node '{agent_id}' cannot run: this build was compiled without the \
             `agentic-worker` feature. Rebuild with --features agentic-worker."
        )
    }

    /// Dispatch a `sorla.call` flow node to the configured
    /// [`RemoteDispatchHandler`], publishing the work to a separate runtime.
    ///
    /// Input payload contract (JSON):
    /// `{ "await": bool (default true), "operation": str, "deadline_ms": u64?,
    ///    "input": any }`.
    ///
    /// The correlation id is the canonical session hint (`ctx.session_id`)
    /// suffixed with `::pack=<pack_id>::flow=<flow_id>` markers. The bare hint
    /// already encodes the conversation; the markers let the resume path
    /// (`RuntimeSessionResumer`) route the response back to a registered
    /// `(pack_id, flow_id)` and re-derive the store key. The markers are the
    /// exact inverse of the resumer's parsing (`::flow=` then `::pack=`,
    /// split off the trailing end).
    ///
    /// - `await=true`  -> publish + PAUSE the flow ([`DispatchOutcome::wait`]).
    /// - `await=false` -> publish + complete immediately with
    ///   `{ "dispatched": true, "correlation_id": <marked hint> }`.
    ///
    /// [`RemoteDispatchHandler`]: crate::runner::remote_dispatch::RemoteDispatchHandler
    async fn execute_sorla_call(
        &self,
        ctx: &FlowContext<'_>,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        self.execute_remote_dispatch(ctx, "sorla", target, payload)
            .await
    }

    /// Dispatch an `operala.call` flow node via the shared remote-dispatch seam.
    /// Identical to [`execute_sorla_call`] except the runtime name is `"operala"`.
    async fn execute_operala_call(
        &self,
        ctx: &FlowContext<'_>,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        self.execute_remote_dispatch(ctx, "operala", target, payload)
            .await
    }

    /// Dispatch an `agentic.call` flow node via the shared remote-dispatch seam.
    /// Identical to [`execute_sorla_call`] except the runtime name is `"agentic"`.
    /// This is the out-of-process agentic path; the in-process `dw.agent` node
    /// is completely separate and untouched.
    async fn execute_agentic_call(
        &self,
        ctx: &FlowContext<'_>,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        self.execute_remote_dispatch(ctx, "agentic", target, payload)
            .await
    }

    /// Dispatch a `telco-x.call` flow node via the shared remote-dispatch seam.
    /// Mirrors [`execute_operala_call`] with runtime name `"telco-x"`. Wire-ready:
    /// no telco-x runtime is deployed yet, so an awaiting node pauses until one is.
    async fn execute_telco_x_call(
        &self,
        ctx: &FlowContext<'_>,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        self.execute_remote_dispatch(ctx, "telco-x", target, payload)
            .await
    }

    /// Execute a `component == "mcp"` flow node (LOCKED ENCODING v2).
    ///
    /// `payload` is the already-rendered node input mapping (the engine
    /// templates `{{ }}` against flow state before dispatch), shaped
    /// `{ "server": <id>, "tool": <name>, "arguments": <object>,
    ///    "output": <optional string state key> }`.
    ///
    /// `server`/`tool` are sourced from this payload (the source of truth);
    /// the `server_id`/`tool` parsed at flow-load time are passed in only as a
    /// fallback for the legacy `operation = "<server>/<tool>"` encoding. The MCP
    /// tool is invoked through the tenant's `flow_editor` catalog (reusing
    /// `greentic-aw-runtime`'s `McpToolSource`); the result value is bound under
    /// `output` when present, else returned as the node payload.
    ///
    /// Graceful by contract: MCP being unconfigured or the tool being
    /// unreachable yields a structured `{"error": ...}` value — never a panic,
    /// never an aborted runtime.
    #[cfg(feature = "agentic-worker")]
    async fn execute_mcp(
        &self,
        ctx: &FlowContext<'_>,
        server_id: &str,
        tool: &str,
        payload: Value,
    ) -> Result<NodeOutput> {
        // Payload is the source of truth: prefer the rendered `server`/`tool`
        // from config, falling back to the values resolved at flow-load time
        // (legacy `operation`/`mcp:` encoding).
        let payload_server = crate::runner::mcp_node::str_field(&payload, "server");
        let payload_tool = crate::runner::mcp_node::str_field(&payload, "tool");
        let server_id = payload_server.as_deref().unwrap_or(server_id);
        let tool = payload_tool.as_deref().unwrap_or(tool);

        // `arguments` defaults to `{}` so a no-arg tool needs no config.
        let arguments = payload
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(JsonMap::new()));

        let result = crate::runner::mcp_node::invoke(
            self.mcp_tool_source.as_ref(),
            ctx.tenant,
            &self.default_env,
            server_id,
            tool,
            &arguments,
        )
        .await;

        // Bind the result under the optional `output` state key. When absent,
        // the raw tool result becomes the node payload (still addressable via
        // the standard `node.<id>.payload` mechanism).
        let bound = match payload.get("output").and_then(Value::as_str) {
            Some(key) if !key.is_empty() => json!({ key: result }),
            _ => result,
        };
        Ok(NodeOutput::new(bound))
    }

    /// Compile-time stub for the MCP flow node when the agentic-worker feature
    /// (which carries the MCP runtime deps) is disabled. The node degrades to a
    /// clear error value rather than failing the build or the run.
    #[cfg(not(feature = "agentic-worker"))]
    async fn execute_mcp(
        &self,
        _ctx: &FlowContext<'_>,
        server_id: &str,
        tool: &str,
        _payload: Value,
    ) -> Result<NodeOutput> {
        Ok(NodeOutput::new(json!({
            "error": format!(
                "mcp node '{server_id}/{tool}' requires the agentic-worker feature (MCP runtime not compiled in)"
            )
        })))
    }

    /// Shared body for all native remote-dispatch flow nodes (`sorla.call`,
    /// `operala.call`, `agentic.call`). Routes through the injected
    /// [`RemoteDispatchHandler`] with the given `runtime` name.
    ///
    /// Input payload contract (JSON):
    /// `{ "await": bool (default true), "operation": str, "deadline_ms": u64?,
    ///    "input": any }`.
    ///
    /// The correlation id is the canonical session hint (`ctx.session_id`)
    /// suffixed with `::pack=<pack_id>::flow=<flow_id>` markers so the resume
    /// path (`RuntimeSessionResumer`) can route the response back.
    ///
    /// - `await=true`  -> publish + PAUSE the flow ([`DispatchOutcome::wait`]).
    /// - `await=false` -> publish + complete immediately with
    ///   `{ "dispatched": true, "correlation_id": <marked hint> }`.
    ///
    /// [`RemoteDispatchHandler`]: crate::runner::remote_dispatch::RemoteDispatchHandler
    async fn execute_remote_dispatch(
        &self,
        ctx: &FlowContext<'_>,
        runtime: &str,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        let handler = self.remote_dispatch_handler.as_ref().with_context(|| {
            format!("{runtime}.call node dispatched but no RemoteDispatchHandler configured")
        })?;

        let await_mode = payload
            .get("await")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let operation = payload
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let deadline_ms = payload.get("deadline_ms").and_then(Value::as_u64);
        let inner_input = payload.get("input").cloned().unwrap_or(Value::Null);

        // The resume path (`RuntimeSessionResumer`) recovers `pack_id` and
        // `flow_id` from `::pack=`/`::flow=` markers on the correlation id to
        // route the synthesized resume envelope, then strips them to recover the
        // bare canonical hint used as the store key. So the published
        // correlation id MUST carry those markers and preserve the bare hint.
        //
        // Bare canonical hint = everything before the first `::` marker. This is
        // robust whether `ctx.session_id` is already bare (the production case)
        // or has accreted a marker.
        let raw_hint = ctx.session_id.unwrap_or_default();
        let bare_hint = raw_hint.split("::").next().unwrap_or_default();
        // The store key (`FlowResumeStore::save`) hashes the inbound reply
        // scope's `conversation`/`thread`/`reply_to`. The bare canonical hint
        // only encodes `conversation`, so a wait saved against a non-empty
        // `thread`/`reply_to` would be un-keyable on resume. Append OPAQUE
        // `::thread=`/`::reply=` markers so `RuntimeSessionResumer` can rebuild
        // the EXACT reply scope and recompute the same `scope_hash`. The remote
        // bridge echoes the correlation verbatim, so this needs no bridge change.
        // Markers are omitted when their value is empty (back-compat with the
        // no-thread case).
        let mut correlation_id =
            format!("{}::pack={}::flow={}", bare_hint, ctx.pack_id, ctx.flow_id);
        if let Some(scope) = ctx.reply_scope {
            if let Some(thread) = scope.thread.as_deref().filter(|value| !value.is_empty()) {
                correlation_id.push_str("::thread=");
                correlation_id.push_str(thread);
            }
            if let Some(reply_to) = scope.reply_to.as_deref().filter(|value| !value.is_empty()) {
                correlation_id.push_str("::reply=");
                correlation_id.push_str(reply_to);
            }
        }
        let mode = if await_mode {
            greentic_types::DispatchMode::Await
        } else {
            greentic_types::DispatchMode::FireAndForget
        };

        let action = handler
            .dispatch(crate::runner::remote_dispatch::RemoteDispatch {
                tenant: ctx.tenant.to_string(),
                env: self.default_env.clone(),
                runtime: runtime.to_string(),
                target: target.to_string(),
                operation,
                mode,
                correlation_id: correlation_id.clone(),
                input: inner_input,
                deadline_ms,
            })
            .await?;

        match action {
            crate::runner::remote_dispatch::RemoteDispatchAction::AwaitingResponse {
                correlation_id,
            } => {
                let reason = format!("await-runtime:{correlation_id}");
                let output = NodeOutput::new(serde_json::json!({
                    "pending": true,
                    "correlation_id": correlation_id,
                }));
                Ok(DispatchOutcome::wait(output, Some(reason)))
            }
            crate::runner::remote_dispatch::RemoteDispatchAction::Dispatched => {
                let output = NodeOutput::new(serde_json::json!({
                    "dispatched": true,
                    "correlation_id": correlation_id,
                }));
                Ok(DispatchOutcome::complete(output))
            }
        }
    }

    /// Dispatch a `DwAgentGraph` flow node to the configured
    /// [`GraphNodeHandler`]. Mirrors [`execute_dw_agent`]: same tenant/env/
    /// session-id derivation, same envelope, same "handler not configured"
    /// error path.
    ///
    /// [`execute_dw_agent`]: FlowEngine::execute_dw_agent
    #[cfg(feature = "agentic-worker")]
    async fn execute_dw_agent_graph(
        &self,
        ctx: &FlowContext<'_>,
        graph_id: &str,
        payload: Value,
    ) -> Result<NodeOutput> {
        let handler = self.graph_node_handler.as_ref().context(
            "DwAgentGraph node dispatched but no GraphNodeHandler configured on FlowEngine",
        )?;
        let session_id = ctx.session_id.unwrap_or("");
        let result = handler
            .execute(
                ctx.tenant,
                &self.default_env,
                graph_id,
                session_id,
                &payload,
            )
            .await?;
        Ok(NodeOutput::new(result))
    }

    #[cfg(not(feature = "agentic-worker"))]
    async fn execute_dw_agent_graph(
        &self,
        _ctx: &FlowContext<'_>,
        graph_id: &str,
        _payload: Value,
    ) -> Result<NodeOutput> {
        anyhow::bail!(
            "DwAgentGraph node '{graph_id}' cannot run: this build was compiled without the \
             `agentic-worker` feature. Rebuild with --features agentic-worker."
        )
    }

    async fn execute_state_get(&self, ctx: &FlowContext<'_>, payload: Value) -> Result<NodeOutput> {
        let key = Self::extract_state_key_helper(&payload)?;
        let pack = self.pack_for_flow(ctx)?;
        let store = pack
            .state_store_handle()
            .context("state store is not configured for this runtime")?;
        let tenant_ctx = self.state_tenant_ctx(ctx)?;
        let state_key = greentic_state::StateKey::new(&key);
        let value = store
            .get_json(
                &tenant_ctx,
                crate::storage::state::STATE_PREFIX,
                &state_key,
                None,
            )
            .with_context(|| format!("state.get failed for key `{key}`"))?;
        let payload = serde_json::json!({
            "key": key,
            "value": value,
            "found": value.is_some(),
        });
        Ok(NodeOutput::new(payload))
    }

    async fn execute_state_set(&self, ctx: &FlowContext<'_>, payload: Value) -> Result<NodeOutput> {
        let key = Self::extract_state_key_helper(&payload)?;
        let value = payload.get("value").cloned().unwrap_or(Value::Null);
        let pack = self.pack_for_flow(ctx)?;
        let store = pack
            .state_store_handle()
            .context("state store is not configured for this runtime")?;
        let tenant_ctx = self.state_tenant_ctx(ctx)?;
        let state_key = greentic_state::StateKey::new(&key);
        store
            .set_json(
                &tenant_ctx,
                crate::storage::state::STATE_PREFIX,
                &state_key,
                None,
                &value,
                None,
            )
            .with_context(|| format!("state.set failed for key `{key}`"))?;
        let payload = serde_json::json!({ "key": key, "value": value });
        Ok(NodeOutput::new(payload))
    }

    fn pack_for_flow(&self, ctx: &FlowContext<'_>) -> Result<&Arc<PackRuntime>> {
        let key = FlowKey {
            pack_id: ctx.pack_id.to_string(),
            flow_id: ctx.flow_id.to_string(),
        };
        let idx = self.flow_sources.get(&key).with_context(|| {
            format!("flow {} (pack {}) not registered", ctx.flow_id, ctx.pack_id)
        })?;
        Ok(&self.packs[*idx])
    }

    fn extract_state_key_helper(payload: &Value) -> Result<String> {
        payload
            .get("key")
            .and_then(Value::as_str)
            .map(String::from)
            .filter(|k| !k.is_empty())
            .context("state node payload missing required `key` (non-empty string)")
    }

    fn state_tenant_ctx(&self, ctx: &FlowContext<'_>) -> Result<greentic_types::TenantCtx> {
        let env = greentic_types::EnvId::from_str(&self.default_env)
            .with_context(|| format!("invalid env id `{}`", self.default_env))?;
        let tenant = greentic_types::TenantId::from_str(ctx.tenant)
            .with_context(|| format!("invalid tenant id `{}`", ctx.tenant))?;
        Ok(greentic_types::TenantCtx::new(env, tenant))
    }

    async fn apply_jump(
        &self,
        ctx: &FlowContext<'_>,
        state: &mut ExecutionState,
        jump: JumpControl,
    ) -> Result<JumpTarget> {
        let target_flow = jump.flow.trim();
        if target_flow.is_empty() {
            bail!("missing_flow");
        }

        let flow = self
            .get_or_load_flow(ctx.pack_id, target_flow)
            .await
            .with_context(|| format!("unknown_flow:{target_flow}"))?;

        let target_node = if let Some(node) = jump.node.as_deref() {
            let parsed = NodeId::from_str(node).with_context(|| format!("unknown_node:{node}"))?;
            if !flow.nodes.contains_key(&parsed) {
                bail!("unknown_node:{node}");
            }
            parsed
        } else {
            flow.start
                .clone()
                .or_else(|| flow.nodes.keys().next().cloned())
                .ok_or_else(|| anyhow!("jump_failed: flow {target_flow} has no start node"))?
        };

        let max_redirects = jump.max_redirects.unwrap_or(3);
        if state.redirect_count() >= max_redirects {
            bail!("redirect_limit");
        }
        state.increment_redirect_count();
        state.replace_input(jump.payload.clone());
        state.last_output = Some(jump.payload);
        tracing::info!(
            flow_id = %ctx.flow_id,
            target_flow = %target_flow,
            target_node = %target_node.as_str(),
            reason = ?jump.reason,
            redirects = state.redirect_count(),
            "flow.jump.applied"
        );

        Ok(JumpTarget {
            flow_id: target_flow.to_string(),
            flow,
            node_id: target_node,
        })
    }

    async fn execute_flow_call(&self, ctx: &FlowContext<'_>, payload: Value) -> Result<NodeOutput> {
        #[derive(Deserialize)]
        struct FlowCallPayload {
            #[serde(alias = "flow")]
            flow_id: String,
            #[serde(default)]
            input: Value,
        }

        let call: FlowCallPayload =
            serde_json::from_value(payload).context("invalid payload for flow.call node")?;
        if call.flow_id.trim().is_empty() {
            bail!("flow.call requires a non-empty flow_id");
        }

        let sub_input = if call.input.is_null() {
            Value::Null
        } else {
            call.input
        };

        let flow_id_owned = call.flow_id;
        let action = "flow.call";
        let sub_ctx = FlowContext {
            tenant: ctx.tenant,
            pack_id: ctx.pack_id,
            flow_id: flow_id_owned.as_str(),
            node_id: None,
            tool: ctx.tool,
            action: Some(action),
            session_id: ctx.session_id,
            provider_id: ctx.provider_id,
            reply_scope: ctx.reply_scope,
            retry_config: ctx.retry_config,
            attempt: ctx.attempt,
            observer: ctx.observer,
            mocks: ctx.mocks,
        };

        let execution = Box::pin(self.execute(sub_ctx, sub_input))
            .await
            .with_context(|| format!("flow.call failed for {}", flow_id_owned))?;
        match execution.status {
            FlowStatus::Completed => Ok(NodeOutput::new(execution.output)),
            FlowStatus::Waiting(wait) => bail!(
                "flow.call cannot pause (flow {} waiting {:?})",
                flow_id_owned,
                wait.reason
            ),
        }
    }

    async fn execute_component_exec(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        node: &HostNode,
        payload: Value,
        event: &NodeEvent<'_>,
        overrides: ComponentOverrides<'_>,
    ) -> Result<NodeOutput> {
        #[derive(Deserialize)]
        struct ComponentPayload {
            #[serde(default, alias = "component_ref", alias = "component")]
            component: Option<String>,
            #[serde(alias = "op")]
            operation: Option<String>,
            #[serde(default)]
            input: Value,
            #[serde(default)]
            config: Value,
        }

        let payload: ComponentPayload =
            serde_json::from_value(payload).context("invalid payload for component.exec")?;
        let component_ref = overrides
            .component
            .map(str::to_string)
            .or_else(|| payload.component.filter(|v| !v.trim().is_empty()))
            .with_context(|| "component.exec requires a component_ref")?;
        let operation = resolve_component_operation(
            node_id,
            node.component_id.as_str(),
            payload.operation,
            overrides.operation,
            node.operation_in_mapping.as_deref(),
        )?;
        let call = ComponentCall {
            component_ref,
            operation,
            input: payload.input,
            config: payload.config,
        };

        self.invoke_component_call(ctx, node_id, call, event).await
    }

    async fn execute_component_call(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        node: &HostNode,
        payload: Value,
        component_ref: &str,
        event: &NodeEvent<'_>,
    ) -> Result<NodeOutput> {
        let payload_operation = extract_operation_from_mapping(&payload);
        let (input, config) = split_operation_payload(payload);
        let operation = resolve_component_operation(
            node_id,
            node.component_id.as_str(),
            payload_operation,
            node.operation_name.as_deref(),
            node.operation_in_mapping.as_deref(),
        )?;
        let call = ComponentCall {
            component_ref: component_ref.to_string(),
            operation,
            input,
            config,
        };
        self.invoke_component_call(ctx, node_id, call, event).await
    }

    async fn invoke_component_call(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        mut call: ComponentCall,
        event: &NodeEvent<'_>,
    ) -> Result<NodeOutput> {
        self.validate_component(ctx, event, &call)?;
        let key = FlowKey {
            pack_id: ctx.pack_id.to_string(),
            flow_id: ctx.flow_id.to_string(),
        };
        let pack_idx = *self.flow_sources.get(&key).with_context(|| {
            format!("flow {} (pack {}) not registered", ctx.flow_id, ctx.pack_id)
        })?;
        let pack = Arc::clone(&self.packs[pack_idx]);

        // Promote adaptive-card defaults from node config (default_card_asset /
        // default_card_inline / default_source) into the invocation, so the
        // component receives a valid `card_spec` field even when the user input
        // is empty (e.g. webchat ConversationStart with no text). Without this,
        // schema validation in the component reports AC_INVOCATION_MISSING_FIELD
        // and the renderer falls back to a generic "Welcome" placeholder.
        promote_card_config_to_invocation(&mut call.input, &call.config);

        // Pre-resolve card asset paths: read JSON files from the pack's assets
        // directory and inject as inline_json so the component doesn't need
        // WASI filesystem access.
        resolve_card_assets(&mut call.input, &pack);

        // When the input is a card-like invocation (has card_source/card_spec),
        // pass it directly to the component instead of wrapping in an
        // InvocationEnvelope.  The envelope serialises the payload field as a
        // byte array which the component cannot decode back, and the
        // InvocationPayload::parse heuristic strips domain fields when a
        // `payload` key is present (e.g.  the card's Handlebars template
        // context `payload: {}`).
        let is_card = is_card_invocation(&call.input);

        let input_json = if is_card {
            serde_json::to_string(&call.input)?
        } else {
            // Runtime owns ctx; flows must not embed ctx, even if they provide envelopes.
            let meta = InvocationMeta {
                env: &self.default_env,
                tenant: ctx.tenant,
                flow_id: ctx.flow_id,
                node_id: Some(node_id),
                provider_id: ctx.provider_id,
                session_id: ctx.session_id,
                attempt: ctx.attempt,
            };
            let invocation_envelope =
                build_invocation_envelope(meta, call.operation.as_str(), call.input)
                    .context("build invocation envelope for component call")?;
            serde_json::to_string(&invocation_envelope)?
        };
        let config_json = if call.config.is_null() {
            None
        } else {
            Some(serde_json::to_string(&call.config)?)
        };

        let exec_ctx = component_exec_ctx(ctx, node_id);
        #[cfg(feature = "fault-injection")]
        {
            let fault_ctx = FaultContext {
                pack_id: ctx.pack_id,
                flow_id: ctx.flow_id,
                node_id: Some(node_id),
                attempt: ctx.attempt,
            };
            maybe_fail(FaultPoint::BeforeComponentCall, fault_ctx)
                .map_err(|err| anyhow!(err.to_string()))?;
        }
        let value = pack
            .invoke_component(
                call.component_ref.as_str(),
                exec_ctx,
                call.operation.as_str(),
                config_json,
                input_json,
            )
            .await?;
        #[cfg(feature = "fault-injection")]
        {
            let fault_ctx = FaultContext {
                pack_id: ctx.pack_id,
                flow_id: ctx.flow_id,
                node_id: Some(node_id),
                attempt: ctx.attempt,
            };
            maybe_fail(FaultPoint::AfterComponentCall, fault_ctx)
                .map_err(|err| anyhow!(err.to_string()))?;
        }

        if let Some((code, message)) = component_error(&value) {
            bail!(
                "component {} failed: {}: {}",
                call.component_ref,
                code,
                message
            );
        }
        Ok(NodeOutput::new(value))
    }

    async fn execute_provider_invoke(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        state: &ExecutionState,
        payload: Value,
        event: &NodeEvent<'_>,
    ) -> Result<NodeOutput> {
        #[derive(Deserialize)]
        struct ProviderPayload {
            #[serde(default)]
            provider_id: Option<String>,
            #[serde(default)]
            provider_type: Option<String>,
            #[serde(default, alias = "operation")]
            op: Option<String>,
            #[serde(default)]
            input: Value,
            #[serde(default)]
            in_map: Value,
            #[serde(default)]
            out_map: Value,
            #[serde(default)]
            err_map: Value,
        }

        let payload: ProviderPayload =
            serde_json::from_value(payload).context("invalid payload for provider.invoke")?;
        let op = payload
            .op
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .with_context(|| "provider.invoke requires an op")?
            .to_string();

        let prev = state
            .last_output
            .as_ref()
            .cloned()
            .unwrap_or_else(|| Value::Object(JsonMap::new()));
        let base_ctx = template_context(state, prev);

        let input_value = if !payload.in_map.is_null() {
            let mut ctx_value = base_ctx.clone();
            if let Value::Object(ref mut map) = ctx_value {
                map.insert("input".into(), payload.input.clone());
                map.insert("result".into(), payload.input.clone());
            }
            render_template_value(
                &payload.in_map,
                &ctx_value,
                TemplateOptions {
                    allow_pointer: true,
                },
            )
            .context("failed to render provider.invoke in_map")?
        } else if !payload.input.is_null() {
            payload.input
        } else {
            Value::Null
        };
        let input_json = serde_json::to_vec(&input_value)?;

        self.validate_tool(
            ctx,
            event,
            payload.provider_id.as_deref(),
            payload.provider_type.as_deref(),
            &op,
            &input_value,
        )?;

        let key = FlowKey {
            pack_id: ctx.pack_id.to_string(),
            flow_id: ctx.flow_id.to_string(),
        };
        let pack_idx = *self.flow_sources.get(&key).with_context(|| {
            format!("flow {} (pack {}) not registered", ctx.flow_id, ctx.pack_id)
        })?;
        let pack = Arc::clone(&self.packs[pack_idx]);
        let binding = pack.resolve_provider(
            payload.provider_id.as_deref(),
            payload.provider_type.as_deref(),
        );

        // If pack-local resolution fails, try the cross-pack resolver (capability registry).
        if binding.is_err()
            && let Some(output) = self.try_invoke_cross_pack_resolver(
                payload.provider_id.as_deref(),
                payload.provider_type.as_deref(),
                &op,
                &input_json,
                ctx.tenant,
            )?
        {
            return Ok(output);
        }

        let binding = binding?;
        let exec_ctx = component_exec_ctx(ctx, node_id);
        #[cfg(feature = "fault-injection")]
        {
            let fault_ctx = FaultContext {
                pack_id: ctx.pack_id,
                flow_id: ctx.flow_id,
                node_id: Some(node_id),
                attempt: ctx.attempt,
            };
            maybe_fail(FaultPoint::BeforeToolCall, fault_ctx)
                .map_err(|err| anyhow!(err.to_string()))?;
        }
        let result = pack
            .invoke_provider(&binding, exec_ctx, &op, input_json)
            .await?;
        #[cfg(feature = "fault-injection")]
        {
            let fault_ctx = FaultContext {
                pack_id: ctx.pack_id,
                flow_id: ctx.flow_id,
                node_id: Some(node_id),
                attempt: ctx.attempt,
            };
            maybe_fail(FaultPoint::AfterToolCall, fault_ctx)
                .map_err(|err| anyhow!(err.to_string()))?;
        }

        let output = if payload.out_map.is_null() {
            result
        } else {
            let mut ctx_value = base_ctx;
            if let Value::Object(ref mut map) = ctx_value {
                map.insert("input".into(), result.clone());
                map.insert("result".into(), result.clone());
            }
            render_template_value(
                &payload.out_map,
                &ctx_value,
                TemplateOptions {
                    allow_pointer: true,
                },
            )
            .context("failed to render provider.invoke out_map")?
        };
        let _ = payload.err_map;
        Ok(NodeOutput::new(output))
    }

    fn try_invoke_cross_pack_resolver(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        op: &str,
        input_json: &[u8],
        tenant: &str,
    ) -> Result<Option<NodeOutput>> {
        eprintln!(
            "[DEBUG] provider.invoke: pack-local failed, has_resolver={}",
            self.cross_pack_resolver.is_some()
        );
        let Some(resolver) = self.cross_pack_resolver.as_ref() else {
            return Ok(None);
        };
        let provider_id = provider_id.unwrap_or("unknown");
        tracing::info!(
            provider_id,
            op = %op,
            "provider.invoke: pack-local resolution failed, trying cross-pack resolver"
        );
        let result_value =
            resolver.invoke(provider_id, provider_type, op, input_json, tenant, None)?;
        Ok(Some(NodeOutput::new(result_value)))
    }

    fn validate_component(
        &self,
        ctx: &FlowContext<'_>,
        event: &NodeEvent<'_>,
        call: &ComponentCall,
    ) -> Result<()> {
        if self.validation.mode == ValidationMode::Off {
            return Ok(());
        }
        let mut metadata = JsonMap::new();
        metadata.insert("tenant_id".to_string(), json!(ctx.tenant));
        if let Some(id) = ctx.session_id {
            metadata.insert("session".to_string(), json!({ "id": id }));
        }
        let envelope = json!({
            "component_id": call.component_ref,
            "operation": call.operation,
            "input": call.input,
            "config": call.config,
            "metadata": Value::Object(metadata),
        });
        let issues = validate_component_envelope(&envelope);
        self.report_validation(ctx, event, "component", issues)
    }

    fn validate_tool(
        &self,
        ctx: &FlowContext<'_>,
        event: &NodeEvent<'_>,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        operation: &str,
        input: &Value,
    ) -> Result<()> {
        if self.validation.mode == ValidationMode::Off {
            return Ok(());
        }
        let tool_id = provider_id.or(provider_type).unwrap_or("provider.invoke");
        let mut metadata = JsonMap::new();
        metadata.insert("tenant_id".to_string(), json!(ctx.tenant));
        if let Some(id) = ctx.session_id {
            metadata.insert("session".to_string(), json!({ "id": id }));
        }
        let envelope = json!({
            "tool_id": tool_id,
            "operation": operation,
            "input": input,
            "metadata": Value::Object(metadata),
        });
        let issues = validate_tool_envelope(&envelope);
        self.report_validation(ctx, event, "tool", issues)
    }

    fn report_validation(
        &self,
        ctx: &FlowContext<'_>,
        event: &NodeEvent<'_>,
        kind: &str,
        issues: Vec<ValidationIssue>,
    ) -> Result<()> {
        if issues.is_empty() {
            return Ok(());
        }
        if let Some(observer) = ctx.observer {
            observer.on_validation(event, &issues);
        }
        match self.validation.mode {
            ValidationMode::Warn => {
                tracing::warn!(
                    tenant = ctx.tenant,
                    flow_id = ctx.flow_id,
                    node_id = event.node_id,
                    kind,
                    issues = ?issues,
                    "invocation envelope validation issues"
                );
                Ok(())
            }
            ValidationMode::Error => {
                tracing::error!(
                    tenant = ctx.tenant,
                    flow_id = ctx.flow_id,
                    node_id = event.node_id,
                    kind,
                    issues = ?issues,
                    "invocation envelope validation failed"
                );
                bail!("invocation_validation_failed");
            }
            ValidationMode::Off => Ok(()),
        }
    }

    pub fn flows(&self) -> &[FlowDescriptor] {
        &self.flows
    }

    pub fn flow_by_key(&self, pack_id: &str, flow_id: &str) -> Option<&FlowDescriptor> {
        self.flows
            .iter()
            .find(|descriptor| descriptor.pack_id == pack_id && descriptor.id == flow_id)
    }

    pub fn flow_by_type(&self, flow_type: &str) -> Option<&FlowDescriptor> {
        let mut matches = self
            .flows
            .iter()
            .filter(|descriptor| descriptor.flow_type == flow_type);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    pub fn flow_by_id(&self, flow_id: &str) -> Option<&FlowDescriptor> {
        let mut matches = self
            .flows
            .iter()
            .filter(|descriptor| descriptor.id == flow_id);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }
}

pub trait ExecutionObserver: Send + Sync {
    fn on_node_start(&self, event: &NodeEvent<'_>);
    fn on_node_end(&self, event: &NodeEvent<'_>, output: &Value);
    fn on_node_error(&self, event: &NodeEvent<'_>, error: &dyn StdError);
    fn on_validation(&self, _event: &NodeEvent<'_>, _issues: &[ValidationIssue]) {}
}

pub struct NodeEvent<'a> {
    pub context: &'a FlowContext<'a>,
    pub node_id: &'a str,
    pub node: &'a HostNode,
    pub payload: &'a Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionState {
    #[serde(default)]
    entry: Value,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    nodes: HashMap<String, NodeOutput>,
    #[serde(default)]
    egress: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_output: Option<Value>,
    #[serde(default)]
    redirect_count: u32,
}

impl ExecutionState {
    fn new(input: Value) -> Self {
        Self {
            entry: input.clone(),
            input,
            nodes: HashMap::new(),
            egress: Vec::new(),
            last_output: None,
            redirect_count: 0,
        }
    }

    /// Refresh `entry` from `input` if the snapshot was loaded without an
    /// entry value. Kept for backwards compatibility with snapshots
    /// persisted before the entry-refresh fix in `FlowEngine::resume`.
    #[allow(dead_code)]
    fn ensure_entry(&mut self) {
        if self.entry.is_null() {
            self.entry = self.input.clone();
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
            "entry": self.entry.clone(),
            "input": self.input.clone(),
            "nodes": nodes,
            "redirect_count": self.redirect_count,
        })
    }

    fn outputs_map(&self) -> JsonMap<String, Value> {
        let mut outputs = JsonMap::new();
        for (id, output) in &self.nodes {
            outputs.insert(id.clone(), output.payload.clone());
        }
        outputs
    }
    fn push_egress(&mut self, payload: Value) {
        self.egress.push(payload);
    }

    fn replace_input(&mut self, input: Value) {
        self.input = input;
    }

    fn clear_egress(&mut self) {
        self.egress.clear();
    }

    fn redirect_count(&self) -> u32 {
        self.redirect_count
    }

    fn increment_redirect_count(&mut self) {
        self.redirect_count = self.redirect_count.saturating_add(1);
    }

    fn finalize_with(mut self, final_payload: Option<Value>) -> Value {
        if self.egress.is_empty() {
            return final_payload.unwrap_or(Value::Null);
        }
        let mut emitted = std::mem::take(&mut self.egress);
        if let Some(value) = final_payload {
            match value {
                Value::Null => {}
                Value::Array(items) => emitted.extend(items),
                other => emitted.push(other),
            }
        }
        Value::Array(emitted)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

struct DispatchOutcome {
    output: NodeOutput,
    control: NodeControl,
}

impl DispatchOutcome {
    fn complete(output: NodeOutput) -> Self {
        Self {
            output,
            control: NodeControl::Continue,
        }
    }

    fn wait(output: NodeOutput, reason: Option<String>) -> Self {
        Self {
            output,
            control: NodeControl::Wait { reason },
        }
    }

    fn with_control(output: NodeOutput, control: NodeControl) -> Self {
        Self { output, control }
    }
}

#[derive(Clone, Debug)]
enum NodeControl {
    Continue,
    Wait {
        reason: Option<String>,
    },
    Jump(JumpControl),
    Respond {
        text: Option<String>,
        card_cbor: Option<Vec<u8>>,
        needs_user: Option<bool>,
    },
}

#[derive(Clone, Debug)]
struct JumpControl {
    flow: String,
    node: Option<String>,
    payload: Value,
    hints: Value,
    max_redirects: Option<u32>,
    reason: Option<String>,
}

#[derive(Clone, Debug)]
struct JumpTarget {
    flow_id: String,
    flow: HostFlow,
    node_id: NodeId,
}

impl NodeOutput {
    fn with_meta(payload: Value, meta: Value) -> Self {
        Self {
            ok: true,
            payload,
            meta,
        }
    }
}

fn component_exec_ctx(ctx: &FlowContext<'_>, node_id: &str) -> ComponentExecCtx {
    ComponentExecCtx {
        tenant: ComponentTenantCtx {
            tenant: ctx.tenant.to_string(),
            team: None,
            user: ctx.provider_id.map(str::to_string),
            trace_id: None,
            i18n_id: None,
            correlation_id: ctx.session_id.map(str::to_string),
            deadline_unix_ms: None,
            attempt: ctx.attempt,
            idempotency_key: ctx.session_id.map(str::to_string),
        },
        i18n_id: None,
        flow_id: ctx.flow_id.to_string(),
        node_id: Some(node_id.to_string()),
    }
}

fn component_error(value: &Value) -> Option<(String, String)> {
    let obj = value.as_object()?;
    let ok = obj.get("ok").and_then(Value::as_bool)?;
    if ok {
        return None;
    }
    let err = obj.get("error")?.as_object()?;
    let code = err
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("component_error");
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("component reported error");
    Some((code.to_string(), message.to_string()))
}

fn extract_wait_reason(payload: &Value) -> Option<String> {
    match payload {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map
            .get("reason")
            .and_then(Value::as_str)
            .map(|value| value.to_string()),
        _ => None,
    }
}

fn component_dispatch_outcome(output: NodeOutput) -> Result<DispatchOutcome> {
    if let Some(control) = parse_component_control(&output.payload)? {
        return Ok(match control {
            NodeControl::Jump(jump) => {
                let adjusted = NodeOutput::with_meta(jump.payload.clone(), jump.hints.clone());
                DispatchOutcome::with_control(adjusted, NodeControl::Jump(jump))
            }
            NodeControl::Respond {
                text,
                card_cbor,
                needs_user,
            } => DispatchOutcome::with_control(
                output,
                NodeControl::Respond {
                    text,
                    card_cbor,
                    needs_user,
                },
            ),
            other => DispatchOutcome::with_control(output, other),
        });
    }
    Ok(DispatchOutcome::complete(output))
}

fn parse_component_control(payload: &Value) -> Result<Option<NodeControl>> {
    let Value::Object(map) = payload else {
        return Ok(None);
    };
    let Some(control_value) = map.get("greentic_control") else {
        return Ok(None);
    };
    let control = control_value
        .as_object()
        .ok_or_else(|| anyhow!("jump_failed: greentic_control must be an object"))?;
    let action = control
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("jump_failed: greentic_control.action is required"))?;
    let version = control
        .get("v")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("jump_failed: greentic_control.v is required"))?;
    if version != 1 {
        bail!("jump_failed: unsupported greentic_control.v={version}");
    }

    match action {
        "jump" => {
            let flow = control
                .get("flow")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("jump_failed: jump flow is required"))?
                .to_string();
            let node = control
                .get("node")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let payload = control.get("payload").cloned().unwrap_or(Value::Null);
            let hints = control.get("hints").cloned().unwrap_or(Value::Null);
            let max_redirects = control
                .get("max_redirects")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let reason = control
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(Some(NodeControl::Jump(JumpControl {
                flow,
                node,
                payload,
                hints,
                max_redirects,
                reason,
            })))
        }
        "respond" => {
            let text = control
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string);
            let card_cbor = control
                .get("card_cbor")
                .and_then(Value::as_array)
                .map(|bytes| {
                    bytes
                        .iter()
                        .filter_map(Value::as_u64)
                        .filter_map(|value| u8::try_from(value).ok())
                        .collect::<Vec<_>>()
                });
            let needs_user = control.get("needs_user").and_then(Value::as_bool);
            Ok(Some(NodeControl::Respond {
                text,
                card_cbor,
                needs_user,
            }))
        }
        _ => Ok(None),
    }
}

fn template_context(state: &ExecutionState, prev: Value) -> Value {
    let entry = if state.entry.is_null() {
        Value::Object(JsonMap::new())
    } else {
        state.entry.clone()
    };
    let mut ctx = JsonMap::new();
    ctx.insert("entry".into(), entry.clone());
    ctx.insert("in".into(), entry); // alias for entry - used in flow templates
    ctx.insert("prev".into(), prev);
    ctx.insert("node".into(), Value::Object(state.outputs_map()));
    ctx.insert("state".into(), state.context());
    Value::Object(ctx)
}

impl From<Flow> for HostFlow {
    fn from(value: Flow) -> Self {
        let mut nodes = IndexMap::new();
        for (id, node) in value.nodes {
            nodes.insert(id.clone(), HostNode::from(node));
        }
        let start = value
            .entrypoints
            .get("default")
            .and_then(Value::as_str)
            .and_then(|id| NodeId::from_str(id).ok())
            .or_else(|| nodes.keys().next().cloned());
        Self {
            id: value.id.as_str().to_string(),
            start,
            nodes,
        }
    }
}

impl From<Node> for HostNode {
    fn from(node: Node) -> Self {
        let full_ref = node.component.id.as_str().to_string();
        // When the pack compiler stores "component.operation" as a single ID
        // without a separate operation field, split on the last dot.
        let is_builtin = full_ref.starts_with("component.exec")
            || full_ref.starts_with("flow.")
            || full_ref.starts_with("emit.")
            || full_ref.starts_with("session.")
            || full_ref.starts_with("provider.")
            || full_ref.starts_with("dw.")
            || full_ref.starts_with("sorla.")
            || full_ref.starts_with("operala.")
            || full_ref.starts_with("agentic.")
            // `mcp:<server>/<tool>` is a self-contained ref; never dot-split it
            // into a `component.operation` pair.
            || full_ref.starts_with("mcp:");
        let (component_ref, raw_operation) = if node.component.operation.is_some() || is_builtin {
            (full_ref, node.component.operation.clone())
        } else if let Some(dot) = full_ref.rfind('.') {
            let comp = full_ref[..dot].to_string();
            let op = full_ref[dot + 1..].to_string();
            (comp, Some(op))
        } else {
            (full_ref, None)
        };
        let operation_in_mapping = extract_operation_from_mapping(&node.input.mapping);
        let operation_is_component_exec = raw_operation.as_deref() == Some("component.exec");
        let operation_is_emit = raw_operation
            .as_deref()
            .map(|op| op.starts_with("emit."))
            .unwrap_or(false);
        let is_component_exec = component_ref == "component.exec" || operation_is_component_exec;

        let kind = if is_component_exec {
            let target = if component_ref == "component.exec" {
                if let Some(op) = raw_operation
                    .as_deref()
                    .filter(|op| op.starts_with("emit."))
                {
                    op.to_string()
                } else {
                    extract_target_component(&node.input.mapping)
                        .unwrap_or_else(|| "component.exec".to_string())
                }
            } else {
                extract_target_component(&node.input.mapping)
                    .unwrap_or_else(|| component_ref.clone())
            };
            if target.starts_with("emit.") {
                NodeKind::BuiltinEmit {
                    kind: emit_kind_from_ref(&target),
                }
            } else {
                NodeKind::Exec {
                    target_component: target,
                }
            }
        } else if operation_is_emit {
            NodeKind::BuiltinEmit {
                kind: emit_kind_from_ref(raw_operation.as_deref().unwrap_or("emit.log")),
            }
        } else {
            match component_ref.as_str() {
                "flow.call" => NodeKind::FlowCall,
                "provider.invoke" => NodeKind::ProviderInvoke,
                "session.wait" => NodeKind::Wait,
                "state.get" => NodeKind::BuiltinStateGet,
                "state.set" => NodeKind::BuiltinStateSet,
                "dw.agent" => NodeKind::DwAgent {
                    agent_id: raw_operation.clone().unwrap_or_default(),
                },
                "dw.agent_graph" => NodeKind::DwAgentGraph {
                    graph_id: raw_operation.clone().unwrap_or_default(),
                },
                "sorla.call" => NodeKind::SorlaCall {
                    target: raw_operation.clone().unwrap_or_default(),
                },
                "operala.call" => NodeKind::OperalaCall {
                    target: raw_operation.clone().unwrap_or_default(),
                },
                "agentic.call" => NodeKind::AgenticCall {
                    target: raw_operation.clone().unwrap_or_default(),
                },
                "telco-x.call" => NodeKind::TelcoXCall {
                    target: raw_operation.clone().unwrap_or_default(),
                },
                comp if comp.starts_with("emit.") => NodeKind::BuiltinEmit {
                    kind: emit_kind_from_ref(comp),
                },
                // LOCKED ENCODING v2 (shared with greentic-flow + designer):
                // `component == "mcp"` (a valid `ComponentId`) with `server` and
                // `tool` carried in the node PAYLOAD/config:
                //   payload = { server, tool, arguments, output? }.
                // The payload is the source of truth. A legacy
                // `operation = "<server>/<tool>"` (or an `mcp:<server>/<tool>`
                // component ref) is honored only as a defensive fallback when the
                // payload lacks the fields, so older packs keep loading.
                "mcp" => mcp_node_kind(&node.input.mapping, raw_operation.as_deref()),
                // `mcp:<server>/<tool>` carried verbatim in `component.id`.
                // `greentic_types::ComponentId` rejects `:`/`/`, so this form
                // only survives when the node-type string bypasses ComponentId
                // validation; it is still recognized as a fallback for older
                // packs.
                comp if comp.starts_with("mcp:") => mcp_node_kind(&node.input.mapping, Some(comp)),
                other => NodeKind::PackComponent {
                    component_ref: other.to_string(),
                },
            }
        };
        let component_label = match &kind {
            NodeKind::Exec { .. } => "component.exec".to_string(),
            NodeKind::PackComponent { component_ref } => component_ref.clone(),
            NodeKind::ProviderInvoke => "provider.invoke".to_string(),
            NodeKind::FlowCall => "flow.call".to_string(),
            NodeKind::BuiltinEmit { kind } => emit_ref_from_kind(kind),
            NodeKind::BuiltinStateGet => "state.get".to_string(),
            NodeKind::BuiltinStateSet => "state.set".to_string(),
            NodeKind::Wait => "session.wait".to_string(),
            NodeKind::DwAgent { .. } => "dw.agent".to_string(),
            NodeKind::DwAgentGraph { .. } => "dw.agent_graph".to_string(),
            NodeKind::SorlaCall { .. } => "sorla.call".to_string(),
            NodeKind::OperalaCall { .. } => "operala.call".to_string(),
            NodeKind::AgenticCall { .. } => "agentic.call".to_string(),
            NodeKind::TelcoXCall { .. } => "telco-x.call".to_string(),
            NodeKind::Mcp { server_id, tool } => format!("mcp:{server_id}/{tool}"),
        };
        let operation_name = if is_component_exec && operation_is_component_exec {
            None
        } else {
            raw_operation.clone()
        };
        let payload_expr = match kind {
            NodeKind::BuiltinEmit { .. } => extract_emit_payload(&node.input.mapping),
            _ => node.input.mapping.clone(),
        };
        Self {
            kind,
            component: component_label,
            component_id: if is_component_exec {
                "component.exec".to_string()
            } else {
                component_ref
            },
            operation_name,
            operation_in_mapping,
            payload_expr,
            routing: node.routing,
        }
    }
}

/// Classify a `component == "mcp"` node into [`NodeKind::Mcp`].
///
/// LOCKED ENCODING v2: `server` and `tool` are read from the node
/// `payload`/config object (the source of truth). When the payload omits them,
/// a legacy `operation = "<server>/<tool>"` string (or an
/// `mcp:<server>/<tool>` component ref) is parsed as a defensive fallback for
/// older packs.
///
/// When neither source yields a usable `(server, tool)` pair the node falls
/// back to an ordinary [`NodeKind::PackComponent`], so a malformed MCP node
/// surfaces as a normal unknown-component error at run time rather than
/// panicking at load. Flow loading stays total.
fn mcp_node_kind(payload: &Value, legacy_ref: Option<&str>) -> NodeKind {
    if let Some((server_id, tool)) = crate::runner::mcp_node::server_tool_from_payload(payload) {
        return NodeKind::Mcp { server_id, tool };
    }
    if let Some((server_id, tool)) = legacy_ref.and_then(parse_legacy_mcp_ref) {
        return NodeKind::Mcp { server_id, tool };
    }
    NodeKind::PackComponent {
        component_ref: "mcp".to_string(),
    }
}

/// Parse a legacy MCP server/tool reference, accepting either the bare
/// `"<server>/<tool>"` operation form or the prefixed `mcp:<server>/<tool>`
/// component-ref form. Returns `None` when either part is missing or empty.
fn parse_legacy_mcp_ref(reference: &str) -> Option<(String, String)> {
    let rest = reference.strip_prefix("mcp:").unwrap_or(reference);
    let (server, tool) = rest.split_once('/')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

fn extract_target_component(payload: &Value) -> Option<String> {
    match payload {
        Value::Object(map) => map
            .get("component")
            .or_else(|| map.get("component_ref"))
            .and_then(Value::as_str)
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn extract_operation_from_mapping(payload: &Value) -> Option<String> {
    match payload {
        Value::Object(map) => map
            .get("operation")
            .or_else(|| map.get("op"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string()),
        _ => None,
    }
}

fn extract_emit_payload(payload: &Value) -> Value {
    if let Value::Object(map) = payload {
        if let Some(input) = map.get("input") {
            return input.clone();
        }
        if let Some(inner) = map.get("payload") {
            return inner.clone();
        }
    }
    payload.clone()
}

fn split_operation_payload(payload: Value) -> (Value, Value) {
    if let Value::Object(mut map) = payload.clone()
        && map.contains_key("input")
    {
        let input = map.remove("input").unwrap_or(Value::Null);
        let config = map.remove("config").unwrap_or(Value::Null);
        let legacy_only = map.keys().all(|key| {
            matches!(
                key.as_str(),
                "operation" | "op" | "component" | "component_ref"
            )
        });
        if legacy_only {
            return (input, config);
        }
    }
    (payload, Value::Null)
}

fn resolve_component_operation(
    node_id: &str,
    component_label: &str,
    payload_operation: Option<String>,
    operation_override: Option<&str>,
    operation_in_mapping: Option<&str>,
) -> Result<String> {
    if let Some(op) = operation_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(op.to_string());
    }

    if let Some(op) = payload_operation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(op.to_string());
    }

    let mut message = format!(
        "missing operation for node `{}` (component `{}`); expected node.component.operation to be set",
        node_id, component_label,
    );
    if let Some(found) = operation_in_mapping {
        message.push_str(&format!(
            ". Found operation in input.mapping (`{}`) but this is not used; pack compiler must preserve node.component.operation.",
            found
        ));
    }
    bail!(message);
}

fn emit_kind_from_ref(component_ref: &str) -> EmitKind {
    match component_ref {
        "emit.log" => EmitKind::Log,
        "emit.response" => EmitKind::Response,
        other => EmitKind::Other(other.to_string()),
    }
}

fn emit_ref_from_kind(kind: &EmitKind) -> String {
    match kind {
        EmitKind::Log => "emit.log".to_string(),
        EmitKind::Response => "emit.response".to_string(),
        EmitKind::Other(other) => other.clone(),
    }
}

/// Returns `true` when `input` looks like an Adaptive Card invocation
/// (contains `card_source` or `card_spec` at the top level).
fn is_card_invocation(input: &Value) -> bool {
    if let Value::Object(map) = input {
        return map.contains_key("card_source") || map.contains_key("card_spec");
    }
    false
}

/// When the node config declares adaptive-card defaults (`default_card_asset`,
/// `default_card_inline`, or `default_source`) but the runtime invocation has
/// no `card_source`/`card_spec` yet, lift those defaults into the invocation.
/// This produces a schema-valid invocation envelope so the component does not
/// fall back to its generic "Welcome" placeholder.
///
/// Adaptive-card defaults can arrive in either of two places depending on how
/// the pack was compiled:
/// - top-level `call.config` (post `split_operation_payload`)
/// - nested `call.input.config` (when the node mapping kept the
///   `{component, config}` shape and `split_operation_payload` left it intact)
fn promote_card_config_to_invocation(input: &mut Value, config: &Value) {
    if is_card_invocation(input) {
        return;
    }

    let cfg_map = card_defaults_source(input, config);
    let Some(cfg) = cfg_map else { return };

    let default_asset = cfg
        .get("default_card_asset")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let default_inline = cfg
        .get("default_card_inline")
        .filter(|value| value.is_object() || value.is_array())
        .cloned();
    let default_source = cfg
        .get("default_source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);

    if default_asset.is_none() && default_inline.is_none() && default_source.is_none() {
        return;
    }

    let card_source = default_source.unwrap_or_else(|| {
        if default_inline.is_some() {
            "inline".to_string()
        } else {
            "asset".to_string()
        }
    });

    let mut card_spec = serde_json::Map::new();
    match card_source.as_str() {
        "asset" => {
            if let Some(path) = default_asset {
                card_spec.insert("asset_path".into(), Value::String(path));
            }
        }
        "inline" => {
            if let Some(inline) = default_inline {
                card_spec.insert("inline_json".into(), inline);
            }
        }
        _ => {}
    }

    if !matches!(input, Value::Object(_)) {
        *input = Value::Object(serde_json::Map::new());
    }
    if let Value::Object(map) = input {
        map.insert("card_source".into(), Value::String(card_source));
        map.insert("card_spec".into(), Value::Object(card_spec));
    }
}

/// Locate the adaptive-card defaults config object, preferring the top-level
/// `call.config` when present, then falling back to a nested `input.config`
/// (the shape produced when `split_operation_payload` leaves the mapping
/// intact).
fn card_defaults_source<'a>(
    input: &'a Value,
    config: &'a Value,
) -> Option<&'a serde_json::Map<String, Value>> {
    if let Value::Object(map) = config {
        return Some(map);
    }
    if let Value::Object(map) = input
        && let Some(Value::Object(nested)) = map.get("config")
    {
        return Some(nested);
    }
    None
}

fn inject_card_locale(payload: &mut Value, entry: &Value) {
    if !is_card_invocation(payload) {
        return;
    }
    let Value::Object(map) = payload else { return };
    if map.contains_key("locale") {
        return;
    }
    let locale = entry
        .pointer("/input/metadata/locale")
        .or_else(|| entry.pointer("/metadata/locale"))
        .and_then(Value::as_str);
    if let Some(locale) = locale {
        map.insert("locale".into(), Value::String(locale.to_string()));
    }
}

/// Pre-resolve `card_source: "asset"` entries by reading the referenced JSON
/// file from the pack's assets directory and converting to
/// `card_source: "inline"` with `inline_json` populated.
///
/// This handles both top-level card fields and the nested `call.payload`
/// structure emitted by cards2pack.
fn resolve_card_assets(input: &mut Value, pack: &crate::pack::PackRuntime) {
    resolve_card_spec_asset(input, pack);

    // Also resolve inside `call.payload` (cards2pack duplicates the card
    // invocation there).
    if let Value::Object(map) = input
        && let Some(Value::Object(call)) = map.get_mut("call")
        && let Some(payload) = call.get_mut("payload")
    {
        resolve_card_spec_asset(payload, pack);
    }
}

/// Resolve a single card_spec asset_path → inline_json.
fn resolve_card_spec_asset(value: &mut Value, pack: &crate::pack::PackRuntime) {
    let Value::Object(map) = value else { return };

    let is_asset = map
        .get("card_source")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("asset"))
        .unwrap_or(false);
    if !is_asset {
        return;
    }

    let asset_path = map
        .get("card_spec")
        .and_then(|spec| spec.get("asset_path"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(asset_path) = asset_path else { return };

    match pack.read_asset(&asset_path) {
        Ok(bytes) => {
            let card_json: Value = match serde_json::from_slice(&bytes) {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!(
                        asset_path,
                        %err,
                        "failed to parse card asset as JSON; leaving as asset reference"
                    );
                    return;
                }
            };
            tracing::debug!(asset_path, "pre-resolved card asset to inline_json");
            map.insert("card_source".into(), Value::String("inline".into()));
            if let Some(Value::Object(spec)) = map.get_mut("card_spec") {
                spec.insert("inline_json".into(), card_json);
                spec.remove("asset_path");
            }
        }
        Err(err) => {
            tracing::warn!(
                asset_path,
                %err,
                "card asset not found in pack; leaving as asset reference"
            );
        }
    }

    // Pre-resolve i18n bundle: the WASM component cannot read pack assets
    // directly (no host resolver registered), so inline the i18n JSON into
    // the invocation under `card_spec.i18n_inline`. Defense-in-depth: when
    // the card omits an explicit `i18n_bundle_path` we still try the
    // conventional `assets/i18n/` location so cards that rely on
    // auto-generated i18n keys (e.g. cards2pack output) keep working.
    let configured_bundle_path = map
        .get("card_spec")
        .and_then(|spec| spec.get("i18n_bundle_path"))
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());

    let bundle_path = configured_bundle_path
        .clone()
        .unwrap_or_else(|| "assets/i18n".to_string());

    let i18n_entries = load_i18n_bundle_entries(&bundle_path, |path| pack.read_asset(path));

    if !i18n_entries.is_empty() {
        let locale_keys: Vec<_> = i18n_entries.keys().cloned().collect();
        if let Some(Value::Object(spec)) = map.get_mut("card_spec") {
            spec.insert("i18n_inline".into(), Value::Object(i18n_entries));
            if configured_bundle_path.is_some() {
                tracing::info!(%bundle_path, ?locale_keys, "pre-resolved i18n bundle into card_spec.i18n_inline");
            } else {
                tracing::info!(%bundle_path, ?locale_keys, "auto-discovered i18n bundle and inlined into card_spec.i18n_inline");
            }
        }
    }
}

fn load_i18n_bundle_entries<F>(bundle_path: &str, mut read_asset: F) -> JsonMap<String, Value>
where
    F: FnMut(&str) -> Result<Vec<u8>>,
{
    let mut i18n_entries = JsonMap::new();

    if bundle_path.ends_with(".json") {
        if let Ok(bytes) = read_asset(bundle_path)
            && let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(&bytes)
        {
            i18n_entries.insert("en".to_string(), Value::Object(entries));
        }
        return i18n_entries;
    }

    let manifest_path = format!("{bundle_path}/_manifest.json");
    let locale_codes: Vec<String> = read_asset(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            let locales = value
                .get("locales")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| value.as_array().cloned());
            locales.map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
        })
        .unwrap_or_default();

    tracing::info!(%bundle_path, ?locale_codes, "i18n manifest discovered locales");

    for locale in &locale_codes {
        let candidate = format!("{bundle_path}/{locale}.json");
        if let Ok(bytes) = read_asset(&candidate)
            && let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(&bytes)
        {
            i18n_entries.insert(locale.clone(), Value::Object(entries));
        }
    }
    if !i18n_entries.contains_key("en") {
        let en_path = format!("{bundle_path}/en.json");
        if let Ok(bytes) = read_asset(&en_path)
            && let Ok(Value::Object(entries)) = serde_json::from_slice::<Value>(&bytes)
        {
            i18n_entries.insert("en".to_string(), Value::Object(entries));
        }
    }

    i18n_entries
}

/// Outcome of `evaluate_custom_routing` for a node's `Routing::Custom` array.
///
/// `Next` advances the flow to the named target. `End` terminates the run.
/// `Wait` pauses the run at the current node so the next inbound activity
/// resumes here and re-evaluates the routing with the new context — this is
/// what allows messaging flows (welcome → ... → confirm) to behave like a
/// live conversation instead of restarting at the entry point on every
/// click.
#[derive(Debug)]
pub(crate) enum CustomRoutingDecision {
    Next(NodeId),
    End,
    Wait,
}

/// Evaluate a node's `Routing::Custom` array against the current execution
/// context.
///
/// Parses `Routing::Custom(Value)` as an array of `{condition, to}` objects.
/// Conditions are simple equality expressions like `response.action == "about"`.
/// Falls back to the first route without a condition (default route).
///
/// The evaluation context includes:
/// - All fields from the node output payload (top-level)
/// - `entry` / `in` — the original flow entry (incoming message)
/// - `response` — synthesized from entry metadata for convenient condition checks
///   (e.g. `response.action` maps to `metadata.action` from the incoming envelope)
fn evaluate_custom_routing(
    raw: &Value,
    output: &NodeOutput,
    state: &ExecutionState,
    flow_ir: &HostFlow,
    node_id: &NodeId,
) -> CustomRoutingDecision {
    let routes = match raw.as_array() {
        Some(arr) => arr,
        None => {
            tracing::warn!(
                flow_id = %flow_ir.id,
                node_id = %node_id,
                "custom routing is not an array; terminating"
            );
            return CustomRoutingDecision::End;
        }
    };

    // Build a rich context for condition evaluation:
    // Start with output payload, then overlay entry and synthesised "response".
    let ctx = build_routing_context(output, state);

    let mut has_condition = false;
    for route in routes {
        let condition = route.get("condition").and_then(|v| v.as_str());
        let to = route.get("to").and_then(|v| v.as_str());

        if let Some(cond) = condition {
            has_condition = true;
            if evaluate_simple_condition(cond, &ctx)
                && let Some(target) = to
                && let Ok(nid) = NodeId::new(target)
            {
                tracing::debug!(
                    flow_id = %flow_ir.id,
                    node_id = %node_id,
                    condition = cond,
                    target = target,
                    "conditional route matched"
                );
                return CustomRoutingDecision::Next(nid);
            }
        } else if let Some(target) = to
            && let Ok(nid) = NodeId::new(target)
        {
            tracing::debug!(
                flow_id = %flow_ir.id,
                node_id = %node_id,
                target = target,
                "default route taken"
            );
            return CustomRoutingDecision::Next(nid);
        }
    }

    // Fall-through. When the routing array contained at least one
    // conditional entry, treat the unmatched fall-through as a pause: the
    // user's next submission should be re-evaluated against this same
    // node's routing rather than restarting the flow from the entry point.
    // Routing arrays with no conditions at all (pure unconditional `out`
    // terminators) remain true ends.
    if has_condition {
        tracing::debug!(
            flow_id = %flow_ir.id,
            node_id = %node_id,
            "no conditional route matched; pausing run at current node for resume"
        );
        CustomRoutingDecision::Wait
    } else {
        tracing::warn!(
            flow_id = %flow_ir.id,
            node_id = %node_id,
            "no route matched and no conditions present; terminating"
        );
        CustomRoutingDecision::End
    }
}

/// Evaluate a simple condition expression like `response.action == "about"`.
///
/// Supports dotted path lookups against a JSON value context.
/// Format: `<path> == "<value>"` or `<path> != "<value>"`
fn evaluate_simple_condition(condition: &str, ctx: &Value) -> bool {
    // Parse: `path == "value"` or `path != "value"`
    let (path, expected, negate) = if let Some(idx) = condition.find("==") {
        let path = condition[..idx].trim();
        let val = condition[idx + 2..].trim().trim_matches('"');
        (path, val, false)
    } else if let Some(idx) = condition.find("!=") {
        let path = condition[..idx].trim();
        let val = condition[idx + 2..].trim().trim_matches('"');
        (path, val, true)
    } else {
        return false;
    };

    // Resolve dotted path against context (case-insensitive comparison)
    let actual = resolve_dotted_path(ctx, path);
    let matches = actual
        .as_deref()
        .is_some_and(|a| a.eq_ignore_ascii_case(expected));
    if negate { !matches } else { matches }
}

/// Resolve a dotted path like `response.action` against a JSON value.
fn resolve_dotted_path(value: &Value, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = value;
    for part in &parts {
        current = current.get(part)?;
    }
    match current {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => Some(current.to_string()),
    }
}

/// Build a context object for routing condition evaluation.
///
/// The context merges the node output with the flow entry so that conditions
/// can reference both component results and incoming message data.
///
/// Layout:
/// ```text
/// {
///   ...output.payload...,     // top-level fields from component output
///   "entry": <flow entry>,
///   "in":    <flow entry>,    // alias
///   "response": {             // synthesised from envelope metadata
///     <key>: <value>,         // e.g. "action": "about"
///     ...
///   }
/// }
/// ```
fn build_routing_context(output: &NodeOutput, state: &ExecutionState) -> Value {
    let mut ctx = match &output.payload {
        Value::Object(map) => map.clone(),
        _ => JsonMap::new(),
    };

    let entry = &state.entry;
    ctx.insert("entry".into(), entry.clone());
    ctx.insert("in".into(), entry.clone());

    // Synthesise "response" from the envelope metadata.
    // greentic-start demo path: entry.input.metadata.*
    // greentic-runner direct path: entry.metadata.*
    let metadata = entry
        .pointer("/input/metadata")
        .or_else(|| entry.pointer("/metadata"));

    let mut response = JsonMap::new();
    if let Some(Value::Object(meta)) = metadata {
        for (k, v) in meta {
            // Flatten string values; stringify others
            match v {
                Value::String(s) => {
                    response.insert(k.clone(), Value::String(s.clone()));
                }
                other => {
                    response.insert(k.clone(), other.clone());
                }
            }
        }
    }
    // Also pull text from the envelope for convenience
    if let Some(text) = entry
        .pointer("/input/text")
        .or_else(|| entry.pointer("/text"))
        .filter(|t| !t.is_null())
    {
        response.insert("text".into(), text.clone());
    }
    ctx.insert("response".into(), Value::Object(response));

    Value::Object(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{ValidationConfig, ValidationMode};
    use greentic_types::{
        Flow, FlowComponentRef, FlowId, FlowKind, InputMapping, Node, NodeId, OutputMapping,
        Routing, TelemetryHints,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap as StdHashMap};
    use std::str::FromStr;
    use std::sync::Mutex;
    use tokio::runtime::Runtime;

    fn minimal_engine() -> FlowEngine {
        FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::new()),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        }
    }

    #[test]
    fn templating_renders_with_partials_and_data() {
        let mut state = ExecutionState::new(json!({ "city": "London" }));
        state.nodes.insert(
            "forecast".to_string(),
            NodeOutput::new(json!({ "temp": "20C" })),
        );

        // templating context includes node outputs for runner-side payload rendering.
        let ctx = state.context();
        assert_eq!(ctx["nodes"]["forecast"]["payload"]["temp"], json!("20C"));
    }

    #[test]
    fn finalize_wraps_emitted_payloads() {
        let mut state = ExecutionState::new(json!({}));
        state.push_egress(json!({ "text": "first" }));
        state.push_egress(json!({ "text": "second" }));
        let result = state.finalize_with(Some(json!({ "text": "final" })));
        assert_eq!(
            result,
            json!([
                { "text": "first" },
                { "text": "second" },
                { "text": "final" }
            ])
        );
    }

    #[test]
    fn finalize_flattens_final_array() {
        let mut state = ExecutionState::new(json!({}));
        state.push_egress(json!({ "text": "only" }));
        let result = state.finalize_with(Some(json!([
            { "text": "extra-1" },
            { "text": "extra-2" }
        ])));
        assert_eq!(
            result,
            json!([
                { "text": "only" },
                { "text": "extra-1" },
                { "text": "extra-2" }
            ])
        );
    }

    #[test]
    fn inject_card_locale_uses_entry_metadata_without_overwriting_payload() {
        let mut payload = json!({
            "card_source": "inline",
            "card_spec": { "title": "Hello" }
        });
        inject_card_locale(
            &mut payload,
            &json!({"input": {"metadata": {"locale": "nl-NL"}}}),
        );
        assert_eq!(payload["locale"], json!("nl-NL"));

        let mut existing = json!({
            "card_source": "inline",
            "card_spec": { "title": "Hello" },
            "locale": "en-GB"
        });
        inject_card_locale(&mut existing, &json!({"metadata": {"locale": "nl-NL"}}));
        assert_eq!(existing["locale"], json!("en-GB"));
    }

    #[test]
    fn load_i18n_bundle_entries_reads_manifest_and_falls_back_to_en() {
        let assets = StdHashMap::from([
            (
                "cards/i18n/_manifest.json".to_string(),
                br#"{"locales":["de"]}"#.to_vec(),
            ),
            (
                "cards/i18n/de.json".to_string(),
                br#"{"title":"Hallo"}"#.to_vec(),
            ),
            (
                "cards/i18n/en.json".to_string(),
                br#"{"title":"Hello"}"#.to_vec(),
            ),
        ]);

        let entries = load_i18n_bundle_entries("cards/i18n", |path| {
            assets
                .get(path)
                .cloned()
                .with_context(|| format!("missing asset {path}"))
        });

        assert_eq!(entries["de"]["title"], json!("Hallo"));
        assert_eq!(entries["en"]["title"], json!("Hello"));
    }

    #[test]
    fn load_i18n_bundle_entries_reads_single_file_bundle() {
        let entries = load_i18n_bundle_entries("cards/i18n.json", |path| {
            if path == "cards/i18n.json" {
                Ok(br#"{"title":"Hello"}"#.to_vec())
            } else {
                bail!("unexpected asset {path}");
            }
        });

        assert_eq!(entries["en"]["title"], json!("Hello"));
    }

    struct TestCrossPackResolver;

    impl CrossPackResolver for TestCrossPackResolver {
        fn invoke(
            &self,
            provider_id: &str,
            provider_type: Option<&str>,
            op: &str,
            input: &[u8],
            tenant: &str,
            team: Option<&str>,
        ) -> Result<Value> {
            Ok(json!({
                "provider_id": provider_id,
                "provider_type": provider_type,
                "op": op,
                "tenant": tenant,
                "team": team,
                "input": serde_json::from_slice::<Value>(input)?,
            }))
        }
    }

    #[test]
    fn cross_pack_resolver_returns_node_output_when_present() {
        let mut engine = minimal_engine();
        engine.set_cross_pack_resolver(Arc::new(TestCrossPackResolver));

        let output = engine
            .try_invoke_cross_pack_resolver(
                Some("mail"),
                Some("messaging"),
                "send",
                br#"{"subject":"hello"}"#,
                "demo",
            )
            .expect("resolver invocation")
            .expect("resolver output");

        assert_eq!(
            output.payload,
            json!({
                "provider_id": "mail",
                "provider_type": "messaging",
                "op": "send",
                "tenant": "demo",
                "team": null,
                "input": { "subject": "hello" },
            })
        );
    }

    #[test]
    fn parse_component_control_ignores_plain_payload() {
        let payload = json!({
            "flow": "not-a-control-field",
            "node": "n1"
        });
        let control = parse_component_control(&payload).expect("parse control");
        assert!(control.is_none());
    }

    #[test]
    fn parse_component_control_parses_jump_marker() {
        let payload = json!({
            "greentic_control": {
                "action": "jump",
                "v": 1,
                "flow": "flow.b",
                "node": "node-2",
                "payload": { "message": "hi" },
                "hints": { "k": "v" },
                "max_redirects": 2,
                "reason": "handoff"
            }
        });
        let control = parse_component_control(&payload)
            .expect("parse control")
            .expect("missing control");
        match control {
            NodeControl::Jump(jump) => {
                assert_eq!(jump.flow, "flow.b");
                assert_eq!(jump.node.as_deref(), Some("node-2"));
                assert_eq!(jump.payload, json!({ "message": "hi" }));
                assert_eq!(jump.hints, json!({ "k": "v" }));
                assert_eq!(jump.max_redirects, Some(2));
                assert_eq!(jump.reason.as_deref(), Some("handoff"));
            }
            other => panic!("expected jump control, got {other:?}"),
        }
    }

    #[test]
    fn parse_component_control_rejects_invalid_marker() {
        let payload = json!({
            "greentic_control": "bad-shape"
        });
        let err = parse_component_control(&payload).expect_err("expected invalid marker error");
        assert!(err.to_string().contains("greentic_control"));
    }

    #[test]
    fn missing_operation_reports_node_and_component() {
        let engine = minimal_engine();
        let rt = Runtime::new().unwrap();
        let retry_config = RetryConfig {
            max_attempts: 1,
            base_delay_ms: 1,
        };
        let ctx = FlowContext {
            tenant: "tenant",
            pack_id: "test-pack",
            flow_id: "flow",
            node_id: Some("missing-op"),
            tool: None,
            action: None,
            session_id: None,
            provider_id: None,
            reply_scope: None,
            retry_config,
            attempt: 1,
            observer: None,
            mocks: None,
        };
        let node = HostNode {
            kind: NodeKind::Exec {
                target_component: "qa.process".into(),
            },
            component: "component.exec".into(),
            component_id: "component.exec".into(),
            operation_name: None,
            operation_in_mapping: None,
            payload_expr: Value::Null,
            routing: Routing::End,
        };
        let _state = ExecutionState::new(Value::Null);
        let payload = json!({ "component": "qa.process" });
        let event = NodeEvent {
            context: &ctx,
            node_id: "missing-op",
            node: &node,
            payload: &payload,
        };
        let err = rt
            .block_on(engine.execute_component_exec(
                &ctx,
                "missing-op",
                &node,
                payload.clone(),
                &event,
                ComponentOverrides {
                    component: None,
                    operation: None,
                },
            ))
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("missing operation for node `missing-op`"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("(component `component.exec`)"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn missing_operation_mentions_mapping_hint() {
        let engine = minimal_engine();
        let rt = Runtime::new().unwrap();
        let retry_config = RetryConfig {
            max_attempts: 1,
            base_delay_ms: 1,
        };
        let ctx = FlowContext {
            tenant: "tenant",
            pack_id: "test-pack",
            flow_id: "flow",
            node_id: Some("missing-op-hint"),
            tool: None,
            action: None,
            session_id: None,
            provider_id: None,
            reply_scope: None,
            retry_config,
            attempt: 1,
            observer: None,
            mocks: None,
        };
        let node = HostNode {
            kind: NodeKind::Exec {
                target_component: "qa.process".into(),
            },
            component: "component.exec".into(),
            component_id: "component.exec".into(),
            operation_name: None,
            operation_in_mapping: Some("render".into()),
            payload_expr: Value::Null,
            routing: Routing::End,
        };
        let _state = ExecutionState::new(Value::Null);
        let payload = json!({ "component": "qa.process" });
        let event = NodeEvent {
            context: &ctx,
            node_id: "missing-op-hint",
            node: &node,
            payload: &payload,
        };
        let err = rt
            .block_on(engine.execute_component_exec(
                &ctx,
                "missing-op-hint",
                &node,
                payload.clone(),
                &event,
                ComponentOverrides {
                    component: None,
                    operation: None,
                },
            ))
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("missing operation for node `missing-op-hint`"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains("Found operation in input.mapping (`render`)"),
            "unexpected message: {message}"
        );
    }

    struct CountingObserver {
        starts: Mutex<Vec<String>>,
        ends: Mutex<Vec<Value>>,
    }

    impl CountingObserver {
        fn new() -> Self {
            Self {
                starts: Mutex::new(Vec::new()),
                ends: Mutex::new(Vec::new()),
            }
        }
    }

    impl ExecutionObserver for CountingObserver {
        fn on_node_start(&self, event: &NodeEvent<'_>) {
            self.starts.lock().unwrap().push(event.node_id.to_string());
        }

        fn on_node_end(&self, _event: &NodeEvent<'_>, output: &Value) {
            self.ends.lock().unwrap().push(output.clone());
        }

        fn on_node_error(&self, _event: &NodeEvent<'_>, _error: &dyn StdError) {}
    }

    #[test]
    fn emits_end_event_for_successful_node() {
        let node_id = NodeId::from_str("emit").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "message": "logged" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("emit.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: Default::default(),
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "emit.flow".to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        };
        let observer = CountingObserver::new();
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "emit.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: None,
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: Some(&observer),
            mocks: None,
        };

        let rt = Runtime::new().unwrap();
        let result = rt.block_on(engine.execute(ctx, Value::Null)).unwrap();
        assert!(matches!(result.status, FlowStatus::Completed));

        let starts = observer.starts.lock().unwrap();
        let ends = observer.ends.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0], json!({ "message": "logged" }));
    }

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn dw_agent_node_routes_to_handler_and_returns_reply() {
        use crate::runner::agent_node::{AgentNodeHandler, RuntimeAgentNodeHandler};
        use greentic_aw_runtime::cost::MockTokenMeter;
        use greentic_aw_runtime::llm::LlmResponse;
        use greentic_aw_runtime::mock::{
            MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, NoopToolLedger,
        };
        use greentic_aw_runtime::{
            AgentConfig, AgentLimits, AgentRuntime, LlmProviderRef, TenantContext,
        };

        // --- mock-backed AgentRuntime: the LLM replies "pong" in one step ---
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content: Some("pong".into()),
            tool_calls: vec![],
            tokens_in: 1,
            tokens_out: 1,
        })]));
        let store = Arc::new(MockAgentStateStore::new());
        let telemetry = Arc::new(MockTelemetry::new());

        // The dispatch builds TenantContext::new(ctx.tenant, default_env) =
        // ("demo", "local"). MockConfigProvider keys by
        // `format!("{}:{agent_id}", tenant.key_prefix())` = "aw:demo:local:greeter",
        // so seed with the SAME tenant+env+agent_id the engine will look up.
        let config_provider = MockConfigProvider::new();
        let tenant = TenantContext::new("demo", "local");
        config_provider.insert(
            &tenant,
            "greeter",
            AgentConfig {
                agent_id: "greeter".into(),
                system_prompt: "sys".into(),
                tools: vec![],
                guardrails: vec![],
                llm: LlmProviderRef {
                    provider: "mock".into(),
                    model: "m".into(),
                    credential_ref: None,
                },
                limits: AgentLimits::default(),
                memory: None,
                knowledge: None,
            },
        );
        let config_provider = Arc::new(config_provider);
        let token_meter = Arc::new(MockTokenMeter::new(0));
        let ledger = Arc::new(NoopToolLedger);
        let ext_runtime = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let runtime = Arc::new(AgentRuntime::new(
            config_provider,
            store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
            None,
        ));
        let handler: Arc<dyn AgentNodeHandler> = Arc::new(RuntimeAgentNodeHandler::new(runtime));

        // --- flow with a single dw.agent node (operation = agent_id) ---
        let node_id = NodeId::from_str("agent").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "dw.agent".parse().unwrap(),
                pack_alias: None,
                operation: Some("greeter".to_string()),
            },
            input: InputMapping {
                mapping: json!({ "user_text": "ping" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("dw.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: Default::default(),
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "dw.flow".to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: Some(handler),
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        };
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "dw.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("sess-1"),
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: None,
            mocks: None,
        };

        let rt = Runtime::new().unwrap();
        let result = rt
            .block_on(engine.execute(ctx, json!({ "user_text": "ping" })))
            .unwrap();
        assert!(matches!(result.status, FlowStatus::Completed));

        // The dw.agent node output is {"reply", "trail", "terminated_by"}; the
        // engine finalises a single-node flow's egress into an array wrapping it.
        let output_str = serde_json::to_string(&result.output).unwrap();
        assert!(
            output_str.contains("pong"),
            "expected agent reply in flow output, got: {output_str}"
        );
    }

    /// Engine twin of [`dw_agent_node_routes_to_handler_and_returns_reply`]:
    /// asserts a `dw.agent_graph` node is detected, routed to the configured
    /// [`GraphNodeHandler`] with the engine-derived tenant/env/session and the
    /// node's `operation` as the `graph_id`, and its reply lands in the flow
    /// output. A lightweight recording stub stands in for the durable executor.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn dw_agent_graph_node_routes_to_handler_and_returns_reply() {
        use std::sync::Mutex;

        use crate::runner::graph_node::GraphNodeHandler;

        /// Records the dispatch arguments and returns a fixed DwAgent envelope.
        struct RecordingGraphHandler {
            seen: Mutex<Option<(String, String, String, String)>>,
        }

        #[async_trait::async_trait]
        impl GraphNodeHandler for RecordingGraphHandler {
            async fn execute(
                &self,
                tenant_id: &str,
                env_id: &str,
                graph_id: &str,
                session_id: &str,
                _flow_input: &Value,
            ) -> Result<Value> {
                *self.seen.lock().unwrap() = Some((
                    tenant_id.to_string(),
                    env_id.to_string(),
                    graph_id.to_string(),
                    session_id.to_string(),
                ));
                Ok(json!({
                    "reply": "graph-pong",
                    "trail": [],
                    "terminated_by": "respond",
                }))
            }
        }

        let handler = Arc::new(RecordingGraphHandler {
            seen: Mutex::new(None),
        });
        let handler_dyn: Arc<dyn GraphNodeHandler> = handler.clone();

        // --- flow with a single dw.agent_graph node (operation = graph_id) ---
        let node_id = NodeId::from_str("graph").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "dw.agent_graph".parse().unwrap(),
                pack_alias: None,
                operation: Some("triage".to_string()),
            },
            input: InputMapping {
                mapping: json!({ "user_text": "ping" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("dwg.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: Default::default(),
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "dwg.flow".to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: Some(handler_dyn),
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        };
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "dwg.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("sess-1"),
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: None,
            mocks: None,
        };

        let rt = Runtime::new().unwrap();
        let result = rt
            .block_on(engine.execute(ctx, json!({ "user_text": "ping" })))
            .unwrap();
        assert!(matches!(result.status, FlowStatus::Completed));

        // The handler must have been called with the engine-derived
        // tenant/env/session and the node's operation as graph_id.
        let seen = handler.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            Some((
                "demo".to_string(),
                "local".to_string(),
                "triage".to_string(),
                "sess-1".to_string(),
            )),
            "dw.agent_graph dispatch must mirror dw.agent's tenant/env/graph_id/session derivation"
        );

        let output_str = serde_json::to_string(&result.output).unwrap();
        assert!(
            output_str.contains("graph-pong"),
            "expected graph reply in flow output, got: {output_str}"
        );
    }

    /// When `GREENTIC_AW_DISPATCH=nats` is set, a `dw.agent` node must be
    /// rerouted through the remote-dispatch path (`"agentic"` runtime) rather
    /// than calling the in-process `AgentNodeHandler`. The node payload is
    /// wrapped as `input`, `await=true` is injected, and the engine pauses
    /// (returns a wait outcome, not a complete one).
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn dw_agent_nats_mode_dispatches_remote() {
        use std::sync::Mutex;

        use crate::runner::agent_node::DwAgentDispatch;
        use crate::runner::remote_dispatch::{
            RemoteDispatch, RemoteDispatchAction, RemoteDispatchHandler,
        };

        /// Recording stub: captures the last dispatch and returns
        /// `AwaitingResponse` so the engine pauses.
        struct RecordingDispatcher {
            seen: Mutex<Option<RemoteDispatch>>,
        }

        #[async_trait::async_trait]
        impl RemoteDispatchHandler for RecordingDispatcher {
            async fn dispatch(
                &self,
                request: RemoteDispatch,
            ) -> anyhow::Result<RemoteDispatchAction> {
                let corr = request.correlation_id.clone();
                *self.seen.lock().unwrap() = Some(request);
                Ok(RemoteDispatchAction::AwaitingResponse {
                    correlation_id: corr,
                })
            }
        }

        let dispatcher = Arc::new(RecordingDispatcher {
            seen: Mutex::new(None),
        });

        // --- two-node flow: dw.agent → emit (resume target) ---
        // The agent node must have Routing::Next so the engine knows where to
        // resume once the async response arrives (same requirement as sorla.call /
        // agentic.call nodes in production).
        let resume_id = NodeId::from_str("after-agent").unwrap();
        let node_id = NodeId::from_str("agent-nats").unwrap();
        let agent_node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "dw.agent".parse().unwrap(),
                pack_alias: None,
                operation: Some("greeter".to_string()),
            },
            input: InputMapping {
                mapping: json!({ "user_text": "hi" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: resume_id.clone(),
            },
            telemetry: TelemetryHints::default(),
        };
        let resume_node = Node {
            id: resume_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "message": "done" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), agent_node);
        nodes.insert(resume_id.clone(), resume_node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("nats-agent.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: Default::default(),
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "nats-agent.flow".to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: Some(dispatcher.clone() as Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>),
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: DwAgentDispatch::Nats,
            #[cfg(feature = "agentic-worker")]
            // No in-process handler wired — Nats path must NOT call it.
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        };

        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "nats-agent.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("sess-nats"),
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: None,
            mocks: None,
        };

        let rt = Runtime::new().unwrap();
        let result = rt
            .block_on(engine.execute(ctx, json!({ "user_text": "hi" })))
            .unwrap();

        // The Nats path pauses the flow (await=true → DispatchOutcome::wait).
        assert!(
            matches!(result.status, FlowStatus::Waiting(_)),
            "expected Waiting outcome from dw.agent Nats mode, got: {:?}",
            result.status
        );

        // The dispatcher must have been called with runtime="agentic" and
        // target=<agent_id>, and the node payload wrapped as `input`.
        let seen = dispatcher.seen.lock().unwrap();
        let dispatch = seen.as_ref().expect("dispatcher was not called");
        assert_eq!(
            dispatch.runtime, "agentic",
            "runtime name must be 'agentic'"
        );
        assert_eq!(dispatch.target, "greeter", "target must be the agent_id");
        assert_eq!(
            dispatch.input,
            json!({ "user_text": "hi" }),
            "node payload must be forwarded as dispatch input"
        );
    }

    fn host_flow_for_test(
        flow_id: &str,
        node_ids: &[&str],
        default_start: Option<&str>,
    ) -> HostFlow {
        let mut nodes = indexmap::IndexMap::default();
        for node_id in node_ids {
            let id = NodeId::from_str(node_id).unwrap();
            let node = Node {
                id: id.clone(),
                component: FlowComponentRef {
                    id: "emit.log".parse().unwrap(),
                    pack_alias: None,
                    operation: None,
                },
                input: InputMapping {
                    mapping: json!({ "message": node_id }),
                },
                output: OutputMapping {
                    mapping: Value::Null,
                },
                err_map: None,
                routing: Routing::End,
                telemetry: TelemetryHints::default(),
            };
            nodes.insert(id, node);
        }
        let mut entrypoints = BTreeMap::new();
        if let Some(start) = default_start {
            entrypoints.insert("default".to_string(), Value::String(start.to_string()));
        }
        HostFlow::from(Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str(flow_id).unwrap(),
            kind: FlowKind::Messaging,
            entrypoints,
            nodes,
            metadata: Default::default(),
        })
    }

    fn jump_test_engine() -> FlowEngine {
        let target_flow = host_flow_for_test("flow.target", &["node-a", "node-b"], None);
        FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "flow.target".to_string(),
                },
                target_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
        }
    }

    fn jump_ctx<'a>(flow_id: &'a str) -> FlowContext<'a> {
        FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id,
            node_id: None,
            tool: None,
            action: None,
            session_id: None,
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: None,
            mocks: None,
        }
    }

    #[test]
    fn apply_jump_unknown_flow_errors() {
        let engine = minimal_engine();
        let mut state = ExecutionState::new(Value::Null);
        let rt = Runtime::new().unwrap();
        let err = rt
            .block_on(engine.apply_jump(
                &jump_ctx("flow.source"),
                &mut state,
                JumpControl {
                    flow: "flow.missing".into(),
                    node: None,
                    payload: json!({ "ok": true }),
                    hints: Value::Null,
                    max_redirects: None,
                    reason: None,
                },
            ))
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown_flow"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn apply_jump_unknown_node_errors() {
        let engine = jump_test_engine();
        let mut state = ExecutionState::new(Value::Null);
        let rt = Runtime::new().unwrap();
        let err = rt
            .block_on(engine.apply_jump(
                &jump_ctx("flow.source"),
                &mut state,
                JumpControl {
                    flow: "flow.target".into(),
                    node: Some("node-missing".into()),
                    payload: json!({ "ok": true }),
                    hints: Value::Null,
                    max_redirects: None,
                    reason: None,
                },
            ))
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown_node"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn apply_jump_uses_default_start_fallback() {
        let engine = jump_test_engine();
        let mut state = ExecutionState::new(Value::Null);
        let rt = Runtime::new().unwrap();
        let target = rt
            .block_on(engine.apply_jump(
                &jump_ctx("flow.source"),
                &mut state,
                JumpControl {
                    flow: "flow.target".into(),
                    node: None,
                    payload: json!({ "k": "v" }),
                    hints: Value::Null,
                    max_redirects: None,
                    reason: None,
                },
            ))
            .expect("jump target");
        assert_eq!(target.flow_id, "flow.target");
        assert_eq!(target.node_id.as_str(), "node-a");
    }

    #[test]
    fn apply_jump_redirect_limit_enforced() {
        let engine = jump_test_engine();
        let mut state = ExecutionState::new(Value::Null);
        state.redirect_count = 3;
        let rt = Runtime::new().unwrap();
        let err = rt
            .block_on(engine.apply_jump(
                &jump_ctx("flow.source"),
                &mut state,
                JumpControl {
                    flow: "flow.target".into(),
                    node: None,
                    payload: json!({ "k": "v" }),
                    hints: Value::Null,
                    max_redirects: Some(3),
                    reason: None,
                },
            ))
            .unwrap_err();
        assert_eq!(err.to_string(), "redirect_limit");
    }

    /// Regression: a `Routing::Custom` array containing at least one
    /// conditional entry must pause (return `Wait`) when no condition
    /// matches, instead of terminating. Concrete bug it guards against:
    /// every card click used to terminate the flow because the entry-card's
    /// routing array didn't enumerate every downstream action, so users got
    /// looped back to the entry on every interaction.
    #[test]
    fn evaluate_custom_routing_waits_when_conditional_falls_through() {
        let raw_routing = json!([
            { "condition": "response.action == \"go\"", "to": "next" },
            { "out": true }
        ]);
        let flow_ir = HostFlow {
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
        };
        let current_node = NodeId::from_str("current").unwrap();
        let output = NodeOutput::new(Value::Null);

        // First case: empty action -> conditional does not match, must wait.
        let mut state_empty = ExecutionState::new(json!({ "metadata": { "action": "" } }));
        state_empty.entry = json!({ "metadata": { "action": "" } });
        let decision_empty =
            evaluate_custom_routing(&raw_routing, &output, &state_empty, &flow_ir, &current_node);
        assert!(
            matches!(decision_empty, CustomRoutingDecision::Wait),
            "expected Wait on conditional fall-through, got {decision_empty:?}"
        );

        // Second case: action == "go" -> conditional matches, must advance.
        let mut state_go = ExecutionState::new(json!({ "metadata": { "action": "go" } }));
        state_go.entry = json!({ "metadata": { "action": "go" } });
        let decision_go =
            evaluate_custom_routing(&raw_routing, &output, &state_go, &flow_ir, &current_node);
        match decision_go {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "next"),
            other => panic!("expected Next(\"next\"), got {other:?}"),
        }
    }

    /// Live end-to-end test: `dw.agent` NATS dispatch path.
    ///
    /// Requires a real NATS server (JetStream not needed for this test — core
    /// NATS pub/sub is sufficient) and an `aw-serve` consumer (or the in-process
    /// fake bridge below acts as one).
    ///
    /// # Run recipe
    ///
    /// ```text
    /// # Terminal 1 – NATS server (JetStream-enabled for prod parity, but core works too)
    /// nats-server -js
    ///
    /// # Terminal 2 – aw-serve test-mock (replies "pong" for any agent)
    /// AW_SERVE_AGENT_ID=greeter AW_SERVE_REPLY=pong \
    ///   GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 \
    ///   GREENTIC_AW_JETSTREAM=off \
    ///   cargo run -p greentic-aw-runtime --features serve,test-mock --bin aw-serve
    ///
    /// # Terminal 3 – run this ignored test
    /// GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 \
    ///   cargo test -p greentic-runner-host --lib \
    ///   tests::dw_agent_scale_to_zero_nats_e2e \
    ///   -- --nocapture --ignored
    /// ```
    ///
    /// When `GREENTIC_EVENTS_NATS_URL` is unset the test skips immediately.
    /// The test wires its own in-process fake bridge so the `aw-serve` binary is
    /// optional; running with the real `aw-serve` exercises the full out-of-process
    /// path. Both variants must produce a resumed reply of `"pong"`.
    #[cfg(feature = "agentic-worker")]
    #[tokio::test]
    #[ignore = "requires live NATS; run with --ignored after `nats-server -js`"]
    async fn dw_agent_scale_to_zero_nats_e2e() {
        use crate::runner::agent_node::DwAgentDispatch;
        use crate::runner::dispatch_listener::{SessionResumer, run_response_listener};
        use crate::runner::remote_dispatch::NatsDispatcher;
        use futures::StreamExt as _;
        use greentic_types::{
            RuntimeDispatchResponse, TenantCtx as DispatchTenantCtx, request_topic, response_topic,
        };
        use tokio::sync::Notify;

        let nats_url = match std::env::var("GREENTIC_EVENTS_NATS_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!(
                    "skipping dw_agent_scale_to_zero_nats_e2e: GREENTIC_EVENTS_NATS_URL not set"
                );
                return;
            }
        };

        // ── 1. Build a two-node flow: dw.agent → emit.log (resume target) ──
        // The agent node must have Routing::Next so the engine knows the resume
        // target (same requirement as agentic.call / sorla.call in production).
        let resume_id = NodeId::from_str("after-agent").unwrap();
        let agent_node_id = NodeId::from_str("agent-e2e").unwrap();
        let agent_node = Node {
            id: agent_node_id.clone(),
            component: FlowComponentRef {
                id: "dw.agent".parse().unwrap(),
                pack_alias: None,
                operation: Some("greeter".to_string()),
            },
            input: InputMapping {
                mapping: json!({ "user_text": "ping" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: resume_id.clone(),
            },
            telemetry: TelemetryHints::default(),
        };
        let resume_node = Node {
            id: resume_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "message": "resumed" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(agent_node_id.clone(), agent_node);
        nodes.insert(resume_id.clone(), resume_node);
        let flow = greentic_types::Flow {
            schema_version: "1.0".into(),
            id: greentic_types::FlowId::from_str("e2e-agent.flow").unwrap(),
            kind: greentic_types::FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(agent_node_id.to_string()),
            )]),
            nodes,
            metadata: Default::default(),
        };
        let host_flow = HostFlow::from(flow);

        // ── 2. Connect NATS clients ──
        let dispatcher_client = async_nats::connect(&nats_url)
            .await
            .expect("NATS: dispatcher client");
        let bridge_client = async_nats::connect(&nats_url)
            .await
            .expect("NATS: fake bridge client");
        let listener_client = async_nats::connect(&nats_url)
            .await
            .expect("NATS: response listener client");

        // ── 3. Fake bridge: subscribe to agentic request subject, reply "pong" ──
        let agentic_request_subject = request_topic("agentic");
        let agentic_response_subject = response_topic("agentic");
        let mut req_sub = bridge_client
            .subscribe(agentic_request_subject.clone())
            .await
            .expect("fake bridge: subscribe to agentic request subject");
        let bridge_reply_client = bridge_client.clone();
        let reply_subject = agentic_response_subject.clone();
        tokio::spawn(async move {
            while let Some(msg) = req_sub.next().await {
                let headers = msg.headers.as_ref();
                let get_hdr = |name: &str| {
                    headers
                        .and_then(|h| h.get(name))
                        .map(|v| v.as_str().to_owned())
                        .unwrap_or_default()
                };
                let correlation_id = get_hdr("Greentic-Correlation-Id");
                let tenant = get_hdr("Greentic-Tenant");
                let env = get_hdr("Greentic-Env");

                let response_payload = RuntimeDispatchResponse {
                    ok: true,
                    output: json!({
                        "reply": "pong",
                        "trail": [],
                        "terminated_by": "final_reply"
                    }),
                    events: vec![],
                    error: None,
                };
                let body =
                    serde_json::to_vec(&response_payload).expect("serialize fake bridge response");

                let mut resp_headers = async_nats::HeaderMap::new();
                resp_headers.insert("Greentic-Correlation-Id", correlation_id.as_str());
                resp_headers.insert("Greentic-Tenant", tenant.as_str());
                resp_headers.insert("Greentic-Env", env.as_str());

                bridge_reply_client
                    .publish_with_headers(reply_subject.clone(), resp_headers, body.into())
                    .await
                    .expect("fake bridge: publish response");
            }
        });

        // ── 4. Recording resumer + run_response_listener ──
        struct RecordingResumer {
            calls: std::sync::Mutex<Vec<(String, Value)>>,
            notify: Notify,
        }

        impl RecordingResumer {
            fn new() -> Self {
                Self {
                    calls: std::sync::Mutex::new(vec![]),
                    notify: Notify::new(),
                }
            }
        }

        #[async_trait::async_trait]
        impl SessionResumer for RecordingResumer {
            async fn resume(
                &self,
                _tenant: DispatchTenantCtx,
                correlation_id: &str,
                output: Value,
            ) -> anyhow::Result<()> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((correlation_id.to_string(), output));
                self.notify.notify_one();
                Ok(())
            }
        }

        let resumer = Arc::new(RecordingResumer::new());
        let resumer_for_listener = resumer.clone();
        tokio::spawn(async move {
            run_response_listener(listener_client, "agentic".to_owned(), resumer_for_listener)
                .await
                .expect("response listener exited unexpectedly");
        });

        // Give subscriptions a moment to register.
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        // ── 5. Build FlowEngine with NatsDispatcher + DwAgentDispatch::Nats ──
        let nats_engine_dispatcher = Arc::new(NatsDispatcher::new(dispatcher_client));
        let engine = FlowEngine {
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "e2e-pack".to_string(),
                    flow_id: "e2e-agent.flow".to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: crate::validate::ValidationConfig {
                mode: crate::validate::ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: Some(
                nats_engine_dispatcher
                    as Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>,
            ),
            dw_agent_dispatch: DwAgentDispatch::Nats,
            agent_node_handler: None,
            graph_node_handler: None,
            mcp_tool_source: None,
        };

        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "e2e-pack",
            flow_id: "e2e-agent.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("e2e-sess-1"),
            provider_id: None,
            reply_scope: None,
            retry_config: RetryConfig {
                max_attempts: 1,
                base_delay_ms: 1,
            },
            attempt: 1,
            observer: None,
            mocks: None,
        };

        // ── 6. Execute: the dw.agent NATS path must PAUSE the flow ──
        let result = engine
            .execute(ctx, json!({ "user_text": "ping" }))
            .await
            .expect("engine.execute succeeded");

        assert!(
            matches!(result.status, FlowStatus::Waiting(_)),
            "expected FlowStatus::Waiting from dw.agent Nats path, got: {:?}",
            result.status
        );
        eprintln!("dw.agent: flow paused (Waiting) — dispatch published to NATS");

        // ── 7. Wait for the fake bridge reply to reach the resumer (up to 5 s) ──
        let wait = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            resumer.notify.notified(),
        )
        .await;

        assert!(
            wait.is_ok(),
            "timed out waiting for fake bridge reply — is NATS running? ({nats_url})"
        );

        // ── 8. Assert the resumed reply == "pong" ──
        let calls = resumer.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "resumer should have been called exactly once"
        );
        let (ref _corr, ref output) = calls[0];
        assert_eq!(
            output["output"]["reply"],
            json!("pong"),
            "resumed reply must match the aw-serve canned reply"
        );
        eprintln!(
            "PASSED: dw.agent scale-to-zero NATS e2e — reply={:?}",
            output["output"]["reply"]
        );
    }
}

use tracing::Instrument;

pub struct FlowContext<'a> {
    pub tenant: &'a str,
    pub pack_id: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub action: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    /// Reply scope of the originating inbound activity, when known.
    ///
    /// Carried so async-dispatch nodes (`sorla.call await`) can encode the
    /// inbound `thread`/`reply_to` into the published correlation id. Without
    /// it, a wait saved against a threaded scope cannot be re-keyed on resume
    /// (the resumer would synthesize an empty thread/reply_to and miss the
    /// saved wait). See `execute_sorla_call` and `RuntimeSessionResumer`.
    pub reply_scope: Option<&'a greentic_types::ReplyScope>,
    pub retry_config: RetryConfig,
    pub attempt: u32,
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
    lower.contains("transient")
        || lower.contains("unavailable")
        || lower.contains("internal")
        || lower.contains("timeout")
}

impl From<FlowRetryConfig> for RetryConfig {
    fn from(value: FlowRetryConfig) -> Self {
        Self {
            max_attempts: value.max_attempts.max(1),
            base_delay_ms: value.base_delay_ms.max(50),
        }
    }
}
