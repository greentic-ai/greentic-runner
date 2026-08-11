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
use crate::telemetry::{
    FlowSpanAttributes, RolloutIds, annotate_span, backoff_delay_ms, set_flow_context,
};
#[cfg(feature = "fault-injection")]
use crate::testing::fault_injection::{FaultContext, FaultPoint, maybe_fail};
use crate::validate::{
    ValidationConfig, ValidationIssue, ValidationMode, validate_component_envelope,
    validate_tool_envelope,
};
use greentic_flow::SLOT_SCHEMA_METADATA_KEY;
use greentic_types::{Flow, Node, NodeId, Routing};

/// Component ID of the slot-extractor WASM component. Used to detect
/// slot-extractor nodes and inject flow-level `slot_schema` as
/// `slot_definitions` into the invocation payload (Phase D).
const SLOT_EXTRACTOR_COMPONENT_ID: &str = "ai.greentic.component-slot-extractor";

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
    /// Rollout identifiers of the revision-keyed runtime this engine belongs to,
    /// stamped onto every per-invocation `TenantCtx` for telemetry attribution
    /// (C5.4). Empty for tenant-only (legacy) runtimes; the Phase-D revision
    /// dispatcher supplies real IDs via [`with_rollout_ids`](Self::with_rollout_ids).
    rollout_ids: RolloutIds,
    /// Bridges `sorla.call` flow nodes into a separate runtime over pub/sub.
    /// Not feature-gated: `sorla.call` is a core runtime-dispatch node.
    remote_dispatch_handler: Option<Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>>,
    /// Bridges `operala.call` flow nodes into an in-process deep-worker
    /// runtime (see `runner::operala_node`). Not feature-gated (like
    /// `remote_dispatch_handler`): `operala.call` is a core runtime-dispatch
    /// node and the trait itself has no feature-gated dependencies — only the
    /// concrete production impl (built under `desktop-agent-ephemeral` in
    /// `runtime.rs`) does. `None` falls back to the existing NATS
    /// `RemoteDispatchHandler` path (`execute_remote_dispatch`).
    operala_node_handler: Option<Arc<dyn crate::runner::operala_node::OperalaNodeHandler>>,
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
    /// Per-node outputs for this turn (`node_id → node_output_view(payload)`),
    /// captured from the flow's `ExecutionState` before it is finalised.
    /// Observability only — never affects `output`/egress.
    pub node_outputs: JsonMap<String, Value>,
}

#[derive(Clone, Debug)]
struct HostFlow {
    id: String,
    start: Option<NodeId>,
    nodes: IndexMap<NodeId, HostNode>,
    vars_init: JsonMap<String, Value>,
    /// Names of `vars_init` declarations whose decl has `"required": true`.
    /// Parsed from `metadata.extra.vars_init`; order follows the map iteration.
    /// Not yet read by production dispatch code — this is Task 1 of the
    /// "required flow-var fail-fast" plan; a later task consumes it to reject
    /// flow execution when a required var has no value at start. Exercised by
    /// `from_flow_collects_required_vars` in the meantime.
    #[allow(dead_code)]
    required_vars: Vec<String>,
    /// Flow-level slot definitions extracted from `metadata.extra["greentic.slot_schema"]`.
    /// Injected into slot-extractor component invocations at dispatch time (Phase D).
    slot_schema: Option<Value>,
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
    /// Per-node implicit output bindings: after this node runs, each entry
    /// `{ varName: template }` is rendered against a context where `prev` is
    /// the node's own output payload, and the result is written to
    /// `ExecutionState.vars[varName]`.
    vars_out: Option<JsonMap<String, Value>>,
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

#[cfg(test)]
impl HostNode {
    /// Test-only constructor. `HostNode`'s fields (and `NodeKind`/`Routing`
    /// literals) are private to this module, so sibling-module unit tests
    /// (e.g. `trace::recorder`) that need to build a `NodeEvent` for the
    /// `ExecutionObserver` trait cannot construct one via struct literal.
    /// This is additive test scaffolding only — no production behavior change.
    pub(crate) fn for_test(component_id: &str, operation_name: Option<&str>) -> Self {
        HostNode {
            kind: NodeKind::Exec {
                target_component: component_id.to_string(),
            },
            component: component_id.to_string(),
            component_id: component_id.to_string(),
            operation_name: operation_name.map(str::to_string),
            operation_in_mapping: None,
            payload_expr: Value::Null,
            routing: Routing::End,
            vars_out: None,
        }
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
    /// Session-scoped variable write: renders `value` against the current
    /// template context and inserts into `ExecutionState.vars[name]`.
    /// Config shape: `{ name: String, value: any }`.
    VarSet {
        name: String,
        value: Value,
    },
    Wait,
    DwAgent {
        agent_id: String,
        /// SP2: opt into multi-turn conversation-segment park-loop behaviour.
        /// SP3 will populate this from the flow doc; the loader defaults it false.
        conversational: bool,
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
    /// Native runtime-dispatch node for the Human-in-the-Loop approval runtime.
    /// Mirrors [`SorlaCall`] but routes to the `"approval"` runtime name and
    /// applies an autonomy gate (auto-approve below the configured risk /
    /// above the configured confidence) before dispatching.
    ApprovalCall {
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
    /// Whether the originating node has an `on_error`-family route, so a
    /// component failure is surfaced as a node_io `{errors}` output and routed
    /// to that branch instead of aborting the flow (see `node_has_error_route`).
    has_error_route: bool,
}

impl FlowExecution {
    fn completed(output: Value, node_outputs: JsonMap<String, Value>) -> Self {
        Self {
            output,
            status: FlowStatus::Completed,
            node_outputs,
        }
    }

    fn waiting(output: Value, wait: FlowWait, node_outputs: JsonMap<String, Value>) -> Self {
        Self {
            output,
            status: FlowStatus::Waiting(Box::new(wait)),
            node_outputs,
        }
    }
}

/// What a chat user is told when a flow fails terminally.
///
/// Deliberately generic. The engine's own error text stays in
/// `metadata.error_message` for logs, the conformance harness and richer
/// clients — putting it in `text` is what the surrounding code exists to
/// prevent ("instead of leaking raw engine text to the chat").
///
/// It is NOT optional, and that is the point. A terminal failure on a session
/// flow used to be reported as an `Ok` whose payload carried metadata ONLY, and
/// every messaging provider refuses a payload with no `text`/card:
/// `messaging-provider-telegram` returns `"text required"`, slack/teams/whatsapp
/// /email/webex behave the same, and even `messaging-provider-webchat` — the one
/// provider with an `extract_error_envelope` error-card path — rejects it at its
/// `"text, adaptive_card, attachments, or extensions required"` guard BEFORE
/// that path is reached. So a failing flow was silent on every channel: no
/// reply, no error, nothing. The worst case is an `approval.call` gate with
/// `mode: always`, which must reach a human and instead failed invisibly.
///
/// webchat still renders its styled card rather than this string: it checks the
/// envelope first and only falls back to `text`. This value is what the other
/// twelve providers send.
///
/// Not localized. The engine has no per-conversation locale here, so a tenant
/// serving non-English users sees English. Worth fixing by threading locale or
/// making this host-configurable; silence was worse.
const USER_FACING_FLOW_FAILURE_TEXT: &str =
    "Sorry — something went wrong while handling that. Please try again.";

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
            rollout_ids: RolloutIds::default(),
            operala_node_handler: None,
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

    /// Bind the rollout identifiers of the revision-keyed runtime this engine
    /// serves, so every invocation's telemetry carries deployment/bundle/
    /// revision attribution (C5.4). Called by the Phase-D revision dispatcher
    /// when it constructs a revision runtime; tenant-only runtimes leave the
    /// default (empty) IDs.
    pub fn with_rollout_ids(mut self, rollout_ids: RolloutIds) -> Self {
        self.rollout_ids = rollout_ids;
        self
    }

    /// The rollout identifiers bound to this engine (read counterpart to
    /// [`with_rollout_ids`](Self::with_rollout_ids)). Empty by default for the
    /// legacy tenant-only path.
    pub fn rollout_ids(&self) -> &RolloutIds {
        &self.rollout_ids
    }

    /// Use an MCP tool source the embedding host built, in place of the one
    /// [`FlowEngine::new`] derives from the process-global `GREENTIC_AW_*`
    /// environment.
    ///
    /// Env can only ever name ONE tenant, so a host that serves many tenants
    /// from one process cannot express its per-tenant catalog that way. Such a
    /// host builds a source per tenant (see
    /// `greentic_aw_runtime::McpCallerIdentity`) and injects it here.
    ///
    /// `None` LEAVES THE ENV-DERIVED SOURCE IN PLACE rather than clearing it —
    /// callers thread this straight through from a host that may not have one,
    /// and the standalone runner must keep working off its environment. Use
    /// `GREENTIC_AW_MCP=0` to disable MCP outright.
    #[cfg(feature = "agentic-worker")]
    pub fn with_mcp_source(
        mut self,
        source: Option<Arc<greentic_aw_runtime::McpToolSource>>,
    ) -> Self {
        if let Some(source) = source {
            self.mcp_tool_source = Some(source);
        }
        self
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

    /// Set the handler that bridges `operala.call` flow nodes into an
    /// in-process deep-worker runtime. Constructed by the runner binary
    /// (`runtime.rs`, `desktop-agent-ephemeral` feature) so `operala.call`
    /// nodes run with no NATS in that build. Mirrors [`set_agent_node_handler`].
    ///
    /// [`set_agent_node_handler`]: FlowEngine::set_agent_node_handler
    pub fn set_operala_node_handler(
        &mut self,
        handler: Arc<dyn crate::runner::operala_node::OperalaNodeHandler>,
    ) {
        self.operala_node_handler = Some(handler);
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

    /// Create the `flow.execute` span and install per-invocation telemetry:
    /// declared span fields, the task-local tenant context, and the **exported**
    /// `gt.*` attribution — the live `pack_id` plus any rollout identifiers from
    /// the owning revision runtime (C5.4). Returned for the caller to
    /// `.instrument()`. Both `execute` and `resume` route through here so every
    /// per-invocation entry point carries the same attribution.
    fn flow_execute_span(&self, ctx: &FlowContext<'_>) -> tracing::Span {
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
            &span,
            &self.default_env,
            ctx.tenant,
            ctx.flow_id,
            ctx.node_id,
            ctx.provider_id,
            ctx.session_id,
            ctx.pack_id,
            &self.rollout_ids,
        );
        span
    }

    pub async fn execute(&self, ctx: FlowContext<'_>, input: Value) -> Result<FlowExecution> {
        let span = self.flow_execute_span(&ctx);
        let retry_config = ctx.retry_config;
        let original_input = input;
        let mut ctx = ctx;
        let metric_tenant = ctx.tenant.to_string();
        let metric_flow_id = ctx.flow_id.to_string();
        let started = std::time::Instant::now();
        let result = async move {
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
                            // User-facing session flows surface the terminal
                            // error as a metadata-only Ok envelope so the
                            // messaging provider renders it instead of leaking
                            // raw engine text to the chat.
                            if ctx.session_id.is_some() {
                                return Ok(FlowExecution::completed(
                                    json!({
                                        "text": USER_FACING_FLOW_FAILURE_TEXT,
                                        // Generic, user-safe, and REQUIRED for the
                                        // failure to reach anyone. See
                                        // `USER_FACING_FLOW_FAILURE_TEXT`.
                                        "metadata": {
                                            "error_kind": "flow_execution_failed",
                                            "error_message": err.to_string(),
                                            "flow_id": ctx.flow_id,
                                        }
                                    }),
                                    Default::default(),
                                ));
                            }
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
        .await;
        let status = if result.is_ok() { "ok" } else { "err" };
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        crate::metrics::record_flow_execution(&metric_tenant, &metric_flow_id, status, duration_ms);
        result
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
        let span = self.flow_execute_span(&ctx);
        self.drive_flow(&ctx, flow_ir, state, Some(snapshot.next_node), resume_flow)
            .instrument(span)
            .await
    }

    async fn execute_once(&self, ctx: &FlowContext<'_>, input: Value) -> Result<FlowExecution> {
        let flow_ir = self.get_or_load_flow(ctx.pack_id, ctx.flow_id).await?;
        let mut state = ExecutionState::new(input);
        let missing = seed_vars_and_collect_missing_required(
            &flow_ir.vars_init,
            &flow_ir.required_vars,
            &mut state.vars,
        );
        if !missing.is_empty() {
            // Non-retryable by design: the message avoids should_retry's trigger
            // words (transient/unavailable/internal/timeout).
            let label = crate::runner::i18n::resolve_message(
                "runner.flow.required_var_missing",
                "required flow variable not provided",
                "en",
            );
            anyhow::bail!("{label}: {}", missing.join(", "));
        }
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
            let mut payload =
                render_template_value(&payload_template, &ctx_value, TemplateOptions::default())
                    .context("failed to render node input template")?;
            let node_id = current.clone();

            // Phase D: inject flow-level slot_schema as slot_definitions into
            // the slot-extractor's input when the author omitted inline
            // definitions. Explicit inline `slot_definitions` win
            // (back-compat with M2.4 NDA demo).
            if let NodeKind::Exec { target_component } = &node.kind
                && target_component == SLOT_EXTRACTOR_COMPONENT_ID
                && let Some(schema) = flow_ir.slot_schema.as_ref()
                && let Some(map) = payload.as_object_mut()
            {
                let input = map.entry("input").or_insert(Value::Null);
                inject_slot_definitions(input, schema, step_ctx.flow_id, node_id.as_str());
            }

            let observed_payload = payload.clone();
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
                    // Propagate so `execute()`'s retry loop can retry transient
                    // failures, then convert to a metadata-only Ok envelope at
                    // the top level once retries are exhausted (session flows).
                    return Err(err);
                }
            };

            state.nodes.insert(node_id.clone().into(), output.clone());
            state.last_output = Some(output.payload.clone());
            // Apply per-node vars_out bindings: render each template against a
            // context where `prev` is this node's own output payload (already
            // stored in state.last_output above), then write into state.vars.
            if let Some(bindings) = node.vars_out.as_ref() {
                let ctx = template_context(&state, output.payload.clone());
                for (var_name, template) in bindings.iter() {
                    let rendered = render_template_value(
                        template,
                        &ctx,
                        TemplateOptions {
                            allow_pointer: true,
                        },
                    )
                    .with_context(|| format!("failed to render vars_out binding `{var_name}`"))?;
                    state.vars.insert(var_name.clone(), rendered);
                }
            }
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
                            let node_outputs = state.outputs_map();
                            let nodes_snapshot = state.nodes.clone();
                            let final_output = state.finalize_with(Some(output.payload.clone()));
                            return Ok(FlowExecution::completed(
                                lift_first_node_error_from_nodes(final_output, &nodes_snapshot),
                                node_outputs,
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
                            let node_outputs = state.outputs_map();
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
                                node_outputs,
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
                    let node_outputs = state.outputs_map();
                    let output_value = state.clone().finalize_with(None);
                    return Ok(FlowExecution::waiting(
                        output_value,
                        FlowWait { reason, snapshot },
                        node_outputs,
                    ));
                }
                NodeControl::LoopHere { reason } => {
                    // Conversational dw.agent: park and re-enter THIS node so the
                    // next user message drives the next turn. Render the reply
                    // (finalize_with Some) — unlike NodeControl::Wait, which
                    // resumes at the successor and finalizes with None.
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
                    let node_outputs = state.outputs_map();
                    let output_value = state.finalize_with(Some(output.payload.clone()));
                    return Ok(FlowExecution::waiting(
                        output_value,
                        FlowWait { reason, snapshot },
                        node_outputs,
                    ));
                }
                NodeControl::AwaitHere {
                    reason,
                    correlation_id: _,
                } => {
                    // Await the async agent response, but resume at THIS node so
                    // the conversational branch evaluates `terminated_by`. Mirror the
                    // remote-await Wait snapshot construction EXCEPT next_node = self.
                    //
                    // Keying note: the resume snapshot is stored under the SAME
                    // `(session_hint, scope_hash)` store key as every other wait kind
                    // (`build_store_ctx` strips the correlation from the key). The
                    // correlation id is NOT part of the key — it only drives how the
                    // NATS response reconstructs the hint/scope (RuntimeSessionResumer)
                    // so it recomputes that same key. So an AwaitHere park and a later
                    // LoopHere park for the same conversation occupy the same single
                    // slot; they never coexist because each park overwrites it. Safety
                    // rests on sequential single-slot resume, not key separation — an
                    // inbound arriving mid-await resolves to this slot (a known
                    // interleaving limitation, tracked as a follow-up).
                    let mut snapshot_state = state.clone();
                    snapshot_state.clear_egress();
                    let snapshot = FlowSnapshot {
                        pack_id: step_ctx.pack_id.to_string(),
                        flow_id: step_ctx.flow_id.to_string(),
                        next_flow: (current_flow_id != step_ctx.flow_id)
                            .then_some(current_flow_id.clone()),
                        next_node: node_id.as_str().to_string(), // SELF, not successor
                        state: snapshot_state,
                    };
                    let node_outputs = state.outputs_map();
                    // Finalize with None (render nothing here — the reply, if any,
                    // was already surfaced before the dispatch): match the
                    // remote-await Wait finalize semantics confirmed in Task 1.
                    let output_value = state.clone().finalize_with(None);
                    return Ok(FlowExecution::waiting(
                        output_value,
                        FlowWait { reason, snapshot },
                        node_outputs,
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
                    let node_outputs = state.outputs_map();
                    let nodes_snapshot = state.nodes.clone();
                    let final_output = state.finalize_with(None);
                    return Ok(FlowExecution::completed(
                        lift_first_node_error_from_nodes(final_output, &nodes_snapshot),
                        node_outputs,
                    ));
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
            NodeKind::VarSet { name, value } => {
                if name.trim().is_empty() {
                    tracing::warn!(
                        node_id = %node_id,
                        "var_set node has an empty variable name; skipping write"
                    );
                    return Ok(DispatchOutcome::complete(NodeOutput::new(
                        serde_json::json!({ "ok": true }),
                    )));
                }
                let prev = state.last_output.clone().unwrap_or(Value::Null);
                let ctx_val = template_context(state, prev);
                let rendered = render_template_value(
                    value,
                    &ctx_val,
                    TemplateOptions {
                        allow_pointer: true,
                    },
                )
                .context("failed to render var_set value")?;
                state.vars.insert(name.clone(), rendered);
                Ok(DispatchOutcome::complete(NodeOutput::new(
                    serde_json::json!({ "ok": true }),
                )))
            }
            NodeKind::Wait => {
                let reason = extract_wait_reason(&payload);
                Ok(DispatchOutcome::wait(NodeOutput::new(payload), reason))
            }
            NodeKind::DwAgent {
                agent_id,
                conversational,
            } => {
                #[cfg(feature = "agentic-worker")]
                match self.dw_agent_dispatch {
                    crate::runner::agent_node::DwAgentDispatch::Nats => {
                        if *conversational {
                            // Interleave guard (#1): the pending-await marker alone
                            // does not prove this resume carries the agent's NATS
                            // response — a user message can arrive before it (e.g.
                            // the user types again while the agent is still
                            // thinking) and land in `state.entry` too. The NATS
                            // response envelope is always `{ok, output, events,
                            // error}`; a user-message resume has no `"ok"` key. Only
                            // treat this as the response path when BOTH the marker
                            // is set AND `state.entry` looks like that envelope.
                            // `&&` short-circuits, so `take_agent_await` (which
                            // clears the marker) is not called when the shape check
                            // fails — the marker survives for the real response,
                            // and the stray user message falls through to the
                            // fresh-dispatch branch below (re-dispatches as a new
                            // turn instead of being misread as a null agent reply).
                            let is_agent_response = state.entry.get("ok").is_some();
                            if is_agent_response && state.take_agent_await(node_id) {
                                // Resuming with the agent's NATS response. SPIKE §Q2:
                                // the response is NOT in the `payload` argument (that
                                // is a freshly re-rendered request-mapping template) —
                                // it landed in `state.entry` as the envelope
                                // `{ok, output, events, error}`, so the agent output is
                                // `state.entry.output` (= `{reply, trail, terminated_by}`)
                                // and `terminated_by` is nested one level under `.output`.
                                // `state.entry` is readable here (same module; precedent
                                // `inject_card_locale(&mut payload, &state.entry)` above).
                                let ok = state
                                    .entry
                                    .get("ok")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                if !ok {
                                    // Error envelope (Fix B): a transport/agent error
                                    // must not be silently swallowed as a null reply,
                                    // and must NOT bump the park-loop cap — a flapping
                                    // backend should not burn the segment's turn budget
                                    // or force-advance past it. Surface the error
                                    // message as the reply and re-park (fail-safe:
                                    // await the next user message). There is currently
                                    // no self-inflicted await deadline (see the
                                    // fresh-dispatch branch below), but this also
                                    // handles a `{ok:false}` envelope from any other
                                    // source (e.g. a flow-authored deadline) the same way.
                                    let message = state
                                        .entry
                                        .get("error")
                                        .and_then(|e| e.get("message"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("the agent could not respond");
                                    tracing::warn!(
                                        agent_id = %agent_id,
                                        error = %message,
                                        "conversational dw.agent (nats) response was an error/timeout envelope; surfacing and re-parking without bumping the park-loop cap"
                                    );
                                    let output =
                                        NodeOutput::new(serde_json::json!({ "reply": message }));
                                    Ok(DispatchOutcome::with_control(
                                        output,
                                        NodeControl::LoopHere {
                                            reason: Some(format!(
                                                "conversational dw.agent `{agent_id}` (nats) awaiting next user message after error response"
                                            )),
                                        },
                                    ))
                                } else {
                                    let agent_out =
                                        state.entry.get("output").cloned().unwrap_or(Value::Null);
                                    let output = NodeOutput::new(agent_out.clone());
                                    let ended = agent_out
                                        .get("terminated_by")
                                        .and_then(serde_json::Value::as_str)
                                        == Some("conversation_ended");
                                    if ended {
                                        state.reset_park_turns(node_id);
                                        Ok(DispatchOutcome::complete(output))
                                    } else {
                                        let turns = state.bump_park_turns(node_id);
                                        if turns >= MAX_PARK_TURNS {
                                            tracing::warn!(
                                                agent_id = %agent_id,
                                                turns,
                                                "conversational dw.agent (nats) hit park-loop cap ({MAX_PARK_TURNS}); force-advancing to successor"
                                            );
                                            // Reset on force-advance too, so a graph that
                                            // re-enters this node later starts with a fresh
                                            // budget (mirrors the `conversation_ended` path —
                                            // the counter is cleared on every exit past the node).
                                            state.reset_park_turns(node_id);
                                            Ok(DispatchOutcome::complete(output))
                                        } else {
                                            Ok(DispatchOutcome::with_control(
                                                output,
                                                NodeControl::LoopHere {
                                                    reason: Some(format!(
                                                        "conversational dw.agent `{agent_id}` (nats) awaiting next user message"
                                                    )),
                                                },
                                            ))
                                        }
                                    }
                                }
                            } else {
                                // Fresh user turn: dispatch to NATS, park awaiting the
                                // response and re-enter THIS node on resume (not the
                                // routing successor) so the conversational decision
                                // above can evaluate the agent's response. No await
                                // deadline: a bounded wait needs a per-dispatch
                                // correlation nonce + watchdog cancellation (the
                                // correlation id is deterministic per-conversation and
                                // the resume-store slot is overwritten every turn, so a
                                // naive deadline's fire-and-forget watchdog can inject a
                                // spurious timeout into a later, healthy turn) — tracked
                                // as a follow-up.
                                state.mark_agent_await(node_id);
                                let remote_payload =
                                    serde_json::json!({ "await": true, "input": payload });
                                self.execute_remote_dispatch(
                                    ctx,
                                    "agentic",
                                    agent_id,
                                    remote_payload,
                                    true,
                                )
                                .await
                            }
                        } else {
                            // Non-conversational: unchanged single await → resumes at
                            // the routing successor, exactly as before.
                            let remote_payload =
                                serde_json::json!({ "await": true, "input": payload });
                            self.execute_remote_dispatch(
                                ctx,
                                "agentic",
                                agent_id,
                                remote_payload,
                                false,
                            )
                            .await
                        }
                    }
                    crate::runner::agent_node::DwAgentDispatch::InProcess => {
                        let output = self
                            .execute_dw_agent(ctx, agent_id, payload, *conversational)
                            .await?;
                        if *conversational {
                            let ended = output
                                .payload
                                .get("terminated_by")
                                .and_then(serde_json::Value::as_str)
                                == Some("conversation_ended");
                            if ended {
                                state.reset_park_turns(node_id);
                                Ok(DispatchOutcome::complete(output))
                            } else {
                                let turns = state.bump_park_turns(node_id);
                                if turns >= MAX_PARK_TURNS {
                                    tracing::warn!(
                                        agent_id = %agent_id,
                                        turns,
                                        "conversational dw.agent hit park-loop cap ({MAX_PARK_TURNS}); force-advancing to successor"
                                    );
                                    // Reset on force-advance too, so a graph that
                                    // re-enters this node later starts with a fresh
                                    // budget (mirrors the `conversation_ended` path —
                                    // the counter is cleared on every exit past the node).
                                    state.reset_park_turns(node_id);
                                    Ok(DispatchOutcome::complete(output))
                                } else {
                                    Ok(DispatchOutcome::with_control(
                                        output,
                                        NodeControl::LoopHere {
                                            reason: Some(format!(
                                                "conversational dw.agent `{agent_id}` awaiting next user message"
                                            )),
                                        },
                                    ))
                                }
                            }
                        } else {
                            Ok(DispatchOutcome::complete(output))
                        }
                    }
                }
                #[cfg(not(feature = "agentic-worker"))]
                {
                    self.execute_dw_agent(ctx, agent_id, payload, *conversational)
                        .await
                        .map(DispatchOutcome::complete)
                }
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
            NodeKind::ApprovalCall { target } => {
                self.execute_approval_call(ctx, node_id, target, payload, state)
                    .await
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
        conversational: bool,
    ) -> Result<NodeOutput> {
        let handler = self
            .agent_node_handler
            .as_ref()
            .context("DwAgent node dispatched but no AgentNodeHandler configured on FlowEngine")?;
        let session_id = ctx.session_id.unwrap_or("");
        let started = std::time::Instant::now();
        let mut result = handler
            .execute(
                ctx.tenant,
                &self.default_env,
                agent_id,
                session_id,
                &payload,
                conversational,
            )
            .await?;
        // Per-node timing: record this agent step's own execution time on its
        // output (`duration_ms`), so a trace can show per-node — not just
        // per-turn — latency. Injected only when the output is a JSON object.
        if let Value::Object(map) = &mut result {
            map.insert(
                "duration_ms".into(),
                Value::from(started.elapsed().as_millis() as u64),
            );
        }
        Ok(NodeOutput::new(result))
    }

    #[cfg(not(feature = "agentic-worker"))]
    async fn execute_dw_agent(
        &self,
        _ctx: &FlowContext<'_>,
        agent_id: &str,
        _payload: Value,
        _conversational: bool,
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
        self.execute_remote_dispatch(ctx, "sorla", target, payload, false)
            .await
    }

    /// Dispatch an `operala.call` flow node.
    ///
    /// When an in-process [`OperalaNodeHandler`] is wired (`desktop-agent-
    /// ephemeral`, e.g. the designer's offline Test-chat sidecar), the node
    /// runs the deep-worker runtime directly — no NATS, no
    /// `RemoteDispatchHandler` — and completes inline. Otherwise falls back to
    /// the shared remote-dispatch seam, identical to [`execute_sorla_call`]
    /// except the runtime name is `"operala"`.
    ///
    /// [`OperalaNodeHandler`]: crate::runner::operala_node::OperalaNodeHandler
    async fn execute_operala_call(
        &self,
        ctx: &FlowContext<'_>,
        target: &str,
        payload: Value,
    ) -> Result<DispatchOutcome> {
        if let Some(handler) = self.operala_node_handler.as_ref() {
            // `greentic-dw-authoring` stamps `operation: "invoke"` on the
            // `operala.call` node, but the deep-worker invoker's contract
            // accepts only `"" | "run"`. Normalize so an authored `deep_worker`
            // actually runs in-process instead of failing operation validation.
            let raw_operation = payload
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let operation = if raw_operation.eq_ignore_ascii_case("invoke") {
                "run"
            } else {
                raw_operation
            };
            let inner_input = payload.get("input").cloned().unwrap_or(Value::Null);
            let session_id = ctx.session_id.unwrap_or("");
            let result = handler
                .execute(
                    ctx.tenant,
                    &self.default_env,
                    target,
                    operation,
                    session_id,
                    &inner_input,
                )
                .await?;
            return Ok(DispatchOutcome::complete(NodeOutput::new(result)));
        }
        self.execute_remote_dispatch(ctx, "operala", target, payload, false)
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
        self.execute_remote_dispatch(ctx, "agentic", target, payload, false)
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
        self.execute_remote_dispatch(ctx, "telco-x", target, payload, false)
            .await
    }

    /// Human-in-the-loop approval gate.
    ///
    /// Sets `meta["outcome"]` to `approved` / `denied` / `timeout` on every path,
    /// so routing conditions (`event == "approved"`) work identically whether a
    /// human decided or the gate auto-approved. `ok` cannot serve as the
    /// discriminator: greentic-admin publishes `ok: true` for approve AND deny.
    ///
    /// Uses `resume_at_self = true` so the response re-enters THIS node and its
    /// own routing sees the decision. `pending_approval_await` distinguishes the
    /// first entry from the resume, and stops a stray inbound (which lands in the
    /// same wait slot — see the keying note on `NodeControl::AwaitHere`) from
    /// re-dispatching a duplicate approval request.
    async fn execute_approval_call(
        &self,
        ctx: &FlowContext<'_>,
        node_id: &str,
        target: &str,
        payload: Value,
        state: &mut ExecutionState,
    ) -> Result<DispatchOutcome> {
        if state.take_approval_await(node_id) {
            if entry_is_approval_response(&state.entry) {
                let outcome = approval_outcome_from_entry(&state.entry);
                let output = NodeOutput::with_meta(
                    state.entry.clone(),
                    serde_json::json!({ "outcome": outcome }),
                );
                return Ok(DispatchOutcome::complete(output));
            }
            // A user activity arrived while we were parked. Re-park without
            // re-dispatching; the correlation id is discarded by the AwaitHere
            // handler, so re-parking needs nothing from the original dispatch.
            state.mark_approval_await(node_id);
            return Ok(DispatchOutcome::await_here(
                NodeOutput::new(serde_json::json!({ "pending": true })),
                Some("awaiting approval decision".to_string()),
                String::new(),
            ));
        }

        let input = payload.get("input").cloned().unwrap_or(Value::Null);
        if !approval_requires_human(&input) {
            let output = NodeOutput::with_meta(
                serde_json::json!({
                    "ok": true,
                    "output": { "decision": "approved", "auto": true },
                    "error": serde_json::Value::Null,
                }),
                serde_json::json!({ "outcome": "approved" }),
            );
            return Ok(DispatchOutcome::complete(output));
        }

        // Mark only once the dispatch actually parked. A fire-and-forget
        // (`await: false`) dispatch completes without parking, and an early `?`
        // error never parks either — marking before the call would leave the
        // node believing it is awaiting a decision it never requested, and every
        // later inbound would re-park forever.
        let outcome = self
            .execute_remote_dispatch(ctx, "approval", target, payload, true)
            .await?;
        if matches!(outcome.control, NodeControl::AwaitHere { .. }) {
            state.mark_approval_await(node_id);
        }
        Ok(outcome)
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
    /// `resume_at_self`: when the runtime enters `AwaitingResponse`, controls
    /// whether the flow resumes at the routing successor
    /// ([`DispatchOutcome::wait`], `resume_at_self = false` — the behavior for
    /// every non-conversational caller) or re-enters THIS node
    /// ([`DispatchOutcome::await_here`], `resume_at_self = true` — the
    /// conversational out-of-process `dw.agent` caller) once the async
    /// response arrives. Only affects the `AwaitingResponse` branch; the
    /// `Dispatched` (fire-and-forget) branch is unaffected either way.
    ///
    /// [`RemoteDispatchHandler`]: crate::runner::remote_dispatch::RemoteDispatchHandler
    async fn execute_remote_dispatch(
        &self,
        ctx: &FlowContext<'_>,
        runtime: &str,
        target: &str,
        payload: Value,
        resume_at_self: bool,
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
                    "correlation_id": correlation_id.clone(),
                }));
                if resume_at_self {
                    Ok(DispatchOutcome::await_here(
                        output,
                        Some(reason),
                        correlation_id,
                    ))
                } else {
                    Ok(DispatchOutcome::wait(output, Some(reason)))
                }
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
            has_error_route: node_has_error_route(&node.routing),
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
            has_error_route: node_has_error_route(&node.routing),
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
            // node_io error routing: a node with an `on_error`-family route
            // surfaces the failure as an `{errors}` output and lets its error
            // branch handle it. Nodes without such a route keep the historical
            // hard-fail, so this is purely additive.
            if call.has_error_route {
                return Ok(NodeOutput::errored(value));
            }
            bail!(
                "component {} failed: {}: {}",
                call.component_ref,
                code,
                message
            );
        }
        // MCP-shaped tool errors (greentic-mcp-generator's tool_error_with_status)
        // come back as a top-level `{ "error": { "code", "message", "status" } }`
        // value with the WIT envelope still ok=true (because the wasm guest
        // returned normally). Treat them the same as a component_error so the
        // engine error-envelope lift path surfaces the failure to the user.
        if let Some((code, message)) = mcp_tool_error(&value) {
            bail!(
                "component {} returned tool error: {}: {}",
                call.component_ref,
                code,
                message
            );
        }
        let meta = outcome_meta(&value);
        Ok(NodeOutput::with_meta(value, meta))
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
        let provider_metric_id = payload
            .provider_id
            .as_deref()
            .or(payload.provider_type.as_deref())
            .unwrap_or("unknown");
        let invoke_started = std::time::Instant::now();
        let invoke_result = pack
            .invoke_provider(&binding, exec_ctx, &op, input_json)
            .await;
        let invoke_duration_ms = invoke_started.elapsed().as_secs_f64() * 1000.0;
        crate::metrics::record_provider_invocation(
            ctx.tenant,
            provider_metric_id,
            &op,
            if invoke_result.is_ok() { "ok" } else { "err" },
            invoke_duration_ms,
        );
        let result = invoke_result?;
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

/// Safety backstop for a CONVERSATIONAL `dw.agent` node's park-and-loop cycle.
///
/// This is NOT a UX limit — it exists purely so that a stuck or misbehaving
/// conversational agent (one that never emits `conversation_ended`) cannot
/// trap a flow at the same node forever. After this many parked turns at a
/// single conversational `dw.agent` node without a `conversation_ended`
/// termination, the flow force-advances to the node's successor using the
/// agent's last output. Deliberately a plain constant: no env var, no
/// per-agent config knob.
///
/// Only referenced from the conversational `DwAgent` branch of
/// `dispatch_node` and from the (already `#[cfg(feature = "agentic-worker")]`
/// -gated) park-loop-cap unit tests below; cfg-gate it the same way so it
/// isn't flagged dead when that feature is off (e.g. a lean
/// `--no-default-features --features verify` build). Unlike
/// `bump_park_turns`/etc. below, no plain (ungated) test references this
/// constant directly, so no `test` alternative is needed here.
#[cfg(feature = "agentic-worker")]
const MAX_PARK_TURNS: u32 = 100;

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
    #[serde(default)]
    vars: JsonMap<String, Value>,
    /// Per-node park-loop turn counter for conversational `dw.agent` nodes,
    /// keyed by node id. Mirrors `redirect_count`'s per-execution safety-cap
    /// pattern, but tracked per node since a flow may hold more than one
    /// conversational agent.
    #[serde(default)]
    park_turns: HashMap<String, u32>,
    /// Marks a node as awaiting an async `dw.agent` NATS dispatch response.
    /// Set on dispatch, checked-and-cleared on resume; mirrors `park_turns`'
    /// per-node, serde-defaulted, persisted-in-snapshot pattern.
    #[serde(default)]
    pending_agent_await: HashMap<String, ()>,
    /// Nodes that dispatched an approval request and are parked awaiting the
    /// decision. Set on dispatch, cleared when the response re-enters the node.
    /// Without it a stray inbound arriving mid-await would look like a first
    /// entry and re-dispatch — a duplicate approval request to the operator.
    #[serde(default)]
    pending_approval_await: HashMap<String, ()>,
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
            vars: JsonMap::new(),
            park_turns: HashMap::new(),
            pending_agent_await: HashMap::new(),
            pending_approval_await: HashMap::new(),
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
            "park_turns": self.park_turns.clone(),
            "pending_agent_await": self.pending_agent_await.keys().cloned().collect::<Vec<_>>(),
        })
    }

    fn outputs_map(&self) -> JsonMap<String, Value> {
        let mut outputs = JsonMap::new();
        for (id, output) in &self.nodes {
            outputs.insert(id.clone(), node_output_view(&output.payload));
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

    /// Bump the park-loop turn counter for `node_id` and return the NEW count.
    ///
    /// Like `MAX_PARK_TURNS`, only called from the conversational `DwAgent`
    /// branch of `dispatch_node` (`agentic-worker`-gated) plus the unit tests
    /// below.
    #[cfg(any(feature = "agentic-worker", test))]
    fn bump_park_turns(&mut self, node_id: &str) -> u32 {
        let entry = self.park_turns.entry(node_id.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Clear the park-loop turn counter for `node_id` (e.g. once the
    /// conversational segment ends), so a later re-entry starts fresh.
    #[cfg(any(feature = "agentic-worker", test))]
    fn reset_park_turns(&mut self, node_id: &str) {
        self.park_turns.remove(node_id);
    }

    /// Mark `node_id` as awaiting an async `dw.agent` NATS dispatch response.
    /// Called from the conversational `DwAgentDispatch::Nats` branch in
    /// `dispatch_node` before dispatching a fresh user turn.
    #[cfg(any(feature = "agentic-worker", test))]
    fn mark_agent_await(&mut self, node_id: &str) {
        self.pending_agent_await.insert(node_id.to_string(), ());
    }

    /// Check-and-clear: returns whether `node_id` was awaiting an agent response.
    /// Called from the conversational `DwAgentDispatch::Nats` branch in
    /// `dispatch_node` on resume, to distinguish "resuming with the agent's
    /// response" from "a fresh user turn".
    #[cfg(any(feature = "agentic-worker", test))]
    fn take_agent_await(&mut self, node_id: &str) -> bool {
        self.pending_agent_await.remove(node_id).is_some()
    }

    /// Mark `node_id` as parked awaiting an approval decision.
    fn mark_approval_await(&mut self, node_id: &str) {
        self.pending_approval_await.insert(node_id.to_string(), ());
    }

    /// Check-and-clear: returns whether `node_id` dispatched an approval and is
    /// awaiting the decision.
    fn take_approval_await(&mut self, node_id: &str) -> bool {
        self.pending_approval_await.remove(node_id).is_some()
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

    /// `ok=false` output stashing error context in `meta.error`. Currently
    /// only used by `lift_first_node_error_from_nodes` tests — kept around so
    /// drive_flow can resume populating it once we have a hook for it.
    #[allow(dead_code)]
    fn with_error(node_id: &str, err: &(dyn std::error::Error + 'static)) -> Self {
        Self {
            ok: false,
            payload: Value::Null,
            meta: json!({
                "error": {
                    "kind": "flow_node_failed",
                    "message": err.to_string(),
                    "node_id": node_id,
                }
            }),
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

    fn await_here(output: NodeOutput, reason: Option<String>, correlation_id: String) -> Self {
        Self {
            output,
            control: NodeControl::AwaitHere {
                reason,
                correlation_id,
            },
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
    /// Park the flow and RE-ENTER this same node on the next inbound activity
    /// (conversational `dw.agent` loop), rendering the node output first.
    /// Unlike `Wait` (which resumes at the routing successor and renders
    /// nothing), `LoopHere` sets the resume target to the current node and
    /// renders the reply.
    ///
    /// Only constructed from the conversational `DwAgent` branch of
    /// `dispatch_node`, which is `#[cfg(feature = "agentic-worker")]` —
    /// `#[allow(dead_code)]` rather than cfg-gating the variant itself so the
    /// (unconditionally-compiled) match arm handling it in `run_flow`'s
    /// dispatch loop doesn't need a matching cfg attribute.
    #[allow(dead_code)]
    LoopHere {
        reason: Option<String>,
    },
    /// Park the flow and await a correlation-keyed async runtime response
    /// (out-of-process conversational `dw.agent`), but RE-ENTER this same
    /// node when the response arrives — like `LoopHere` it resumes at THIS
    /// node (not the routing successor) so the conversational decision can
    /// evaluate the response, unlike `Wait` which resumes at the routing
    /// successor. Resumed via the dispatch listener + `RuntimeSessionResumer`.
    AwaitHere {
        reason: Option<String>,
        // Audit-carried, not read: per the spike's keying model (§Q3), the
        // actual resume lookup is keyed by `(session_hint, ReplyScope.scope_hash)`
        // via `build_store_ctx`/`FlowResumeStore`, not by this field — the
        // `drive_flow` handler destructures it as `correlation_id: _`. Kept on
        // the variant for debugging/observability only.
        #[allow(dead_code)]
        correlation_id: String,
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

    /// A failure output (`ok == false`). `build_routing_context` derives the
    /// `error_event` from this, so a node with an `on_error`-family route lands
    /// on its failure branch; `node_output_view` exposes the `{errors}` envelope.
    fn errored(payload: Value) -> Self {
        Self {
            ok: false,
            payload,
            meta: Value::Null,
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

/// Surface a component-emitted `outcome` (from its output envelope) as node
/// metadata, so the routing context can match `event == "<outcome>"`. A
/// component opts in by adding `"outcome": "<name>"` to its output envelope
/// (alongside `ok`); `<name>` must be one of its declared
/// `ComponentDescribe.outcomes`. Returns `Value::Null` when the component does
/// not emit one — the engine then falls back to the `ok`-derived default
/// (`on_success`/`on_error`) in `build_routing_context`.
/// Adapt a raw component/node result `Value` into the typed node_io [`NodeOutput`]
/// (`greentic_types::node_io`). Native `{data}` / `{errors}` envelopes parse straight
/// through; legacy `{ok, error}` results are shimmed (`ok:false` + `error` → `Errors`,
/// otherwise → `Data{data: <value>}`) so existing packs keep routing unchanged.
fn to_node_output(value: &Value) -> greentic_types::node_io::NodeOutput {
    use greentic_types::node_io::{ErrorKind, NodeError, NodeOutput as NioOutput};

    if let Value::Object(map) = value {
        // Native node_io envelopes carry a sole `errors` or `data` key and no legacy
        // `ok` flag — deserialize them directly so `kind`/`retryable`/etc. round-trip.
        let native_errors = map.contains_key("errors") && !map.contains_key("ok");
        let native_data = map.contains_key("data") && !map.contains_key("ok") && map.len() == 1;
        if (native_errors || native_data)
            && let Ok(parsed) = serde_json::from_value::<NioOutput>(value.clone())
        {
            return parsed;
        }
        // Legacy failure envelope `{ok:false, error:{code,message}}` → Errors.
        if let Some((code, message)) = component_error(value) {
            return NioOutput::failed(vec![NodeError {
                code,
                message,
                kind: ErrorKind::Internal,
                retryable: false,
                source: None,
                details: Value::Null,
            }]);
        }
    }
    // Default: a bare result (or `{ok:true, ...}`) is success data.
    NioOutput::ok(value.clone())
}

/// Build the per-node template view exposed under `{{node.<id>...}}`. Object payloads
/// keep their fields at the top level (legacy `{{node.<id>.<field>}}`) and additionally
/// gain canonical node_io surfaces `data` (`{{node.<id>.data.<field>}}`) and `errors`
/// (`{{node.<id>.errors}}`). Non-object payloads are exposed verbatim, as before.
fn node_output_view(payload: &Value) -> Value {
    let nio = to_node_output(payload);
    let data = nio.data().cloned().unwrap_or(Value::Null);
    let errors = serde_json::to_value(nio.errors()).unwrap_or_else(|_| Value::Array(Vec::new()));
    match payload {
        Value::Object(map) => {
            let mut view = map.clone();
            view.insert("data".to_string(), data);
            view.insert("errors".to_string(), errors);
            Value::Object(view)
        }
        other => other.clone(),
    }
}

fn outcome_meta(output: &Value) -> Value {
    match output.get("outcome").and_then(Value::as_str) {
        Some(outcome) => json!({ "outcome": outcome }),
        None => Value::Null,
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

/// MCP tool-error wire shape from greentic-mcp-generator's `tool_error_with_status`:
/// `{ "error": { "code", "message", "status" } }`. The component returned ok=true at
/// the WIT level (the HTTP failure was caught and serialized), so the regular
/// component_error path doesn't catch it.
fn mcp_tool_error(value: &Value) -> Option<(String, String)> {
    let obj = value.as_object()?;
    // Must be the error shape: no `result` field, just `error`.
    if obj.contains_key("result") {
        return None;
    }
    let err = obj.get("error")?.as_object()?;
    let code = err
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("tool_error");
    let raw_message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("tool returned an error");
    let status = err.get("status").and_then(Value::as_u64);
    let message = match status {
        Some(s) => format!("{raw_message} (status {s})"),
        None => raw_message.to_string(),
    };
    Some((code.to_string(), message))
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
    ctx.insert("vars".into(), Value::Object(state.vars.clone()));
    Value::Object(ctx)
}

/// Seed declared flow variables into `target` (entry-or-insert; never
/// overwrites an already-present key), then return the `required` names that
/// remain absent from `target`. A required var is satisfied by either a
/// declared default (seeded here) or a value already placed in `target`
/// (e.g. an operator-provided demo value).
fn seed_vars_and_collect_missing_required(
    vars_init: &JsonMap<String, Value>,
    required: &[String],
    target: &mut JsonMap<String, Value>,
) -> Vec<String> {
    for (name, default) in vars_init.iter() {
        target
            .entry(name.clone())
            .or_insert_with(|| default.clone());
    }
    required
        .iter()
        .filter(|name| !target.contains_key(name.as_str()))
        .cloned()
        .collect()
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
        let vars_init = value
            .metadata
            .extra
            .get("vars_init")
            .and_then(|v| v.as_object())
            .map(|decls| {
                decls
                    .iter()
                    .filter_map(|(name, decl)| {
                        decl.get("default").map(|d| (name.clone(), d.clone()))
                    })
                    .collect::<JsonMap<String, Value>>()
            })
            .unwrap_or_default();
        let required_vars = value
            .metadata
            .extra
            .get("vars_init")
            .and_then(|v| v.as_object())
            .map(|decls| {
                decls
                    .iter()
                    .filter(|(_, decl)| decl.get("required") == Some(&Value::Bool(true)))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        // Extract flow-level slot_schema from metadata.extra (Phase D).
        // The producer side (greentic-flow compile_flow) stores it under
        // "greentic.slot_schema" when the FlowDoc has a `slot_schema` field.
        let slot_schema = value
            .metadata
            .extra
            .get(SLOT_SCHEMA_METADATA_KEY)
            .filter(|v| !v.is_null())
            .cloned();
        Self {
            id: value.id.as_str().to_string(),
            start,
            nodes,
            vars_init,
            required_vars,
            slot_schema,
        }
    }
}

impl From<Node> for HostNode {
    fn from(node: Node) -> Self {
        let full_ref = node.component.id.as_str().to_string();
        let operation_in_mapping = extract_operation_from_mapping(&node.input.mapping);
        // A dotted component id is only a packed "<component>.<operation>" string
        // when the operation isn't carried structurally elsewhere. greentic-pack
        // resolves a component node to a bare component symbol (e.g.
        // `ai.greentic.component-templates`) and keeps the operation in the input
        // mapping, so splitting on the last dot here would corrupt the reference
        // (→ `ai.greentic`, "not found in pack"). Prefer the structured operation —
        // from `component.operation` or the input mapping — and only fall back to
        // the legacy single-ID split when neither is present.
        let is_builtin = full_ref.starts_with("component.exec")
            || full_ref.starts_with("flow.")
            || full_ref.starts_with("emit.")
            || full_ref.starts_with("session.")
            || full_ref.starts_with("provider.")
            || full_ref.starts_with("dw.")
            || full_ref.starts_with("sorla.")
            || full_ref.starts_with("operala.")
            || full_ref.starts_with("agentic.")
            // Same dispatch family as sorla./operala./agentic. above, and listed in
            // NATIVE_OP_KEYS like them. Without it an `approval.call` node — which
            // packc emits with `operation: None` because the id is already complete
            // — gets split into component "approval" + operation "call", and
            // "approval" is by construction absent from the pack's component map.
            // The gate then fails with `component 'approval' not found in pack` on a
            // pack that is perfectly well-formed, so rebuilding never helps.
            || full_ref.starts_with("approval.")
            || full_ref.starts_with("var.")
            // `mcp:<server>/<tool>` is a self-contained ref; never dot-split it
            // into a `component.operation` pair.
            || full_ref.starts_with("mcp:");
        // packc emits `operation: None` to mean "the id is already complete" and
        // carries the operation in input.mapping instead. Splitting such an id would
        // produce a prefix that is never a key in the pack's component map (the map is
        // keyed verbatim by `node.component.id`), so only split when the mapping does
        // not tell us the operation.
        let (component_ref, raw_operation) =
            if node.component.operation.is_some() || is_builtin || operation_in_mapping.is_some() {
                (full_ref, node.component.operation.clone())
            } else if let Some(dot) = full_ref.rfind('.') {
                let comp = full_ref[..dot].to_string();
                let op = full_ref[dot + 1..].to_string();
                (comp, Some(op))
            } else {
                (full_ref, None)
            };
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
                "var.set" => {
                    let name = node
                        .input
                        .mapping
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let value = node
                        .input
                        .mapping
                        .get("value")
                        .cloned()
                        .unwrap_or(Value::Null);
                    NodeKind::VarSet { name, value }
                }
                "dw.agent" => NodeKind::DwAgent {
                    agent_id: raw_operation.clone().unwrap_or_default(),
                    // SP3: honour the flow-doc `conversational` flag (greentic-flow
                    // parses it into `greentic_types::Node.conversational`).
                    conversational: node.conversational,
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
                "approval.call" => NodeKind::ApprovalCall {
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
            NodeKind::VarSet { .. } => "var.set".to_string(),
            NodeKind::Wait => "session.wait".to_string(),
            NodeKind::DwAgent { .. } => "dw.agent".to_string(),
            NodeKind::DwAgentGraph { .. } => "dw.agent_graph".to_string(),
            NodeKind::SorlaCall { .. } => "sorla.call".to_string(),
            NodeKind::OperalaCall { .. } => "operala.call".to_string(),
            NodeKind::AgenticCall { .. } => "agentic.call".to_string(),
            NodeKind::TelcoXCall { .. } => "telco-x.call".to_string(),
            NodeKind::ApprovalCall { .. } => "approval.call".to_string(),
            NodeKind::Mcp { server_id, tool } => format!("mcp:{server_id}/{tool}"),
        };
        let operation_name = if is_component_exec && operation_is_component_exec {
            None
        } else {
            raw_operation.clone()
        };
        // Extract per-node output bindings before the mapping is consumed by
        // `payload_expr`. Stored as raw (unrendered) templates so they can be
        // applied after the node runs, using the node's own output as `prev`.
        let vars_out = node
            .input
            .mapping
            .get("vars_out")
            .and_then(Value::as_object)
            .cloned();
        let payload_expr = match kind {
            NodeKind::BuiltinEmit { .. } => extract_emit_payload(&node.input.mapping),
            // VarSet dispatch re-reads name/value from NodeKind::VarSet directly;
            // the payload render is redundant and must not be forwarded as node input.
            NodeKind::VarSet { .. } => Value::Null,
            _ => {
                // Strip the internal `vars_out` meta-key so it is never
                // forwarded as an input field to wasm components or other
                // non-emit node kinds (which may have strict schemas).
                let mut mapping = node.input.mapping.clone();
                if let Some(obj) = mapping.as_object_mut() {
                    obj.remove("vars_out");
                }
                mapping
            }
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
            vars_out,
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

/// Inject flow-level `slot_schema` as `slot_definitions` into the
/// slot-extractor component's input value. Skips injection when the input
/// already contains an explicit `slot_definitions` key (back-compat with
/// M2.4 NDA demo inline definitions). When the input is `Null`, promotes it
/// to an empty object first.
fn inject_slot_definitions(input: &mut Value, slot_schema: &Value, flow_id: &str, node_id: &str) {
    if input.is_null() {
        *input = Value::Object(serde_json::Map::new());
    }
    let Some(map) = input.as_object_mut() else {
        tracing::warn!(
            flow_id,
            node_id,
            "slot-extractor input is not an object; cannot inject slot_definitions"
        );
        return;
    };
    if map.contains_key("slot_definitions") {
        return;
    }
    let slot_count = slot_schema.as_array().map_or(0, Vec::len);
    tracing::debug!(
        flow_id,
        slot_count,
        "injecting flow-level slot_schema as slot_definitions into slot-extractor input"
    );
    map.insert("slot_definitions".to_string(), slot_schema.clone());
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
    // The default `event` is chosen from the success/error-family port this node
    // actually routes on, so happy paths named `on_complete`/`on_submit` and
    // failure paths named `on_cancel`/`on_timeout` resolve instead of stalling
    // at `Wait`.
    let ctx = build_routing_context(
        output,
        state,
        default_success_event(routes),
        default_error_event(routes),
    );

    let mut has_condition = false;
    let mut unmatched_conditions: Vec<&str> = Vec::new();
    for route in routes {
        let condition = route.get("condition").and_then(|v| v.as_str());
        let to = route.get("to").and_then(|v| v.as_str());

        if let Some(cond) = condition {
            has_condition = true;
            unmatched_conditions.push(cond);
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
        tracing::warn!(
            flow_id = %flow_ir.id,
            node_id = %node_id,
            conditions = ?unmatched_conditions,
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

/// Evaluate a simple condition expression used by `Routing::Custom` entries and
/// `conditional_branch` guards (e.g. `response.action == "about"`,
/// `register.q_age >= 18`, `msg.text contains "hello"`).
///
/// Dotted paths resolve against the JSON context; an unresolved path is false.
/// Operators (detected longest-token-first so `>=`/`<=` win over `>`/`<`):
/// - `== ` / `!=` — case-insensitive string equality.
/// - `>=` / `<=` / `>` / `<` — numeric ordering; both operands are parsed as
///   `f64`, and a non-numeric operand makes the condition false (never a panic).
/// - `contains` — case-insensitive substring of the resolved string.
fn evaluate_simple_condition(condition: &str, ctx: &Value) -> bool {
    if let Some((path, expected)) = split_condition(condition, "==") {
        return string_eq(ctx, path, expected, false);
    }
    if let Some((path, expected)) = split_condition(condition, "!=") {
        return string_eq(ctx, path, expected, true);
    }
    if let Some((path, expected)) = split_condition(condition, ">=") {
        return numeric_cmp(ctx, path, expected, |a, b| a >= b);
    }
    if let Some((path, expected)) = split_condition(condition, "<=") {
        return numeric_cmp(ctx, path, expected, |a, b| a <= b);
    }
    if let Some((path, expected)) = split_condition(condition, ">") {
        return numeric_cmp(ctx, path, expected, |a, b| a > b);
    }
    if let Some((path, expected)) = split_condition(condition, "<") {
        return numeric_cmp(ctx, path, expected, |a, b| a < b);
    }
    if let Some((path, expected)) = split_condition(condition, " contains ") {
        let needle = expected.to_lowercase();
        return resolve_dotted_path(ctx, path)
            .is_some_and(|actual| actual.to_lowercase().contains(&needle));
    }
    false
}

/// Split a condition on the first occurrence of `op` into a trimmed
/// `(path, value)`, with surrounding quotes stripped from the value.
/// `None` when `op` is absent.
fn split_condition<'a>(condition: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let idx = condition.find(op)?;
    let path = condition[..idx].trim();
    let value = condition[idx + op.len()..].trim().trim_matches('"');
    Some((path, value))
}

/// Case-insensitive string equality of the resolved path against `expected`,
/// optionally negated. An unresolved path is treated as not-equal.
fn string_eq(ctx: &Value, path: &str, expected: &str, negate: bool) -> bool {
    let matches = resolve_dotted_path(ctx, path)
        .as_deref()
        .is_some_and(|a| a.eq_ignore_ascii_case(expected));
    if negate { !matches } else { matches }
}

/// Numeric comparison of the resolved path against `expected`. Both sides are
/// parsed as `f64`; if either fails to parse the condition is false.
fn numeric_cmp(ctx: &Value, path: &str, expected: &str, cmp: impl Fn(f64, f64) -> bool) -> bool {
    let Some(actual) = resolve_dotted_path(ctx, path).and_then(|a| a.trim().parse::<f64>().ok())
    else {
        return false;
    };
    let Ok(rhs) = expected.parse::<f64>() else {
        return false;
    };
    cmp(actual, rhs)
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

/// Success-family outcome ports, in the priority order used to pick the default
/// success `event` for a node that succeeded without emitting an explicit
/// `outcome`. `on_success` is first so components whose success name is the
/// historical default keep routing unchanged (e.g. http).
const SUCCESS_EVENT_PORTS: [&str; 3] = ["on_success", "on_complete", "on_submit"];

/// Error-family outcome ports, priority order, mirroring [`SUCCESS_EVENT_PORTS`]
/// for the failure (`ok == false`) branch. `on_error` is first so the historical
/// default is preserved; `on_cancel` / `on_timeout` let a node whose failure
/// port is named differently (qa cancel, http timeout) route instead of stalling.
const ERROR_EVENT_PORTS: [&str; 3] = ["on_error", "on_cancel", "on_timeout"];

/// Whether a node opts into node_io error routing: a `Routing::Custom` array with
/// at least one route targeting an error-family port (`on_error` / `on_cancel` /
/// `on_timeout`), either as an explicit `event` field or via an `event == "<port>"`
/// condition (the form the designer emits). Such a node surfaces a component
/// failure as an `{errors}` output routed to that branch; every other node keeps
/// the historical hard-fail (`bail!`) on error — so this change is purely additive.
fn node_has_error_route(routing: &Routing) -> bool {
    let Routing::Custom(raw) = routing else {
        return false;
    };
    let Some(routes) = raw.as_array() else {
        return false;
    };
    routes.iter().any(|route| {
        let by_event = route
            .get("event")
            .and_then(Value::as_str)
            .is_some_and(|e| ERROR_EVENT_PORTS.contains(&e));
        let by_condition = route
            .get("condition")
            .and_then(Value::as_str)
            .is_some_and(|c| ERROR_EVENT_PORTS.iter().any(|port| c.contains(port)));
        by_event || by_condition
    })
}

/// Derive the success `event` to default to when a node succeeds (`ok == true`)
/// but emits no explicit `outcome`. Designer-built nodes whose happy port is
/// `on_complete` (native `qa.process` / `llm.openai.chat` / `template_render`)
/// or `on_submit` (forms) compile to `event == "<port>"` conditions; with a
/// blanket `on_success` default those never match and the node stalls at
/// `Wait`. We instead pick the first success-family port the node actually has
/// an outgoing `event == "<port>"` edge for, so the happy path routes. Falls
/// back to `on_success` when no success-family port is referenced (preserving
/// the prior behaviour).
fn default_success_event(routes: &[Value]) -> &'static str {
    default_event(routes, &SUCCESS_EVENT_PORTS, "on_success")
}

/// Failure-branch counterpart of [`default_success_event`]: the `event` to
/// default to when a node fails (`ok == false`) without an explicit `outcome`.
/// Picks the first error-family port the node actually routes on, falling back
/// to `on_error`.
fn default_error_event(routes: &[Value]) -> &'static str {
    default_event(routes, &ERROR_EVENT_PORTS, "on_error")
}

/// Pick the first port in `ports` (priority order) that the node has an outgoing
/// `event == "<port>"` edge for; `fallback` when none is referenced.
fn default_event(routes: &[Value], ports: &[&'static str], fallback: &'static str) -> &'static str {
    let referenced: Vec<&str> = routes
        .iter()
        .filter_map(|route| route.get("condition").and_then(Value::as_str))
        .filter_map(condition_event_eq)
        .collect();
    ports
        .iter()
        .copied()
        .find(|port| referenced.contains(port))
        .unwrap_or(fallback)
}

/// Extract `<value>` from an `event == "<value>"` condition; `None` for any
/// other shape (different path, `!=`, no `==`).
fn condition_event_eq(condition: &str) -> Option<&str> {
    let idx = condition.find("==")?;
    if condition[..idx].trim() != "event" {
        return None;
    }
    Some(condition[idx + 2..].trim().trim_matches('"'))
}

/// Build the context a routing condition is evaluated against.
///
/// Layout:
/// ```text
/// {
///   ...output.payload...,     // this node's fields, spread at the top level
///   "entry":    <flow entry>,
///   "in":       <flow entry>, // alias for entry
///   "node":     { "<id>": <node_output_view>, ... },  // every prior node
///   "response": { <key>: <value>, ... },              // from envelope metadata
///   "event":    "<outcome>"   // the port this node routes on
/// }
/// ```
///
/// The spread comes FIRST and the named keys are inserted after, so
/// **`entry`, `in`, `node`, `response` and `event` are reserved**: a component
/// whose payload has a top-level field with one of those names has it shadowed
/// here. That is deliberate — the spread is what lets a guard say `q_age >= 18`
/// about its own node (and is what the designer's source-node prefix strip
/// relies on) — but it means those five names are not usable as payload fields
/// in a routed node.
///
/// `node` is the same `outputs_map()` projection [`template_context`] exposes,
/// so a condition resolves `node.<id>.<field>` exactly as a param template
/// resolves `{{node.<id>.<field>}}`. Note `vars` is NOT here: `vars.x` in a
/// condition does not resolve, whereas `{{vars.x}}` in a param does.
fn build_routing_context(
    output: &NodeOutput,
    state: &ExecutionState,
    success_event: &str,
    error_event: &str,
) -> Value {
    let mut ctx = match &output.payload {
        Value::Object(map) => map.clone(),
        _ => JsonMap::new(),
    };

    let entry = &state.entry;
    ctx.insert("entry".into(), entry.clone());
    ctx.insert("in".into(), entry.clone());

    // Every prior node's output, keyed by id — the SAME projection
    // `template_context` exposes for `{{node.<id>.<field>}}`, so a routing
    // condition resolves `node.<id>.<field>` exactly as a param template does.
    // Without this a guard could only read the current node's payload (spread
    // at the top level, above), so a condition naming any other node resolved
    // to nothing and silently took the false branch on every input.
    ctx.insert("node".into(), Value::Object(state.outputs_map()));

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

    // Inject the node's outcome as `event` so port-name routing
    // (`event == "<outcome>"`, emitted by the designer for nodes with multiple
    // outgoing edges) resolves. Prefer an explicit outcome the node emitted in
    // its output metadata; otherwise derive a default from `ok` — `success_event`
    // on success / `error_event` on failure (the success/error-family port the
    // node actually has an edge for; see `default_success_event` /
    // `default_error_event`). Without this, a multi-edge node falls through to
    // `Wait` at runtime.
    let event = output
        .meta
        .get("outcome")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if output.ok {
                success_event
            } else {
                error_event
            }
            .to_string()
        });
    ctx.insert("event".into(), Value::String(event));

    Value::Object(ctx)
}

/// Pure autonomy-gate decision for an `approval.call` node. Returns `true` when
/// the request must go to a human (dispatch), `false` when it auto-approves.
///
/// The gate config fields (`mode`, `risk_threshold`, `confidence_threshold`)
/// are compiled by the designer as FLAT fields directly on the node input
/// (not nested under a `gate` object); `risk`/`confidence` are already
/// flat/dynamic values populated at flow render time.
fn approval_requires_human(input: &Value) -> bool {
    let mode = input
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("always");
    match mode {
        "above_risk" => {
            let risk = input.get("risk").and_then(Value::as_f64).unwrap_or(0.0);
            let threshold = input
                .get("risk_threshold")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            risk >= threshold
        }
        "above_confidence" => {
            let confidence = input
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let threshold = input
                .get("confidence_threshold")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            confidence < threshold
        }
        // "always" and any unknown mode fail safe: require a human.
        _ => true,
    }
}

/// True when `entry` is a runtime-dispatch response envelope rather than a user
/// activity. The dispatch response always carries a top-level `ok`
/// (`{ok, output, events, error}`); an inbound activity never does. Mirrors the
/// conversational `dw.agent` discriminator (`state.entry.get("ok").is_some()`).
fn entry_is_approval_response(entry: &Value) -> bool {
    entry.get("ok").is_some()
}

/// Map an approval response envelope to the routing outcome that becomes
/// `event` via `NodeOutput.meta["outcome"]`.
///
/// A watchdog timeout arrives as `{ok: false, output: null, error: {code: "timeout"}}`
/// and wins over any decision. Otherwise the discriminator is `output.decision`
/// — NOT `ok`, which greentic-admin sets to `true` for approve *and* deny.
///
/// Fails closed: an unrecognised or absent decision is `denied`, mirroring
/// `approval_requires_human`'s own `_ => true` fail-safe. A corrupt payload must
/// never become a pass.
fn approval_outcome_from_entry(entry: &Value) -> &'static str {
    let timed_out = entry
        .pointer("/error/code")
        .and_then(Value::as_str)
        .is_some_and(|code| code.eq_ignore_ascii_case("timeout"));
    if timed_out {
        return "timeout";
    }
    // Fail closed: a failed envelope is never a pass, whatever `output` says.
    if entry.get("ok").and_then(Value::as_bool) == Some(false) {
        return "denied";
    }
    match entry.pointer("/output/decision").and_then(Value::as_str) {
        Some(decision) if decision.eq_ignore_ascii_case("approved") => "approved",
        _ => "denied",
    }
}

#[cfg(test)]
mod approval_gate_tests {
    use super::{approval_outcome_from_entry, approval_requires_human, entry_is_approval_response};
    use serde_json::json;

    #[test]
    fn above_risk_auto_approves_below_threshold() {
        let input = json!({ "risk": 0.5, "mode": "above_risk", "risk_threshold": 0.7 });
        assert!(!approval_requires_human(&input));
    }

    #[test]
    fn above_risk_requires_human_at_or_above_threshold() {
        let input = json!({ "risk": 0.9, "mode": "above_risk", "risk_threshold": 0.7 });
        assert!(approval_requires_human(&input));
    }

    #[test]
    fn above_confidence_requires_human_when_low_confidence() {
        let input =
            json!({ "confidence": 0.4, "mode": "above_confidence", "confidence_threshold": 0.8 });
        assert!(approval_requires_human(&input));
    }

    #[test]
    fn always_and_missing_gate_require_human() {
        assert!(approval_requires_human(&json!({ "mode": "always" })));
        assert!(approval_requires_human(&json!({})));
    }

    #[test]
    fn entry_is_a_response_only_when_the_envelope_has_ok() {
        // The dispatch response envelope always carries a top-level `ok`.
        assert!(entry_is_approval_response(
            &json!({ "ok": true, "output": { "decision": "approved" } })
        ));
        assert!(entry_is_approval_response(
            &json!({ "ok": false, "error": { "code": "timeout" } })
        ));
        // A user activity arriving mid-await has no top-level `ok`.
        assert!(!entry_is_approval_response(
            &json!({ "text": "hi", "metadata": { "action": "go" } })
        ));
        assert!(!entry_is_approval_response(&json!({})));
    }

    #[test]
    fn outcome_reads_the_decision() {
        assert_eq!(
            approval_outcome_from_entry(
                &json!({ "ok": true, "output": { "decision": "approved" } })
            ),
            "approved"
        );
        assert_eq!(
            approval_outcome_from_entry(&json!({ "ok": true, "output": { "decision": "denied" } })),
            "denied"
        );
    }

    #[test]
    fn timeout_wins_over_the_decision() {
        // The watchdog envelope has output: null and error.code == "timeout".
        assert_eq!(
            approval_outcome_from_entry(
                &json!({ "ok": false, "output": null, "error": { "code": "timeout" } })
            ),
            "timeout"
        );
    }

    #[test]
    fn unknown_or_missing_decision_fails_closed_to_denied() {
        // Fail closed: a corrupt payload must never become a pass.
        assert_eq!(
            approval_outcome_from_entry(&json!({ "ok": true, "output": { "decision": "maybe" } })),
            "denied"
        );
        assert_eq!(
            approval_outcome_from_entry(&json!({ "ok": true, "output": {} })),
            "denied"
        );
        assert_eq!(
            approval_outcome_from_entry(&json!({ "ok": true })),
            "denied"
        );
        assert_eq!(
            approval_outcome_from_entry(&json!({ "ok": false, "error": { "code": "nats_down" } })),
            "denied"
        );
    }

    #[test]
    fn a_failed_envelope_is_denied_even_if_it_carries_an_approval() {
        // Fail closed: `ok: false` with a non-timeout error must not pass,
        // regardless of a stale/forged decision field.
        assert_eq!(
            approval_outcome_from_entry(&json!({
                "ok": false,
                "error": { "code": "nats_down" },
                "output": { "decision": "approved" }
            })),
            "denied"
        );
    }

    #[test]
    fn decision_match_is_case_insensitive() {
        assert_eq!(
            approval_outcome_from_entry(
                &json!({ "ok": true, "output": { "decision": "Approved" } })
            ),
            "approved"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{ValidationConfig, ValidationMode};
    use greentic_types::{
        Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata, InputMapping, Node, NodeId,
        OutputMapping, Routing, TelemetryHints,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
            operala_node_handler: None,
        }
    }

    #[test]
    fn to_node_output_legacy_success_becomes_data() {
        // Legacy `{ok:true, ...fields}` (no node_io envelope) → Data{data}.
        let out = to_node_output(&json!({ "ok": true, "temp": "20C" }));
        assert!(out.is_ok(), "legacy ok:true must classify as Data");
        let data = out.data().expect("data present");
        assert_eq!(data.get("temp").and_then(Value::as_str), Some("20C"));
    }

    #[test]
    fn to_node_output_legacy_error_becomes_errors() {
        // Legacy `{ok:false, error:{code,message}}` → Errors{errors:[NodeError]}.
        let out = to_node_output(
            &json!({ "ok": false, "error": { "code": "E_BAD", "message": "boom" } }),
        );
        assert!(!out.is_ok(), "legacy ok:false must classify as Errors");
        let errs = out.errors();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].code, "E_BAD");
        assert_eq!(errs[0].message, "boom");
    }

    #[test]
    fn to_node_output_native_data_envelope_roundtrips() {
        // A node_io-native `{data:{...}}` envelope parses straight to Data.
        let out = to_node_output(&json!({ "data": { "x": 1 } }));
        assert!(out.is_ok());
        assert_eq!(
            out.data().and_then(|d| d.get("x")).and_then(Value::as_i64),
            Some(1)
        );
    }

    #[test]
    fn to_node_output_native_errors_envelope_roundtrips() {
        // A node_io-native `{errors:[...]}` envelope parses straight to Errors.
        let out = to_node_output(&json!({
            "errors": [ { "code": "C", "message": "m", "kind": "validation",
                          "retryable": false, "details": {} } ]
        }));
        assert!(!out.is_ok());
        assert_eq!(out.errors()[0].code, "C");
        assert_eq!(
            out.errors()[0].kind,
            greentic_types::node_io::ErrorKind::Validation
        );
    }

    #[test]
    fn to_node_output_bare_object_becomes_data() {
        // A bare result with no envelope keys → Data{data: <whole value>}.
        let out = to_node_output(&json!({ "foo": 1 }));
        assert!(out.is_ok());
        assert_eq!(
            out.data()
                .and_then(|d| d.get("foo"))
                .and_then(Value::as_i64),
            Some(1)
        );
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
    fn outputs_map_exposes_node_io_data_and_errors_alongside_flat() {
        let mut state = ExecutionState::new(json!({}));
        state.nodes.insert(
            "forecast".to_string(),
            NodeOutput::new(json!({ "temp": "20C" })),
        );
        let outs = state.outputs_map();
        // Legacy flat ref `{{node.forecast.temp}}` keeps working.
        assert_eq!(outs["forecast"]["temp"], json!("20C"));
        // Canonical node_io ref `{{node.forecast.data.temp}}` resolves to the same.
        assert_eq!(outs["forecast"]["data"]["temp"], json!("20C"));
        // `{{node.forecast.errors}}` is present and empty for a success output.
        assert_eq!(outs["forecast"]["errors"], json!([]));
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
            vars_out: None,
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
            vars_out: None,
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

    /// `approval.call` must survive lowering intact.
    ///
    /// packc emits the gate with `operation: None` because the id is already
    /// complete. Before `approval.` joined the `is_builtin` allowlist, lowering
    /// dot-split it into component `"approval"` + operation `"call"` — and
    /// `"approval"` is by construction absent from the pack's component map,
    /// which is keyed verbatim by `node.component.id`. Every human-approval gate
    /// then failed at dispatch with `component 'approval' not found in pack` on a
    /// perfectly well-formed pack, so rebuilding never helped.
    #[test]
    fn approval_call_component_id_is_not_dot_split() {
        let node = Node {
            id: NodeId::from_str("gate").unwrap(),
            component: FlowComponentRef {
                id: "approval.call".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "await": true, "input": { "mode": "always" } }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };
        let host_node = HostNode::from(node);
        assert_eq!(
            host_node.component_id(),
            "approval.call",
            "approval.call must stay intact; splitting it yields 'approval', which \
             is never a key in the pack's component map"
        );
        assert_eq!(
            host_node.operation_name(),
            None,
            "the id is already complete — lowering must not invent an operation"
        );
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
            conversational: false,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
            operala_node_handler: None,
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

        // The completed FlowExecution exposes its per-node outputs (captured
        // from ExecutionState.nodes before finalize_with drops them). This is
        // the map the demo reads to surface a mid-flow dw.agent's output.
        assert!(
            !result.node_outputs.is_empty(),
            "completed flow must expose its per-node outputs"
        );
        assert!(
            result
                .node_outputs
                .values()
                .any(|v| v.to_string().contains("logged")),
            "node_outputs must carry the executed node's payload: {:?}",
            result.node_outputs
        );

        let starts = observer.starts.lock().unwrap();
        let ends = observer.ends.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0], json!({ "message": "logged" }));
    }

    #[test]
    fn dotted_component_id_with_mapping_operation_is_not_split() {
        // greentic-pack resolves a component node to a bare component symbol and
        // keeps the operation in the input mapping. The runtime must NOT split the
        // dotted symbol on the last dot (which would yield `ai.greentic`, "not
        // found in pack"); the structured mapping operation makes the id a
        // complete reference.
        let node = Node {
            conversational: false,
            id: NodeId::from_str("render").unwrap(),
            component: FlowComponentRef {
                id: "ai.greentic.component-templates".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "operation": "handle_message", "input": "hi" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let host = HostNode::from(node);
        assert!(
            matches!(&host.kind, NodeKind::PackComponent { component_ref } if component_ref == "ai.greentic.component-templates"),
            "dotted component id must stay intact, got kind {:?}",
            host.kind
        );
        assert_eq!(host.component, "ai.greentic.component-templates");
        assert_eq!(host.operation_in_mapping(), Some("handle_message"));
    }

    #[test]
    fn packed_component_operation_id_still_splits_without_mapping_operation() {
        // Legacy encoding: the operation is packed into the id as
        // `<component>.<operation>` and absent from the mapping. The last-dot
        // split must still recover it.
        let node = Node {
            conversational: false,
            id: NodeId::from_str("render").unwrap(),
            component: FlowComponentRef {
                id: "templating.handlebars".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "text": "hello" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
        };
        let host = HostNode::from(node);
        assert!(
            matches!(&host.kind, NodeKind::PackComponent { component_ref } if component_ref == "templating"),
            "packed <component>.<operation> id must split, got kind {:?}",
            host.kind
        );
        assert_eq!(host.operation_name(), Some("handlebars"));
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
                conversational: false,
                opening_message: None,
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
        let handler: Arc<dyn AgentNodeHandler> =
            Arc::new(RuntimeAgentNodeHandler::new(runtime, None, None, None));

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
            conversational: false,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: Some(handler),
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
            operala_node_handler: None,
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
            conversational: false,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: Some(handler_dyn),
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
            operala_node_handler: None,
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
            conversational: false,
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
            conversational: false,
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
            rollout_ids: RolloutIds::default(),
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
            operala_node_handler: None,
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
                conversational: false,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: None,
            #[cfg(feature = "agentic-worker")]
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            #[cfg(feature = "agentic-worker")]
            agent_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            graph_node_handler: None,
            #[cfg(feature = "agentic-worker")]
            mcp_tool_source: None,
            operala_node_handler: None,
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
    fn with_rollout_ids_binds_revision_identity() {
        let engine = minimal_engine().with_rollout_ids(RolloutIds {
            customer_id: Some("cust-acme".into()),
            deployment_id: Some("01JTKS".into()),
            bundle_id: Some("customer.support".into()),
            revision_id: Some("01JTKR".into()),
        });
        assert_eq!(engine.rollout_ids.revision_id.as_deref(), Some("01JTKR"));
        assert_eq!(engine.rollout_ids.deployment_id.as_deref(), Some("01JTKS"));
        // A freshly-built engine carries no rollout identity (legacy runtime).
        assert!(minimal_engine().rollout_ids.is_empty());
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

    #[test]
    fn park_turns_bump_and_reset() {
        let mut state = ExecutionState::new(Value::Null);
        assert_eq!(state.bump_park_turns("a"), 1);
        assert_eq!(state.bump_park_turns("a"), 2);
        // A different node's counter is independent.
        assert_eq!(state.bump_park_turns("b"), 1);
        assert_eq!(state.bump_park_turns("a"), 3);

        state.reset_park_turns("a");
        assert_eq!(
            state.bump_park_turns("a"),
            1,
            "reset must restart from zero"
        );
        assert_eq!(
            state.bump_park_turns("b"),
            2,
            "resetting `a` must not disturb `b`'s counter"
        );
    }

    #[test]
    fn park_turns_survives_snapshot_roundtrip() {
        // park_turns must persist across a park/resume, which is a serde
        // round-trip of ExecutionState (same contract as redirect_count /
        // vars — see execution_state_vars_survive_serde_round_trip).
        let mut state = ExecutionState::new(json!({}));
        state.bump_park_turns("agent-1");
        state.bump_park_turns("agent-1");

        let encoded = serde_json::to_string(&state).expect("serialize");
        let decoded: ExecutionState = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.park_turns.get("agent-1"), Some(&2));

        // A legacy snapshot serialized before `park_turns` existed (no
        // `park_turns` key) must still load, defaulting to an empty map.
        let legacy = r#"{"entry":{},"input":{},"nodes":{},"egress":[],"redirect_count":0}"#;
        let decoded_legacy: ExecutionState = serde_json::from_str(legacy).expect("legacy loads");
        assert!(decoded_legacy.park_turns.is_empty());
    }

    #[test]
    fn pending_agent_await_mark_and_take() {
        let mut st = ExecutionState::new(serde_json::json!({}));
        assert!(!st.take_agent_await("agent"), "unmarked node takes false");
        st.mark_agent_await("agent");
        assert!(st.take_agent_await("agent"), "marked node takes true");
        assert!(!st.take_agent_await("agent"), "take clears the marker");
        // Independent per node.
        st.mark_agent_await("a");
        st.mark_agent_await("b");
        assert!(st.take_agent_await("a"));
        assert!(st.take_agent_await("b"));
    }

    #[test]
    fn pending_agent_await_survives_snapshot_roundtrip() {
        let mut st = ExecutionState::new(serde_json::json!({}));
        st.mark_agent_await("agent");
        let json = serde_json::to_string(&st).expect("serialize");
        let back: ExecutionState = serde_json::from_str(&json).expect("deserialize");
        let mut back = back;
        assert!(
            back.take_agent_await("agent"),
            "marker survives serde round-trip"
        );
        // Legacy snapshot without the key decodes to empty (serde default).
        let legacy = r#"{"entry":{},"input":{},"nodes":{},"egress":[],"redirect_count":0,"vars":{},"park_turns":{}}"#;
        let mut legacy: ExecutionState = serde_json::from_str(legacy).expect("legacy decode");
        assert!(!legacy.take_agent_await("agent"), "absent key → empty");
    }

    #[test]
    fn approval_await_mark_and_take() {
        let mut st = ExecutionState::new(json!({}));
        assert!(!st.take_approval_await("gate"), "unmarked node takes false");
        st.mark_approval_await("gate");
        assert!(st.take_approval_await("gate"), "marked node takes true");
        assert!(!st.take_approval_await("gate"), "take clears the mark");
    }

    #[test]
    fn approval_await_survives_snapshot_roundtrip() {
        let mut st = ExecutionState::new(json!({}));
        st.mark_approval_await("gate");
        let encoded = serde_json::to_string(&st).expect("serialize");
        let mut decoded: ExecutionState = serde_json::from_str(&encoded).expect("deserialize");
        assert!(
            decoded.take_approval_await("gate"),
            "the marker must survive a park/resume snapshot"
        );
    }

    #[test]
    fn approval_await_defaults_empty_for_old_snapshots() {
        // Snapshots persisted before this field exists must still decode.
        let mut decoded: ExecutionState =
            serde_json::from_str(r#"{"entry":{},"input":{}}"#).expect("old snapshot decodes");
        assert!(!decoded.take_approval_await("gate"));
    }

    #[test]
    fn dispatch_outcome_await_here_stores_variant() {
        // DispatchOutcome::await_here must store NodeControl::AwaitHere with
        // both the reason and the correlation_id carried through unchanged
        // (Task 4/5 build on this; the drive-loop handler resumes at self —
        // see the behavioral coverage in Task 6).
        let output = NodeOutput::new(json!({"ok": true}));
        let outcome = DispatchOutcome::await_here(
            output,
            Some("awaiting agent response".to_string()),
            "corr-123".to_string(),
        );
        match outcome.control {
            NodeControl::AwaitHere {
                reason,
                correlation_id,
            } => {
                assert_eq!(reason.as_deref(), Some("awaiting agent response"));
                assert_eq!(correlation_id, "corr-123");
            }
            other => panic!("expected NodeControl::AwaitHere, got {other:?}"),
        }
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
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
            slot_schema: None,
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

    #[test]
    fn node_output_with_error_marks_ok_false_and_stashes_in_meta() {
        let err: Box<dyn std::error::Error + 'static> =
            Box::<dyn std::error::Error + 'static>::from("weatherapi returned 401 Unauthorized");
        let out = NodeOutput::with_error("call_weather", err.as_ref());
        assert!(!out.ok);
        assert_eq!(out.payload, Value::Null);
        assert_eq!(out.meta["error"]["kind"], "flow_node_failed");
        assert_eq!(out.meta["error"]["node_id"], "call_weather");
        assert_eq!(
            out.meta["error"]["message"],
            "weatherapi returned 401 Unauthorized"
        );
    }

    #[test]
    fn lift_first_node_error_promotes_node_meta_to_output_metadata() {
        // Two nodes ran; the first failed, the second produced a default-
        // looking output (flow author wrote no error routing). The executor
        // must lift the first failure into output.metadata so the messaging
        // provider renders the error card without any flow-author changes.
        let mut nodes: HashMap<String, NodeOutput> = HashMap::new();
        let err: Box<dyn std::error::Error + 'static> =
            Box::<dyn std::error::Error + 'static>::from("weatherapi returned 401 Unauthorized");
        nodes.insert(
            "call_weather".to_string(),
            NodeOutput::with_error("call_weather", err.as_ref()),
        );
        nodes.insert(
            "render_current_card".to_string(),
            NodeOutput::new(json!({ "text": "message" })),
        );

        let final_output = json!({ "text": "message" });
        let enriched = lift_first_node_error_from_nodes(final_output, &nodes);
        assert_eq!(
            enriched["metadata"]["error_kind"], "flow_node_failed",
            "first failing node's kind must be lifted"
        );
        assert_eq!(
            enriched["metadata"]["error_message"],
            "weatherapi returned 401 Unauthorized"
        );
        assert_eq!(enriched["metadata"]["node_id"], "call_weather");
        // Preserves the original payload bits so downstream renderers still
        // see what the flow produced.
        assert_eq!(enriched["text"], "message");
    }

    #[test]
    fn lift_first_node_error_is_noop_when_all_nodes_ok() {
        let mut nodes: HashMap<String, NodeOutput> = HashMap::new();
        nodes.insert(
            "ok_node".to_string(),
            NodeOutput::new(json!({ "text": "all good" })),
        );
        let output = json!({ "text": "all good" });
        let lifted = lift_first_node_error_from_nodes(output.clone(), &nodes);
        assert_eq!(lifted, output);
    }

    #[tokio::test]
    async fn execute_user_facing_flow_failure_returns_completed_with_error_envelope() {
        // Flow whose start node is missing — drive_flow will return Err on
        // node lookup. With session_id present, execute() must convert that
        // to a Completed FlowExecution carrying error_kind/error_message in
        // output.metadata so the chat user sees the error card.
        let flow_id_str = "broken.flow";
        let pack_id_str = "test-pack";
        let host_flow = host_flow_for_test(flow_id_str, &["only-node"], Some("does-not-exist"));
        let engine = FlowEngine {
            operala_node_handler: None,
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: pack_id_str.to_string(),
                    flow_id: flow_id_str.to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            rollout_ids: RolloutIds::default(),
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
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: pack_id_str,
            flow_id: flow_id_str,
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("conv-1"),
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
        let result = engine
            .execute(ctx, Value::Null)
            .await
            .expect("must not propagate Err");
        assert!(matches!(result.status, FlowStatus::Completed));
        assert_eq!(
            result.output["metadata"]["error_kind"],
            "flow_execution_failed"
        );
        let msg = result.output["metadata"]["error_message"]
            .as_str()
            .unwrap_or("");
        assert!(!msg.is_empty(), "error_message must be populated");
        assert_eq!(result.output["metadata"]["flow_id"], "broken.flow");
        // Without a `text`, this envelope reaches nobody: every messaging
        // provider refuses a payload with no text/card, webchat included (its
        // required-content guard runs BEFORE its error-card path). The comment
        // above once claimed the user "sees the error card"; that only held for
        // the `lift_first_node_error_from_nodes` envelope, which enriches an
        // output that already carries text.
        assert_eq!(
            result.output["text"], USER_FACING_FLOW_FAILURE_TEXT,
            "a terminal session-flow failure must carry user-safe text or it is silent on every channel"
        );
        // And the engine's own wording must NOT be what the user is shown.
        assert!(
            !result.output["text"]
                .as_str()
                .unwrap_or_default()
                .contains("does-not-exist"),
            "raw engine text must stay in metadata.error_message"
        );
    }

    #[test]
    fn mcp_tool_error_recognises_generator_error_shape() {
        // greentic-mcp-generator's tool_error_with_status emits this exact
        // shape when the upstream HTTP call to weatherapi.com returns 401.
        let value = json!({
            "error": {
                "code": "tool_error",
                "message": "API request returned status 401",
                "status": 401
            }
        });
        let (code, message) = mcp_tool_error(&value).expect("must detect MCP error shape");
        assert_eq!(code, "tool_error");
        assert!(message.contains("API request returned status 401"));
        assert!(message.contains("(status 401)"));
    }

    #[test]
    fn mcp_tool_error_skips_success_responses() {
        // A success response uses `result`, not `error`.
        let value = json!({ "result": { "current": { "temp_c": 19.0 } } });
        assert!(mcp_tool_error(&value).is_none());
    }

    #[test]
    fn mcp_tool_error_skips_non_object_and_unrelated_shapes() {
        assert!(mcp_tool_error(&Value::Null).is_none());
        assert!(mcp_tool_error(&json!({"unrelated": true})).is_none());
        // `error` must be an object; a string isn't enough.
        assert!(mcp_tool_error(&json!({"error": "oops"})).is_none());
    }

    #[tokio::test]
    async fn execute_non_user_facing_flow_failure_still_propagates() {
        // No session_id => internal job. Errors still propagate as Err so
        // operator alerting / metrics pipelines stay intact.
        let flow_id_str = "broken.flow";
        let pack_id_str = "test-pack";
        let host_flow = host_flow_for_test(flow_id_str, &["only-node"], Some("does-not-exist"));
        let engine = FlowEngine {
            operala_node_handler: None,
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: pack_id_str.to_string(),
                    flow_id: flow_id_str.to_string(),
                },
                host_flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            rollout_ids: RolloutIds::default(),
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
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: pack_id_str,
            flow_id: flow_id_str,
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
        };
        let result = engine.execute(ctx, Value::Null).await;
        assert!(result.is_err(), "non-user-facing flow must propagate Err");
    }

    // ---- Phase D: slot_schema injection tests ----

    #[test]
    fn host_flow_extracts_slot_schema_from_metadata_extra() {
        use greentic_types::FlowMetadata;
        use std::collections::BTreeSet;

        let schema = json!([
            {"name": "counterparty", "slot_type": "string", "required": true},
            {"name": "due_date", "slot_type": "date", "required": true}
        ]);
        let flow = Flow {
            schema_version: "flow-v1".into(),
            id: FlowId::from_str("test.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::new(),
            nodes: IndexMap::default(),
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: BTreeSet::new(),
                extra: json!({(SLOT_SCHEMA_METADATA_KEY): schema}),
            },
        };
        let host = HostFlow::from(flow);
        assert_eq!(
            host.slot_schema.as_ref(),
            Some(&schema),
            "HostFlow must extract slot_schema from metadata.extra"
        );
    }

    #[test]
    fn host_flow_slot_schema_is_none_when_absent() {
        let flow = Flow {
            schema_version: "flow-v1".into(),
            id: FlowId::from_str("test.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::new(),
            nodes: IndexMap::default(),
            metadata: Default::default(),
        };
        let host = HostFlow::from(flow);
        assert!(
            host.slot_schema.is_none(),
            "HostFlow.slot_schema must be None when metadata.extra has no greentic.slot_schema"
        );
    }

    #[test]
    fn inject_slot_definitions_adds_to_object_input() {
        let schema = json!([
            {"name": "city", "slot_type": "string"}
        ]);
        let mut input = json!({"utterance": "hello"});
        inject_slot_definitions(&mut input, &schema, "f", "n");
        assert_eq!(
            input,
            json!({"utterance": "hello", "slot_definitions": schema}),
            "slot_definitions must be injected into existing object"
        );
    }

    #[test]
    fn inject_slot_definitions_wraps_null_input() {
        let schema = json!([{"name": "x", "slot_type": "string"}]);
        let mut input = Value::Null;
        inject_slot_definitions(&mut input, &schema, "f", "n");
        assert_eq!(
            input,
            json!({"slot_definitions": schema}),
            "null input must become an object with slot_definitions"
        );
    }

    #[test]
    fn inject_slot_definitions_preserves_explicit_inline() {
        let flow_schema = json!([{"name": "city", "slot_type": "string"}]);
        let inline_defs = json!([{"name": "country", "slot_type": "string"}]);
        let mut input = json!({
            "utterance": "hello",
            "slot_definitions": inline_defs
        });
        inject_slot_definitions(&mut input, &flow_schema, "f", "n");
        assert_eq!(
            input["slot_definitions"], inline_defs,
            "explicit inline slot_definitions must not be overwritten"
        );
    }

    #[test]
    fn inject_slot_definitions_skips_non_object_input() {
        let schema = json!([{"name": "x", "slot_type": "string"}]);
        let mut input = json!("a string");
        inject_slot_definitions(&mut input, &schema, "f", "n");
        assert_eq!(
            input,
            json!("a string"),
            "non-object input must be left unchanged"
        );
    }

    fn make_flow_doc_for_test(
        id: &str,
        node_name: &str,
        component: &str,
        slot_schema: Option<Value>,
    ) -> greentic_flow::model::FlowDoc {
        use greentic_flow::model::{FlowDoc, NodeDoc};

        let mut nodes = IndexMap::new();
        nodes.insert(
            node_name.to_string(),
            NodeDoc {
                raw: {
                    let mut m = IndexMap::new();
                    m.insert(
                        "component.exec".to_string(),
                        json!({ "component": component }),
                    );
                    m
                },
                routing: json!([{ "out": true }]),
                ..Default::default()
            },
        );

        FlowDoc {
            id: id.into(),
            title: None,
            description: None,
            flow_type: "messaging".into(),
            start: Some(node_name.into()),
            parameters: json!({}),
            tags: Vec::new(),
            schema_version: None,
            entrypoints: IndexMap::new(),
            meta: None,
            slot_schema,
            nodes,
        }
    }

    /// Integration test: exercises the real `greentic_flow::compile_flow`
    /// producer path with a `FlowDoc` carrying `slot_schema`, then converts
    /// through `HostFlow::from` and verifies the runtime-side `slot_schema`
    /// field is populated — closing the gap Codex flagged where the existing
    /// unit tests constructed `FlowMetadata` directly.
    #[test]
    fn compile_flow_round_trips_slot_schema_into_host_flow() {
        let slot_defs = json!([
            { "name": "counterparty", "slot_type": "string", "required": true,
              "pattern": ".+" },
            { "name": "due_date", "slot_type": "date", "required": true,
              "pattern": "\\d{4}-\\d{2}-\\d{2}" }
        ]);
        let doc = make_flow_doc_for_test(
            "slot-test",
            "extractor",
            "slot-extractor",
            Some(slot_defs.clone()),
        );

        let flow = greentic_flow::compile_flow(doc).expect("compile_flow must succeed");
        assert_eq!(
            flow.metadata.extra.get(SLOT_SCHEMA_METADATA_KEY),
            Some(&slot_defs),
            "compile_flow must forward slot_schema into metadata.extra"
        );

        let host = HostFlow::from(flow);
        assert_eq!(
            host.slot_schema.as_ref(),
            Some(&slot_defs),
            "HostFlow.slot_schema must survive the compile_flow -> HostFlow round-trip"
        );
    }

    /// Verify that `compile_flow` without `slot_schema` produces a `Flow`
    /// whose `metadata.extra` has no `greentic.slot_schema` key, and that
    /// `HostFlow.slot_schema` stays `None` through the real compile path.
    #[test]
    fn compile_flow_without_slot_schema_leaves_host_flow_none() {
        let doc = make_flow_doc_for_test("no-slots", "echo", "echo", None);

        let flow = greentic_flow::compile_flow(doc).expect("compile_flow must succeed");
        assert!(
            flow.metadata.extra.get(SLOT_SCHEMA_METADATA_KEY).is_none(),
            "metadata.extra must not contain greentic.slot_schema when FlowDoc.slot_schema is None"
        );

        let host = HostFlow::from(flow);
        assert!(
            host.slot_schema.is_none(),
            "HostFlow.slot_schema must be None when FlowDoc has no slot_schema"
        );
    }

    /// A node with 2+ outgoing edges compiles (in the designer) to
    /// `event == "<outcome>"` conditions. The runner must inject `event` into
    /// the routing context — from the node's emitted outcome, else a default
    /// derived from `ok` — so those conditions resolve instead of falling
    /// through to `Wait`. Without the injection, multi-edge nodes silently
    /// pause at runtime.
    #[test]
    fn multi_edge_node_routes_on_injected_event() {
        let raw_routing = json!([
            { "condition": "event == \"on_success\"", "to": "next" },
            { "condition": "event == \"on_error\"", "to": "err" }
        ]);
        let flow_ir = HostFlow {
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
            slot_schema: None,
        };
        let current = NodeId::from_str("current").unwrap();
        let state = ExecutionState::new(json!({}));

        // ok:true with no explicit outcome → default event "on_success" → "next".
        let ok_out = NodeOutput::new(json!({ "x": 1 }));
        match evaluate_custom_routing(&raw_routing, &ok_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "next"),
            other => panic!("expected Next(\"next\"), got {other:?}"),
        }

        // An explicit outcome in the node metadata wins over the ok-default.
        let routed = NodeOutput::with_meta(json!({}), json!({ "outcome": "on_error" }));
        match evaluate_custom_routing(&raw_routing, &routed, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "err"),
            other => panic!("expected Next(\"err\"), got {other:?}"),
        }
    }

    #[test]
    fn approval_outcomes_route_three_ways() {
        // The contract this feature exists for: an approval node's decision
        // selects the branch. `meta["outcome"]` wins over the ok-derived
        // default, so `ok: true` still routes to "denied" when denied.
        let raw_routing = json!([
            { "condition": "event == \"approved\"", "to": "do_it" },
            { "condition": "event == \"denied\"", "to": "reject" },
            { "condition": "event == \"timeout\"", "to": "escalate" }
        ]);
        let flow_ir = HostFlow {
            slot_schema: None,
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
        };
        let current = NodeId::from_str("gate").unwrap();
        let state = ExecutionState::new(json!({}));

        for (outcome, expected) in [
            ("approved", "do_it"),
            ("denied", "reject"),
            ("timeout", "escalate"),
        ] {
            let out = NodeOutput::with_meta(json!({}), json!({ "outcome": outcome }));
            match evaluate_custom_routing(&raw_routing, &out, &state, &flow_ir, &current) {
                CustomRoutingDecision::Next(nid) => assert_eq!(
                    nid.as_str(),
                    expected,
                    "outcome {outcome} must route to {expected}"
                ),
                other => panic!("outcome {outcome}: expected Next({expected}), got {other:?}"),
            }
        }
    }

    /// A node whose component reports a failure (`{ok:false, error}`) and which
    /// has an `on_error`-family route must surface a node_io `Errors` output
    /// (`ok == false`) and route to that branch instead of aborting the flow.
    #[test]
    fn errored_output_routes_to_on_error_branch() {
        let raw_routing = json!([
            { "condition": "event == \"on_success\"", "to": "ok_node" },
            { "condition": "event == \"on_error\"", "to": "err_node" }
        ]);
        let flow_ir = HostFlow {
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
            slot_schema: None,
        };
        let current = NodeId::from_str("current").unwrap();
        let state = ExecutionState::new(json!({}));

        let errored =
            NodeOutput::errored(json!({ "ok": false, "error": { "code": "E", "message": "m" } }));
        match evaluate_custom_routing(&raw_routing, &errored, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "err_node"),
            other => panic!("expected on_error route, got {other:?}"),
        }
    }

    #[test]
    fn node_has_error_route_detects_error_family_ports() {
        let with_err = Routing::Custom(json!([
            { "condition": "event == \"on_success\"", "to": "n" },
            { "condition": "event == \"on_error\"", "to": "e" }
        ]));
        assert!(
            node_has_error_route(&with_err),
            "on_error route must be detected"
        );

        let only_success = Routing::Custom(json!([
            { "condition": "event == \"on_success\"", "to": "n" }
        ]));
        assert!(
            !node_has_error_route(&only_success),
            "a success-only Custom routing has no error branch"
        );

        let plain = Routing::Next {
            node_id: NodeId::from_str("n").unwrap(),
        };
        assert!(
            !node_has_error_route(&plain),
            "Routing::Next has no error branch"
        );
    }

    /// When a successful node emits no explicit `outcome`, the runner must
    /// derive the success `event` from the success-family port the node
    /// actually has an outgoing edge for (priority `on_success` → `on_complete`
    /// → `on_submit`), not blindly default to `on_success`. This is what lets
    /// native nodes whose happy port is `on_complete` (qa.process,
    /// llm.openai.chat, template_render) — or `on_submit` (forms) — route
    /// instead of silently stalling at `Wait`, while leaving `on_success`
    /// components (e.g. http) unchanged.
    #[test]
    fn success_default_matches_available_outcome_port() {
        let flow_ir = HostFlow {
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
            slot_schema: None,
        };
        let current = NodeId::from_str("current").unwrap();
        let state = ExecutionState::new(json!({}));
        // ok:true, no explicit outcome — the case every native happy path hits.
        let ok_out = NodeOutput::new(json!({ "answer": "hi" }));

        // qa/llm/template shape: happy port is `on_complete`, no `on_success` edge.
        let on_complete_routing = json!([
            { "condition": "event == \"on_complete\"", "to": "next" },
            { "condition": "event == \"on_cancel\"", "to": "cancelled" }
        ]);
        match evaluate_custom_routing(&on_complete_routing, &ok_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "next"),
            other => panic!("expected Next(\"next\") via on_complete default, got {other:?}"),
        }

        // form shape: happy port is `on_submit`.
        let on_submit_routing = json!([
            { "condition": "event == \"on_submit\"", "to": "saved" },
            { "condition": "event == \"on_cancel\"", "to": "cancelled" }
        ]);
        match evaluate_custom_routing(&on_submit_routing, &ok_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "saved"),
            other => panic!("expected Next(\"saved\") via on_submit default, got {other:?}"),
        }

        // http shape: `on_success` present → still routes on_success (priority,
        // no regression for components whose success name is the old default).
        let on_success_routing = json!([
            { "condition": "event == \"on_success\"", "to": "ok" },
            { "condition": "event == \"on_error\"", "to": "err" }
        ]);
        match evaluate_custom_routing(&on_success_routing, &ok_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "ok"),
            other => panic!("expected Next(\"ok\") via on_success default, got {other:?}"),
        }
    }

    /// `evaluate_simple_condition` is a pure expression-parser test: it checks
    /// operator parsing over an arbitrary, hand-built JSON context. This
    /// context is **not** the shape `build_routing_context` produces at
    /// runtime — it is not node-keyed, and the keys below (`a`/`b`/`c`) are
    /// deliberately arbitrary so they cannot be mistaken for node ids. For
    /// coverage of the real routing context (prior node outputs exposed
    /// under `node.<id>`), see
    /// `routing_context_exposes_prior_node_outputs_under_node`.
    ///
    /// What this test does pin (PR #486): beyond `==`/`!=`, the parser must
    /// handle numeric ordering (`>=` `<=` `>` `<`) and `contains`
    /// (case-insensitive substring); otherwise those conditions silently
    /// evaluate to false and route wrong.
    #[test]
    fn condition_evaluator_supports_comparisons_and_contains() {
        let ctx = json!({
            "a": { "age": 18 },
            "b": { "status": "ok" },
            "c": { "text": "Hello World" }
        });

        // Numeric ordering (operands parsed as numbers).
        assert!(evaluate_simple_condition("a.age >= 18", &ctx));
        assert!(!evaluate_simple_condition("a.age > 18", &ctx));
        assert!(evaluate_simple_condition("a.age <= 18", &ctx));
        assert!(!evaluate_simple_condition("a.age < 18", &ctx));

        // contains: case-insensitive substring over the resolved string.
        assert!(evaluate_simple_condition("c.text contains \"world\"", &ctx));
        assert!(!evaluate_simple_condition("c.text contains \"bye\"", &ctx));

        // Existing equality semantics unchanged (regression guard).
        assert!(evaluate_simple_condition("b.status == \"ok\"", &ctx));
        assert!(!evaluate_simple_condition("b.status != \"ok\"", &ctx));
        // A non-numeric operand on an ordering op is false, not a panic.
        assert!(!evaluate_simple_condition("b.status >= 1", &ctx));
    }

    /// A routing condition must be able to read ANY prior node's output, not
    /// just the immediate predecessor's payload. `state.nodes` has always held
    /// them; the routing context simply never exposed them, so
    /// `node.<id>.<field>` resolved to nothing and the guard silently took the
    /// false branch on every input.
    ///
    /// This drives the REAL `build_routing_context` — see
    /// `condition_evaluator_supports_comparisons_and_contains` for why a
    /// hand-built context proves nothing here.
    #[test]
    fn routing_context_exposes_prior_node_outputs_under_node() {
        let mut state = ExecutionState::new(json!({}));
        state
            .nodes
            .insert("register".into(), NodeOutput::new(json!({ "q_age": 21 })));

        let current = NodeOutput::new(json!({ "status": "ok" }));
        let ctx = build_routing_context(&current, &state, "on_success", "on_error");

        // The cross-node form, matching `{{node.<id>.<field>}}` in params.
        assert!(
            evaluate_simple_condition("node.register.q_age >= 18", &ctx),
            "a prior node's output must be readable via node.<id>.<field>: {ctx:?}"
        );
        // The node_io envelope resolves too — same projection as params.
        assert!(
            evaluate_simple_condition("node.register.data.q_age >= 18", &ctx),
            "the data envelope must resolve like it does in params: {ctx:?}"
        );
    }

    #[test]
    fn routing_context_keeps_the_predecessor_shorthand() {
        // Negative-ish: the existing form must not regress. The current node's
        // payload stays spread at the top level, which is what PR #665's
        // source-node prefix strip relies on.
        let mut state = ExecutionState::new(json!({}));
        state
            .nodes
            .insert("register".into(), NodeOutput::new(json!({ "q_age": 21 })));

        let current = NodeOutput::new(json!({ "q_age": 30 }));
        let ctx = build_routing_context(&current, &state, "on_success", "on_error");

        assert!(
            evaluate_simple_condition("q_age >= 30", &ctx),
            "the bare form must still resolve against the current payload: {ctx:?}"
        );
    }

    #[test]
    fn routing_context_does_not_resolve_a_missing_node() {
        // Negative: a ref to a node with no output must NOT resolve — it must
        // stay false, not accidentally match something.
        let state = ExecutionState::new(json!({}));
        let current = NodeOutput::new(json!({ "status": "ok" }));
        let ctx = build_routing_context(&current, &state, "on_success", "on_error");

        assert!(
            !evaluate_simple_condition("node.ghost.x == \"y\"", &ctx),
            "a missing node must not resolve: {ctx:?}"
        );
    }

    /// Symmetric to the success default: when a node FAILS (`ok == false`)
    /// without an explicit outcome, route to the error-family port the node
    /// actually has an edge for (priority `on_error` → `on_cancel` →
    /// `on_timeout`), not blindly `on_error`. Lets a node whose failure port is
    /// `on_cancel` (qa) or `on_timeout` (http) route instead of stalling.
    #[test]
    fn failure_default_matches_available_outcome_port() {
        let flow_ir = HostFlow {
            id: "flow.test".to_string(),
            start: None,
            nodes: IndexMap::new(),
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
            slot_schema: None,
        };
        let current = NodeId::from_str("current").unwrap();
        let state = ExecutionState::new(json!({}));
        // ok:false, no explicit outcome — the failure case.
        let err_out = NodeOutput {
            ok: false,
            payload: json!({}),
            meta: Value::Null,
        };

        // qa shape: failure port is `on_cancel`, no `on_error` edge.
        let on_cancel_routing = json!([
            { "condition": "event == \"on_complete\"", "to": "next" },
            { "condition": "event == \"on_cancel\"", "to": "cancelled" }
        ]);
        match evaluate_custom_routing(&on_cancel_routing, &err_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "cancelled"),
            other => panic!("expected Next(\"cancelled\") via on_cancel default, got {other:?}"),
        }

        // http shape: `on_error` present → on_error (priority, unchanged).
        let on_error_routing = json!([
            { "condition": "event == \"on_success\"", "to": "ok" },
            { "condition": "event == \"on_error\"", "to": "err" }
        ]);
        match evaluate_custom_routing(&on_error_routing, &err_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "err"),
            other => panic!("expected Next(\"err\") via on_error default, got {other:?}"),
        }

        // on_timeout-only failure port.
        let on_timeout_routing = json!([
            { "condition": "event == \"on_success\"", "to": "ok" },
            { "condition": "event == \"on_timeout\"", "to": "timed_out" }
        ]);
        match evaluate_custom_routing(&on_timeout_routing, &err_out, &state, &flow_ir, &current) {
            CustomRoutingDecision::Next(nid) => assert_eq!(nid.as_str(), "timed_out"),
            other => panic!("expected Next(\"timed_out\") via on_timeout default, got {other:?}"),
        }
    }

    #[test]
    fn outcome_meta_surfaces_component_emitted_outcome() {
        // A component opts into outcome routing by adding `outcome` to its
        // output envelope; the runner surfaces it as node meta for routing.
        assert_eq!(
            outcome_meta(&json!({ "ok": true, "outcome": "on_complete" })),
            json!({ "outcome": "on_complete" })
        );
        // No `outcome` → null meta → engine uses the ok-derived default.
        assert_eq!(
            outcome_meta(&json!({ "ok": true, "body": {} })),
            Value::Null
        );
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
            conversational: false,
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
            conversational: false,
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
            rollout_ids: RolloutIds::default(),
            remote_dispatch_handler: Some(
                nats_engine_dispatcher
                    as Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>,
            ),
            dw_agent_dispatch: DwAgentDispatch::Nats,
            agent_node_handler: None,
            graph_node_handler: None,
            mcp_tool_source: None,
            operala_node_handler: None,
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

    #[test]
    fn execution_state_vars_survive_serde_round_trip() {
        // vars must persist across a park/resume, which is a serde round-trip of ExecutionState.
        let mut st = ExecutionState::new(json!({}));
        st.vars.insert("counter".into(), json!(3));
        st.vars.insert("region".into(), json!("us-east-1"));

        let encoded = serde_json::to_string(&st).expect("serialize");
        let decoded: ExecutionState = serde_json::from_str(&encoded).expect("deserialize");

        assert_eq!(decoded.vars.get("counter"), Some(&json!(3)));
        assert_eq!(decoded.vars.get("region"), Some(&json!("us-east-1")));
    }

    #[test]
    fn execution_state_vars_default_empty_for_old_snapshots() {
        // A snapshot serialized before `vars` existed (no `vars` key) must still load.
        let legacy = r#"{"entry":{},"input":{},"nodes":{},"egress":[],"redirect_count":0}"#;
        let decoded: ExecutionState = serde_json::from_str(legacy).expect("legacy loads");
        assert!(decoded.vars.is_empty());
    }

    #[test]
    fn template_context_exposes_vars_namespace_typed() {
        let mut st = ExecutionState::new(serde_json::json!({}));
        st.vars.insert("count".into(), serde_json::json!(5));
        st.vars.insert("name".into(), serde_json::json!("aws"));

        let ctx = template_context(&st, serde_json::Value::Null);
        // {{vars.count}} must resolve to the JSON number 5, not the string "5".
        let rendered_num = render_template_value(
            &serde_json::json!("{{vars.count}}"),
            &ctx,
            TemplateOptions::default(),
        )
        .expect("render num");
        assert_eq!(rendered_num, serde_json::json!(5));

        let rendered_str = render_template_value(
            &serde_json::json!("prefix-{{vars.name}}"),
            &ctx,
            TemplateOptions::default(),
        )
        .expect("render str");
        assert_eq!(rendered_str, serde_json::json!("prefix-aws"));
    }

    #[test]
    fn vars_namespace_does_not_shadow_existing_namespaces() {
        let st = ExecutionState::new(serde_json::json!({"user": {"id": 7}}));
        let ctx = template_context(&st, serde_json::Value::Null);
        let obj = ctx.as_object().expect("ctx object");
        for key in ["entry", "in", "prev", "node", "state", "vars"] {
            assert!(obj.contains_key(key), "context must expose `{key}`");
        }
    }

    // ── vars_init tests ────────────────────────────────────────────────────

    /// Build a minimal flow with the given free-form `metadata.extra` value.
    /// Mirrors the construction used by neighbouring engine tests: a schema-1.0
    /// Messaging flow with no nodes and no entrypoints, only the metadata set.
    fn flow_with_extra(extra: serde_json::Value) -> Flow {
        Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("test.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::new(),
            nodes: indexmap::IndexMap::default(),
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra,
            },
        }
    }

    #[test]
    fn seed_vars_seeds_defaults_and_reports_missing_required() {
        use serde_json::json;
        let mut vars_init = JsonMap::new();
        vars_init.insert("region".into(), json!("us-east-1"));

        // "name" is required but has no default; "region" required WITH a default.
        let required = vec!["name".to_string(), "region".to_string()];
        let mut target = JsonMap::new();

        let missing = seed_vars_and_collect_missing_required(&vars_init, &required, &mut target);

        assert_eq!(
            target.get("region"),
            Some(&json!("us-east-1")),
            "default seeded"
        );
        assert_eq!(
            missing,
            vec!["name".to_string()],
            "only the defaultless required var is missing"
        );
    }

    #[test]
    fn seed_vars_respects_preexisting_value_for_required() {
        use serde_json::json;
        let vars_init = JsonMap::new(); // no defaults declared
        let required = vec!["name".to_string()];
        let mut target = JsonMap::new();
        target.insert("name".into(), json!("Budi")); // operator-provided value already present

        let missing = seed_vars_and_collect_missing_required(&vars_init, &required, &mut target);

        assert!(
            missing.is_empty(),
            "a required var with a provided value is not missing"
        );
        assert_eq!(
            target.get("name"),
            Some(&json!("Budi")),
            "provided value not overwritten"
        );
    }

    #[test]
    fn from_flow_extracts_vars_init() {
        let flow = flow_with_extra(serde_json::json!({
            "vars_init": {
                "region":  { "type": "string", "default": "us-east-1" },
                "counter": { "type": "number", "default": 0 }
            }
        }));
        let host: HostFlow = HostFlow::from(flow);
        assert_eq!(
            host.vars_init.get("region"),
            Some(&serde_json::json!("us-east-1"))
        );
        assert_eq!(host.vars_init.get("counter"), Some(&serde_json::json!(0)));
    }

    #[test]
    fn from_flow_vars_init_absent() {
        let flow = flow_with_extra(serde_json::json!({}));
        let host: HostFlow = HostFlow::from(flow);
        assert!(host.vars_init.is_empty());
    }

    #[test]
    fn from_flow_collects_required_vars() {
        let flow = flow_with_extra(serde_json::json!({
            "vars_init": {
                "name":   { "type": "string", "required": true },
                "region": { "type": "string", "default": "us-east-1" },
                "note":   { "type": "string", "required": false }
            }
        }));
        let host: HostFlow = HostFlow::from(flow);
        assert_eq!(host.required_vars, vec!["name".to_string()]);
    }

    #[test]
    fn execute_once_fails_on_missing_required_var() {
        let node_id = NodeId::from_str("n1").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "message": "{{vars.region}}" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("vars.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra: json!({
                    "vars_init": {
                        "region": { "type": "string", "required": true }
                    }
                }),
            },
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "vars.flow".to_string(),
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
            operala_node_handler: None,
        };

        let observer = CountingObserver::new();
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "vars.flow",
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
        let err = rt
            .block_on(engine.execute(ctx, Value::Null))
            .expect_err("a required var with no default and no value must fail the run");
        let msg = err.to_string();
        assert!(msg.contains("region"), "error names the missing var: {msg}");
        assert!(
            !should_retry(&err),
            "missing-required-var is deterministic and must not be retried"
        );
        assert!(
            observer.ends.lock().unwrap().is_empty(),
            "flow aborted before the node ran"
        );
    }

    #[test]
    fn execute_once_seeds_declared_vars() {
        // A flow with vars_init seeds state.vars before the first node runs.
        // We verify this by using an emit.log node whose message template
        // references {{vars.region}}: if the var is seeded, the rendered
        // output will contain "us-east-1".
        let node_id = NodeId::from_str("n1").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "message": "{{vars.region}}" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };
        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(node_id.clone(), node);
        let flow = Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("vars.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(node_id.to_string()),
            )]),
            nodes,
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra: json!({
                    "vars_init": {
                        "region": { "type": "string", "default": "us-east-1" }
                    }
                }),
            },
        };
        let host_flow = HostFlow::from(flow);

        let engine = FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: HashMap::new(),
            flow_cache: RwLock::new(HashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "vars.flow".to_string(),
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
            operala_node_handler: None,
        };

        let observer = CountingObserver::new();
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "vars.flow",
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

        let ends = observer.ends.lock().unwrap();
        assert_eq!(ends.len(), 1);
        assert_eq!(
            ends[0].get("message").and_then(Value::as_str),
            Some("us-east-1"),
            "vars.region must be seeded to its default and rendered in the node payload"
        );
    }

    // ── var_set tests ──────────────────────────────────────────────────────

    /// Build a two-node flow: var_set → emit.log, with optional vars_init.
    ///
    /// `var_set_input` is the raw input mapping for the var.set node,
    /// e.g. `json!({ "name": "greeting", "value": "hi" })`.
    /// `emit_input` is the input mapping for the emit.log node.
    /// `vars_init_extra` is optional flow-level vars_init metadata.
    fn var_set_flow(
        var_set_input: Value,
        emit_input: Value,
        vars_init_extra: Option<Value>,
    ) -> Flow {
        let set_id = NodeId::from_str("set1").unwrap();
        let emit_id = NodeId::from_str("emit1").unwrap();

        let set_node = Node {
            id: set_id.clone(),
            component: FlowComponentRef {
                id: "var.set".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: var_set_input,
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: emit_id.clone(),
            },
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let emit_node = Node {
            id: emit_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: emit_input,
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(set_id.clone(), set_node);
        nodes.insert(emit_id.clone(), emit_node);

        let extra = vars_init_extra.unwrap_or(serde_json::json!({}));

        Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("var.set.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(set_id.to_string()),
            )]),
            nodes,
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra,
            },
        }
    }

    fn run_var_set_flow(flow: Flow) -> (FlowStatus, Vec<Value>) {
        let host_flow = HostFlow::from(flow);
        let engine = FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "var.set.flow".to_string(),
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
            operala_node_handler: None,
        };
        let observer = CountingObserver::new();
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "var.set.flow",
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
        let ends = observer.ends.lock().unwrap().clone();
        (result.status, ends)
    }

    #[test]
    fn var_set_node_writes_literal_value_into_vars() {
        // A var_set node with a literal value: greeting="hi".
        // The following emit.log node uses {{vars.greeting}} and its output
        // proves the var was written.
        let flow = var_set_flow(
            json!({ "name": "greeting", "value": "hi" }),
            json!({ "message": "{{vars.greeting}}" }),
            None,
        );
        let (status, ends) = run_var_set_flow(flow);

        assert!(
            matches!(status, FlowStatus::Completed),
            "flow must complete"
        );
        assert_eq!(ends.len(), 2, "both nodes must fire");
        // var_set node output
        assert_eq!(ends[0].get("ok"), Some(&json!(true)), "var_set output ok");
        // emit.log node output: vars.greeting was written
        assert_eq!(
            ends[1].get("message").and_then(Value::as_str),
            Some("hi"),
            "vars.greeting must be written and renderable in the next node"
        );
    }

    #[test]
    fn var_set_node_writes_templated_value_with_type_preserved() {
        // vars_init seeds counter=1 (a number).
        // var_set copies it into "copy" via {{vars.counter}}.
        // The emit.log node uses {{vars.copy}} as the sole message template;
        // render_template_value returns the typed JSON number, not a string.
        let flow = var_set_flow(
            json!({ "name": "copy", "value": "{{vars.counter}}" }),
            json!({ "message": "{{vars.copy}}" }),
            Some(json!({
                "vars_init": {
                    "counter": { "type": "number", "default": 1 }
                }
            })),
        );
        let (status, ends) = run_var_set_flow(flow);

        assert!(
            matches!(status, FlowStatus::Completed),
            "flow must complete"
        );
        assert_eq!(ends.len(), 2, "both nodes must fire");
        // emit.log message must be the typed number 1, not the string "1"
        assert_eq!(
            ends[1].get("message"),
            Some(&json!(1)),
            "vars.copy must preserve the JSON number type from vars.counter"
        );
    }

    #[test]
    fn var_set_empty_name_is_skipped_not_written() {
        // A var_set node with an empty (or whitespace-only) name must complete
        // without panic and must NOT insert a "" key into state.vars.
        let engine = minimal_engine();
        let rt = Runtime::new().unwrap();
        let retry_config = RetryConfig {
            max_attempts: 1,
            base_delay_ms: 1,
        };
        let ctx = FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "var.set.flow",
            node_id: Some("set1"),
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
            kind: NodeKind::VarSet {
                name: "".to_string(),
                value: json!("garbage"),
            },
            component: "var.set".into(),
            component_id: "var.set".into(),
            operation_name: None,
            operation_in_mapping: None,
            payload_expr: Value::Null,
            routing: Routing::End,
            vars_out: None,
        };
        let mut state = ExecutionState::new(Value::Null);
        let payload = Value::Null;
        let event = NodeEvent {
            context: &ctx,
            node_id: "set1",
            node: &node,
            payload: &payload,
        };

        let outcome = rt
            .block_on(engine.dispatch_node(
                &ctx,
                "set1",
                &node,
                &mut state,
                payload.clone(),
                &event,
            ))
            .expect("dispatch_node must not error on empty var name");

        // Must return {ok: true} (not an error).
        assert_eq!(
            outcome.output.payload,
            json!({ "ok": true }),
            "dispatch must return ok:true even when name is empty"
        );
        // Must NOT have inserted a \"\" key into state.vars.
        assert!(
            state.vars.get("").is_none(),
            "empty var name must not create a \"\" key in state.vars"
        );
    }

    #[test]
    fn var_set_node_has_empty_payload_expr() {
        // Lowering a var.set Node must yield a HostNode whose payload_expr is
        // Value::Null. The VarSet dispatch arm reads name/value directly from
        // NodeKind::VarSet, so forwarding the mapping as payload_expr is redundant.
        let node_id = NodeId::from_str("set1").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "var.set".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "name": "greeting", "value": "hi" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let host_node = HostNode::from(node);

        // payload_expr must be Null.
        assert_eq!(
            host_node.payload_expr,
            Value::Null,
            "var.set node must have Null payload_expr after lowering"
        );
        // NodeKind::VarSet must still carry the original name and value.
        match &host_node.kind {
            NodeKind::VarSet { name, value } => {
                assert_eq!(name.as_str(), "greeting", "name must be preserved in kind");
                assert_eq!(value, &json!("hi"), "value must be preserved in kind");
            }
            other => panic!("expected NodeKind::VarSet, got {other:?}"),
        }
    }

    // ── vars_out tests ──────────────────────────────────────────────────────

    /// Build a two-node flow: emit.log (with vars_out) → emit.log.
    ///
    /// `emit1_input` is the raw input mapping for the first emit.log node
    /// (should include the `vars_out` binding).
    /// `emit2_input` is the input mapping for the second emit.log node
    /// (reads from `vars.*` to prove bindings were applied).
    fn vars_out_flow(emit1_input: Value, emit2_input: Value) -> Flow {
        let emit1_id = NodeId::from_str("emit1").unwrap();
        let emit2_id = NodeId::from_str("emit2").unwrap();

        let emit1_node = Node {
            id: emit1_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: emit1_input,
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: emit2_id.clone(),
            },
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let emit2_node = Node {
            id: emit2_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: emit2_input,
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(emit1_id.clone(), emit1_node);
        nodes.insert(emit2_id.clone(), emit2_node);

        // Reuse the same flow_id as var_set_flow so we can pass it directly to
        // `run_var_set_flow`, which registers the flow under that key.
        Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("var.set.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(emit1_id.to_string()),
            )]),
            nodes,
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn vars_out_binds_node_output_into_vars() {
        // `emit.log` outputs its rendered payload directly. Node 1 emits
        // `{ message: "hello" }` and declares `vars_out = { lastReply:
        // "{{prev.message}}" }`. After it runs, `state.vars["lastReply"]`
        // must equal "hello". Node 2 reads that var so the assertion is driven
        // from the second node's output rather than internal state.
        let flow = vars_out_flow(
            json!({
                "message": "hello",
                "vars_out": { "lastReply": "{{prev.message}}" }
            }),
            json!({ "message": "{{vars.lastReply}}" }),
        );
        let (status, ends) = run_var_set_flow(flow);

        assert!(
            matches!(status, FlowStatus::Completed),
            "flow must complete"
        );
        assert_eq!(ends.len(), 2, "both nodes must fire");
        // Node 2's message must equal the value captured by vars_out in node 1.
        assert_eq!(
            ends[1].get("message").and_then(Value::as_str),
            Some("hello"),
            "vars_out binding from node 1 must be readable in node 2"
        );
    }

    #[test]
    fn vars_out_is_stripped_from_component_node_payload() {
        // A component (non-emit) node whose input.mapping contains both a
        // real field ("message") and the internal "vars_out" meta-key must NOT
        // forward "vars_out" as part of its payload_expr. This prevents the key
        // from leaking into wasm components with strict additionalProperties schemas.
        //
        // We use a PackComponent-style node (component ref = "my.component") so
        // the non-emit arm of `impl From<Node> for HostNode` is exercised.
        let node_id = NodeId::from_str("comp1").unwrap();
        let node = Node {
            id: node_id.clone(),
            component: FlowComponentRef {
                id: "my.component".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({
                    "message": "hello",
                    "vars_out": { "lastReply": "{{prev.message}}" }
                }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let host_node = HostNode::from(node);

        // The vars_out binding must be preserved on the HostNode itself.
        assert!(
            host_node.vars_out.is_some(),
            "vars_out binding must be carried on the HostNode"
        );
        assert!(
            host_node
                .vars_out
                .as_ref()
                .unwrap()
                .contains_key("lastReply"),
            "vars_out must contain the declared binding"
        );

        // The payload_expr must NOT contain the "vars_out" key.
        assert!(
            host_node.payload_expr.get("vars_out").is_none(),
            "vars_out must not appear in payload_expr (would leak to wasm component input)"
        );

        // The real input field must still be present in the payload_expr.
        assert_eq!(
            host_node
                .payload_expr
                .get("message")
                .and_then(Value::as_str),
            Some("hello"),
            "real input fields must remain in payload_expr"
        );
    }

    #[test]
    fn dotted_component_id_is_not_split_when_mapping_carries_operation() {
        // packc sets `node.component.operation = None` to mean "the id is already
        // complete" (normalize_legacy_component_exec_ids), and puts the operation in
        // input.mapping. Lowering must therefore keep the reverse-DNS id intact and
        // take the operation from the mapping. Splitting on the last dot yields a
        // component_ref that is by construction absent from the pack's component map,
        // so the node fails with "component '<truncated>' not found in pack".
        let node_id = NodeId::from_str("present_koncar").unwrap();
        let node = Node {
            id: node_id,
            component: FlowComponentRef {
                id: "ai.greentic.koncar.component-present".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({
                    "component": "ai.greentic.koncar.component-present",
                    "operation": "present",
                    "query": "hello"
                }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let host_node = HostNode::from(node);

        assert_eq!(
            host_node.component_id(),
            "ai.greentic.koncar.component-present",
            "dotted component id must survive lowering intact"
        );
        match &host_node.kind {
            NodeKind::PackComponent { component_ref } => assert_eq!(
                component_ref, "ai.greentic.koncar.component-present",
                "PackComponent must look up the full id, not a dot-split prefix"
            ),
            other => panic!("expected NodeKind::PackComponent, got {other:?}"),
        }
        assert_eq!(
            host_node.operation_in_mapping(),
            Some("present"),
            "the operation must still be readable from the mapping"
        );
    }

    /// Build a three-node flow: var_set → session.wait → emit.log.
    /// `vars_init` seeds `counter = 1`; `var_set` writes `greeting = "hello"`.
    /// The wait parks the flow; resume runs emit.log which reads both vars.
    fn vars_survive_flow() -> Flow {
        let set_id = NodeId::from_str("set1").unwrap();
        let wait_id = NodeId::from_str("wait1").unwrap();
        let emit_id = NodeId::from_str("emit1").unwrap();

        let set_node = Node {
            id: set_id.clone(),
            component: FlowComponentRef {
                id: "var.set".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({ "name": "greeting", "value": "hello" }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: wait_id.clone(),
            },
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let wait_node = Node {
            id: wait_id.clone(),
            component: FlowComponentRef {
                id: "session.wait".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: Value::Null,
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::Next {
                node_id: emit_id.clone(),
            },
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let emit_node = Node {
            id: emit_id.clone(),
            component: FlowComponentRef {
                id: "emit.log".parse().unwrap(),
                pack_alias: None,
                operation: None,
            },
            input: InputMapping {
                mapping: json!({
                    "greeting": "{{vars.greeting}}",
                    "counter": "{{vars.counter}}"
                }),
            },
            output: OutputMapping {
                mapping: Value::Null,
            },
            err_map: None,
            routing: Routing::End,
            telemetry: TelemetryHints::default(),
            conversational: false,
        };

        let mut nodes = indexmap::IndexMap::default();
        nodes.insert(set_id.clone(), set_node);
        nodes.insert(wait_id.clone(), wait_node);
        nodes.insert(emit_id.clone(), emit_node);

        Flow {
            schema_version: "1.0".into(),
            id: FlowId::from_str("vars.survive.flow").unwrap(),
            kind: FlowKind::Messaging,
            entrypoints: BTreeMap::from([(
                "default".to_string(),
                Value::String(set_id.to_string()),
            )]),
            nodes,
            metadata: FlowMetadata {
                title: None,
                description: None,
                tags: Default::default(),
                extra: json!({
                    "vars_init": {
                        "counter": { "type": "number", "default": 1 }
                    }
                }),
            },
        }
    }

    #[test]
    fn vars_survive_park_and_resume_end_to_end() {
        // vars_init seeds counter=1; var_set writes greeting="hello"; the flow
        // parks at session.wait; resume drives emit.log which reads both vars.
        let flow = vars_survive_flow();
        let host_flow = HostFlow::from(flow);
        let flow_id = "vars.survive.flow";
        let pack_id = "test-pack";
        let engine = FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: pack_id.to_string(),
                    flow_id: flow_id.to_string(),
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
            operala_node_handler: None,
        };
        let rt = Runtime::new().unwrap();

        // First execution: must park at session.wait after var_set fires.
        let ctx1 = FlowContext {
            tenant: "demo",
            pack_id,
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
        };
        let result1 = rt.block_on(engine.execute(ctx1, Value::Null)).unwrap();
        let snapshot = match result1.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after session.wait, got {other:?}"),
        };

        // Both vars must be present in the snapshot before resume.
        assert_eq!(
            snapshot.state.vars.get("greeting"),
            Some(&json!("hello")),
            "greeting var must be in snapshot"
        );
        assert_eq!(
            snapshot.state.vars.get("counter"),
            Some(&json!(1)),
            "counter var (from vars_init) must be in snapshot"
        );

        // Resume: emit.log must read both vars from the restored state.
        let observer2 = CountingObserver::new();
        let ctx2 = FlowContext {
            tenant: "demo",
            pack_id,
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
            observer: Some(&observer2),
            mocks: None,
        };
        let result2 = rt
            .block_on(engine.resume(ctx2, snapshot, Value::Null))
            .unwrap();
        assert!(
            matches!(result2.status, FlowStatus::Completed),
            "flow must complete after resume"
        );
        let ends2 = observer2.ends.lock().unwrap().clone();
        assert_eq!(ends2.len(), 1, "only emit.log fires after resume");
        assert_eq!(
            ends2[0].get("greeting").and_then(Value::as_str),
            Some("hello"),
            "vars.greeting must survive the park/resume"
        );
        assert_eq!(
            ends2[0].get("counter"),
            Some(&json!(1)),
            "vars.counter (vars_init) must survive the park/resume"
        );
    }

    #[cfg(feature = "agentic-worker")]
    struct StubAgentHandler {
        payload: serde_json::Value,
    }
    #[cfg(feature = "agentic-worker")]
    #[async_trait::async_trait]
    impl crate::runner::agent_node::AgentNodeHandler for StubAgentHandler {
        async fn execute(
            &self,
            _tenant_id: &str,
            _env_id: &str,
            _agent_id: &str,
            _session_id: &str,
            _flow_input: &serde_json::Value,
            _conversational: bool,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(self.payload.clone())
        }
    }

    /// Build a 2-node flow: a `dw.agent` node (id "agent", conversational as
    /// given) routing to an emit "thanks" node that ends the flow.
    #[cfg(feature = "agentic-worker")]
    fn conversational_dw_flow(conversational: bool) -> HostFlow {
        let mut nodes = IndexMap::new();
        let agent_id = NodeId::from_str("agent").unwrap();
        let thanks_id = NodeId::from_str("thanks").unwrap();
        nodes.insert(
            agent_id.clone(),
            HostNode {
                kind: NodeKind::DwAgent {
                    agent_id: "a".to_string(),
                    conversational,
                },
                component: "dw.agent".to_string(),
                component_id: "dw.agent".to_string(),
                operation_name: Some("a".to_string()),
                operation_in_mapping: None,
                payload_expr: json!({ "user_text": "hi" }),
                routing: Routing::Next {
                    node_id: thanks_id.clone(),
                },
                vars_out: None,
            },
        );
        nodes.insert(
            thanks_id.clone(),
            HostNode {
                kind: NodeKind::BuiltinEmit {
                    kind: EmitKind::Response,
                },
                component: "emit.response".to_string(),
                component_id: "emit.response".to_string(),
                operation_name: None,
                operation_in_mapping: None,
                payload_expr: json!({ "text": "thanks" }),
                routing: Routing::End,
                vars_out: None,
            },
        );
        HostFlow {
            slot_schema: None,
            id: "conv.flow".to_string(),
            start: Some(agent_id),
            nodes,
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
        }
    }

    /// Build an engine holding `flow` with a stub agent handler returning `payload`.
    /// Mirrors the FlowEngine literal in `vars_survive_park_and_resume_end_to_end`.
    #[cfg(feature = "agentic-worker")]
    fn conv_engine(flow: HostFlow, payload: serde_json::Value) -> FlowEngine {
        FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "conv.flow".to_string(),
                },
                flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            agent_node_handler: Some(std::sync::Arc::new(StubAgentHandler { payload })),
            graph_node_handler: None,
            mcp_tool_source: None,
            operala_node_handler: None,
        }
    }

    #[cfg(feature = "agentic-worker")]
    fn conv_ctx<'a>() -> FlowContext<'a> {
        FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "conv.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("sess-conv"),
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

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_parks_and_loops_on_normal_reply() {
        let engine = conv_engine(
            conversational_dw_flow(true),
            json!({ "reply": "hello there", "trail": [], "terminated_by": "final_reply" }),
        );
        let rt = Runtime::new().unwrap();
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting (park-loop), got {other:?}"),
        };
        assert_eq!(
            snapshot.next_node, "agent",
            "must re-enter the dw.agent node itself"
        );
        // The reply is rendered in the parked output.
        assert!(
            serde_json::to_string(&result.output)
                .unwrap()
                .contains("hello there"),
            "the agent reply must be rendered before parking: {:?}",
            result.output
        );
    }

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_advances_on_conversation_ended() {
        let engine = conv_engine(
            conversational_dw_flow(true),
            json!({ "reply": "bye", "trail": [], "terminated_by": "conversation_ended" }),
        );
        let rt = Runtime::new().unwrap();
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        assert!(
            matches!(result.status, FlowStatus::Completed),
            "conversation_ended must advance to the successor and complete, got {:?}",
            result.status
        );
    }

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn non_conversational_dw_agent_never_loops() {
        // Even with terminated_by == conversation_ended, a non-conversational
        // node just routes onward (today's one-shot behaviour) — never parks.
        for tb in ["final_reply", "conversation_ended"] {
            let engine = conv_engine(
                conversational_dw_flow(false),
                json!({ "reply": "x", "trail": [], "terminated_by": tb }),
            );
            let rt = Runtime::new().unwrap();
            let result = rt
                .block_on(engine.execute(conv_ctx(), Value::Null))
                .unwrap();
            assert!(
                matches!(result.status, FlowStatus::Completed),
                "non-conversational must complete (route onward) for terminated_by={tb}, got {:?}",
                result.status
            );
        }
    }

    /// Safety-backstop behavioral test: a conversational `dw.agent` that
    /// never emits `conversation_ended` must keep parking up to
    /// `MAX_PARK_TURNS` turns, then force-advance to the successor instead
    /// of trapping the flow forever.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_force_advances_after_park_loop_cap() {
        let engine = conv_engine(
            conversational_dw_flow(true),
            json!({ "reply": "still thinking", "trail": [], "terminated_by": "final_reply" }),
        );
        let rt = Runtime::new().unwrap();

        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let mut snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after turn 1, got {other:?}"),
        };

        // Turns 2..MAX_PARK_TURNS (exclusive) must keep parking.
        for turn in 2..MAX_PARK_TURNS {
            let result = rt
                .block_on(engine.resume(conv_ctx(), snapshot, json!({ "text": "still here" })))
                .unwrap();
            snapshot = match result.status {
                FlowStatus::Waiting(w) => w.snapshot,
                other => panic!("expected Waiting at turn {turn}, got {other:?}"),
            };
        }

        // The MAX_PARK_TURNS-th turn must force-advance instead of parking again.
        let result = rt
            .block_on(engine.resume(conv_ctx(), snapshot, json!({ "text": "still here" })))
            .unwrap();
        assert!(
            matches!(result.status, FlowStatus::Completed),
            "park-loop cap must force-advance to the successor at turn {MAX_PARK_TURNS}, got {:?}",
            result.status
        );
    }

    // ── NATS conversational `dw.agent` park-loop (Task 6) ──────────────────
    //
    // These tests drive the SAME `conversational_dw_flow`/`conv_ctx` harness as
    // the in-process tests above, but with `DwAgentDispatch::Nats` and a stub
    // `RemoteDispatchHandler` that never touches a live NATS server — it just
    // records the dispatch and immediately returns `AwaitingResponse`, exactly
    // like `dw_agent_nats_mode_dispatches_remote` above. The "NATS response
    // arriving" half of the round trip is simulated by calling `engine.resume`
    // directly with a hand-built envelope `{ok, output, events, error}` — the
    // exact shape `dispatch_listener::decode_response` builds and that lands in
    // `state.entry` on a real resume (spike finding §Q2). No live NATS server is
    // needed or used.

    /// Records every dispatch and immediately returns `AwaitingResponse`, so the
    /// engine parks without a live NATS server. Mirrors `RecordingDispatcher` in
    /// `dw_agent_nats_mode_dispatches_remote`, kept separate (and named for
    /// re-use across the tests below) since three tests share it.
    #[cfg(feature = "agentic-worker")]
    struct ScriptedNatsDispatcher {
        calls: Mutex<Vec<crate::runner::remote_dispatch::RemoteDispatch>>,
    }

    #[cfg(feature = "agentic-worker")]
    #[async_trait::async_trait]
    impl crate::runner::remote_dispatch::RemoteDispatchHandler for ScriptedNatsDispatcher {
        async fn dispatch(
            &self,
            request: crate::runner::remote_dispatch::RemoteDispatch,
        ) -> anyhow::Result<crate::runner::remote_dispatch::RemoteDispatchAction> {
            let correlation_id = request.correlation_id.clone();
            self.calls.lock().unwrap().push(request);
            Ok(
                crate::runner::remote_dispatch::RemoteDispatchAction::AwaitingResponse {
                    correlation_id,
                },
            )
        }
    }

    /// Build an engine holding `flow` in `DwAgentDispatch::Nats` mode, wired to
    /// `dispatcher`. Mirrors `conv_engine` (the in-process counterpart) so the
    /// two harnesses are structurally comparable.
    #[cfg(feature = "agentic-worker")]
    fn nats_conv_engine(
        flow: HostFlow,
        dispatcher: std::sync::Arc<dyn crate::runner::remote_dispatch::RemoteDispatchHandler>,
    ) -> FlowEngine {
        FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "conv.flow".to_string(),
                },
                flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: Some(dispatcher),
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::Nats,
            agent_node_handler: None,
            graph_node_handler: None,
            mcp_tool_source: None,
            operala_node_handler: None,
        }
    }

    /// Build the envelope a real NATS response resume lands in `state.entry`,
    /// per spike finding §Q2: `{ok, output: {reply, trail, terminated_by},
    /// events, error}` (mirrors `dispatch_listener::decode_response`).
    #[cfg(feature = "agentic-worker")]
    fn agent_response_envelope(reply: &str, terminated_by: &str) -> Value {
        json!({
            "ok": true,
            "output": { "reply": reply, "trail": [], "terminated_by": terminated_by },
            "events": [],
            "error": Value::Null,
        })
    }

    /// Turn 1 (fresh, no prior await marker): the conversational Nats arm must
    /// mark the pending await, dispatch to NATS exactly once, and park via
    /// `NodeControl::AwaitHere` — resuming at the node itself (not the routing
    /// successor) with no reply surfaced yet (the response hasn't arrived).
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_turn1_parks_via_await_here() {
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let engine = nats_conv_engine(conversational_dw_flow(true), dispatcher.clone());
        let rt = Runtime::new().unwrap();

        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after fresh dispatch, got {other:?}"),
        };
        assert_eq!(
            snapshot.next_node, "agent",
            "AwaitHere must resume at self, not the routing successor"
        );
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            1,
            "a fresh user turn must dispatch to NATS exactly once"
        );
        assert_eq!(
            result.output,
            Value::Null,
            "no reply is known yet on the initial dispatch — the async response hasn't arrived"
        );
    }

    /// Full turn cycle, behavioral: fresh dispatch → AwaitHere park; simulated
    /// "not ended" NATS response resume → LoopHere park (reply surfaced,
    /// session-keyed park awaiting the next user message); a user-reply resume
    /// dispatches to NATS again; a `conversation_ended` response resume →
    /// Completed (advanced to the successor). This is the exact turn-by-turn
    /// script called for in Task 6's brief.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_park_loop_full_turn_cycle() {
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let engine = nats_conv_engine(conversational_dw_flow(true), dispatcher.clone());
        let rt = Runtime::new().unwrap();

        // Turn 1: fresh user turn → dispatch to NATS → AwaitHere (self, park).
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting (AwaitHere) after turn 1 dispatch, got {other:?}"),
        };
        assert_eq!(snapshot.next_node, "agent");
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1);

        // Simulated NATS response resume, "not ended": LoopHere (session-keyed
        // park awaiting the next user message), reply surfaced.
        let result = rt
            .block_on(engine.resume(
                conv_ctx(),
                snapshot,
                agent_response_envelope("hello there", "final_reply"),
            ))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting (LoopHere) after not-ended response, got {other:?}"),
        };
        assert_eq!(
            snapshot.next_node, "agent",
            "LoopHere also re-enters the node itself"
        );
        assert!(
            serde_json::to_string(&result.output)
                .unwrap()
                .contains("hello there"),
            "the agent's reply must be surfaced once the response resume lands: {:?}",
            result.output
        );
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            1,
            "the response landing must not itself trigger another NATS dispatch"
        );

        // User-reply resume: a fresh user turn dispatches to NATS again.
        let result = rt
            .block_on(engine.resume(conv_ctx(), snapshot, json!({ "text": "user says more" })))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting (AwaitHere) after turn 2 dispatch, got {other:?}"),
        };
        assert_eq!(snapshot.next_node, "agent");
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            2,
            "a second fresh user turn must dispatch to NATS again"
        );

        // Simulated NATS response resume, `conversation_ended`: advance to the
        // successor and complete.
        let result = rt
            .block_on(engine.resume(
                conv_ctx(),
                snapshot,
                agent_response_envelope("bye", "conversation_ended"),
            ))
            .unwrap();
        assert!(
            matches!(result.status, FlowStatus::Completed),
            "conversation_ended response must advance to the successor and complete, got {:?}",
            result.status
        );
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            2,
            "conversation end must not trigger another NATS dispatch"
        );
    }

    /// Safety-backstop parity with the in-process cap test: a NATS
    /// conversational `dw.agent` whose response never carries
    /// `conversation_ended` must keep parking (dispatch → AwaitHere →
    /// response-resume → LoopHere) up to `MAX_PARK_TURNS` "not ended" responses,
    /// then force-advance to the successor instead of trapping the flow.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_force_advances_after_park_loop_cap() {
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let engine = nats_conv_engine(conversational_dw_flow(true), dispatcher.clone());
        let rt = Runtime::new().unwrap();

        // Turn 1: fresh dispatch (does not itself count toward the park cap —
        // the cap is bumped only on a "not ended" response, matching the
        // in-process semantics).
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let mut snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after turn 1 dispatch, got {other:?}"),
        };

        // Responses 1..MAX_PARK_TURNS (exclusive) must keep looping: a "not
        // ended" response resume (LoopHere), then a user-message resume that
        // re-dispatches to NATS (AwaitHere) for the next response.
        for turn in 1..MAX_PARK_TURNS {
            let result = rt
                .block_on(engine.resume(
                    conv_ctx(),
                    snapshot,
                    agent_response_envelope("still thinking", "final_reply"),
                ))
                .unwrap();
            snapshot = match result.status {
                FlowStatus::Waiting(w) => w.snapshot,
                other => panic!("expected Waiting (LoopHere) at response #{turn}, got {other:?}"),
            };
            let result = rt
                .block_on(engine.resume(conv_ctx(), snapshot, json!({ "text": "still here" })))
                .unwrap();
            snapshot = match result.status {
                FlowStatus::Waiting(w) => w.snapshot,
                other => {
                    panic!("expected Waiting (AwaitHere) after user turn #{turn}, got {other:?}")
                }
            };
        }

        // The MAX_PARK_TURNS-th "not ended" response must force-advance instead
        // of parking again.
        let result = rt
            .block_on(engine.resume(
                conv_ctx(),
                snapshot,
                agent_response_envelope("still thinking", "final_reply"),
            ))
            .unwrap();
        assert!(
            matches!(result.status, FlowStatus::Completed),
            "park-loop cap must force-advance to the successor at response {MAX_PARK_TURNS}, got {:?}",
            result.status
        );
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            1 + (MAX_PARK_TURNS as usize - 1),
            "exactly one NATS dispatch per user turn across the whole park-loop"
        );
    }

    /// Parity: for the same scripted two-turn conversation (turn 1 replies
    /// "hello there", not ended; turn 2 replies "bye", `conversation_ended`),
    /// the NATS and in-process dispatch paths must be *observationally*
    /// identical — same sequence of user-visible statuses, and the same
    /// surfaced reply text on the parked turn.
    ///
    /// Caveat (documented, not hidden): the NATS path has one extra *internal*
    /// resume between user turns — the async response landing (AwaitHere →
    /// LoopHere) — that the in-process path does synchronously inside a single
    /// `execute`/`resume` call. That extra step is invisible to the flow's
    /// outward status/reply, which is exactly what this test asserts; it does
    /// NOT assert the two paths take the same number of `resume` calls.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_and_inprocess_transcripts_match_for_same_script() {
        let rt = Runtime::new().unwrap();

        // ── In-process transcript ──
        let inproc_handler = Arc::new(ScriptedAgentHandler {
            script: Mutex::new(std::collections::VecDeque::from(vec![
                json!({ "reply": "hello there", "trail": [], "terminated_by": "final_reply" }),
                json!({ "reply": "bye", "trail": [], "terminated_by": "conversation_ended" }),
            ])),
        });
        let inproc_engine = conv_engine_scripted(conversational_dw_flow(true), inproc_handler);
        let r1 = rt
            .block_on(inproc_engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let inproc_snapshot = match r1.status {
            FlowStatus::Waiting(ref w) => w.snapshot.clone(),
            ref other => panic!("in-process turn 1: expected Waiting, got {other:?}"),
        };
        let r2 = rt
            .block_on(inproc_engine.resume(conv_ctx(), inproc_snapshot, json!({ "text": "more" })))
            .unwrap();

        // ── NATS transcript, same script ──
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let nats_engine = nats_conv_engine(conversational_dw_flow(true), dispatcher);
        let n1 = rt
            .block_on(nats_engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let n1_snapshot = match n1.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("nats turn 1 dispatch: expected Waiting, got {other:?}"),
        };
        let n1r = rt
            .block_on(nats_engine.resume(
                conv_ctx(),
                n1_snapshot,
                agent_response_envelope("hello there", "final_reply"),
            ))
            .unwrap();
        let n1r_snapshot = match n1r.status {
            FlowStatus::Waiting(ref w) => w.snapshot.clone(),
            ref other => panic!("nats turn 1 response resume: expected Waiting, got {other:?}"),
        };
        let n2 = rt
            .block_on(nats_engine.resume(conv_ctx(), n1r_snapshot, json!({ "text": "more" })))
            .unwrap();
        let n2_snapshot = match n2.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("nats turn 2 dispatch: expected Waiting, got {other:?}"),
        };
        let n2r = rt
            .block_on(nats_engine.resume(
                conv_ctx(),
                n2_snapshot,
                agent_response_envelope("bye", "conversation_ended"),
            ))
            .unwrap();

        // Same user-visible status per turn.
        assert!(matches!(r1.status, FlowStatus::Waiting(_)));
        assert!(
            matches!(n1r.status, FlowStatus::Waiting(_)),
            "nats turn 1's user-visible status must also be Waiting"
        );
        assert!(matches!(r2.status, FlowStatus::Completed));
        assert!(
            matches!(n2r.status, FlowStatus::Completed),
            "nats turn 2 must also complete, matching the in-process transcript"
        );

        // Same surfaced reply text on the parked turn.
        assert!(
            serde_json::to_string(&r1.output)
                .unwrap()
                .contains("hello there"),
            "in-process turn 1 must surface the reply: {:?}",
            r1.output
        );
        assert!(
            serde_json::to_string(&n1r.output)
                .unwrap()
                .contains("hello there"),
            "nats turn 1 must surface the identical reply once the response resume lands: {:?}",
            n1r.output
        );
    }

    /// Build the error envelope shape a NATS response resume can also land in
    /// `state.entry`: `{ok:false, output:null, events:[], error:{code,
    /// message}}` (mirrors `agent_response_envelope`, but for the failure
    /// path — a genuine agent/transport error. This code sets no deadline of
    /// its own, but the same `{ok:false}` shape is also what a flow-authored
    /// timeout, or any other error source, would arrive as — Fix B handles it
    /// identically either way.
    #[cfg(feature = "agentic-worker")]
    fn agent_error_envelope(message: &str, code: Option<&str>) -> Value {
        json!({
            "ok": false,
            "output": Value::Null,
            "events": [],
            "error": { "code": code, "message": message },
        })
    }

    /// Fix A (interleave guard): a user message arriving before the agent's
    /// NATS response must NOT be misread as that response. With the
    /// pending-await marker set (turn 1's fresh dispatch), a resume whose
    /// `state.entry` is a plain user-message shape (no `"ok"` key) must fall
    /// through to the fresh-dispatch branch — re-dispatching to NATS as a new
    /// turn and parking via `AwaitHere` again — instead of being consumed as
    /// a (null) agent reply. The marker must also survive: it was NOT
    /// consumed by the misrouted resume, only by the eventual real response.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_interleaved_user_message_is_not_misread_as_response() {
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let engine = nats_conv_engine(conversational_dw_flow(true), dispatcher.clone());
        let rt = Runtime::new().unwrap();

        // Turn 1: fresh dispatch marks the pending-await and parks (AwaitHere).
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after turn 1 dispatch, got {other:?}"),
        };
        assert!(
            snapshot.state.pending_agent_await.contains_key("agent"),
            "turn 1 dispatch must mark the pending await"
        );
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1);

        // A stray user message arrives BEFORE the agent's NATS response —
        // same shape a real inbound activity would resume with, no `"ok"` key.
        let result = rt
            .block_on(engine.resume(
                conv_ctx(),
                snapshot,
                json!({ "text": "are you still there?" }),
            ))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!(
                "a stray user message must re-dispatch as a fresh turn (Waiting/AwaitHere), got {other:?}"
            ),
        };
        assert_eq!(
            snapshot.next_node, "agent",
            "the fresh re-dispatch still awaits at self"
        );
        assert_eq!(
            dispatcher.calls.lock().unwrap().len(),
            2,
            "the stray user message must trigger its OWN fresh NATS dispatch, not be swallowed"
        );
        assert!(
            snapshot.state.pending_agent_await.contains_key("agent"),
            "the marker must still be set for the real response to land against"
        );
        assert_eq!(
            result.output,
            Value::Null,
            "no reply is surfaced — this was not a misread null agent turn"
        );
        assert!(
            !snapshot.state.park_turns.contains_key("agent"),
            "a stray user message must not touch the park-loop cap"
        );
    }

    /// Fix B (error envelope handling): a `{ok:false, ...}` response — any
    /// agent/transport error, or a timeout-shaped envelope from any source
    /// (this code no longer sets its own deadline) — must surface the error
    /// message as the reply, re-park via `LoopHere` (fail-safe: await the
    /// next user message, do not force-advance), and must NOT bump the
    /// park-loop turn counter. Exercises two full error cycles (error →
    /// user turn → error) to confirm the cap counter never advances even
    /// after repeated failures.
    #[cfg(feature = "agentic-worker")]
    #[test]
    fn conversational_dw_agent_nats_error_envelope_surfaces_and_reparks_without_cap_bump() {
        let dispatcher = Arc::new(ScriptedNatsDispatcher {
            calls: Mutex::new(vec![]),
        });
        let engine = nats_conv_engine(conversational_dw_flow(true), dispatcher.clone());
        let rt = Runtime::new().unwrap();

        // Turn 1: fresh dispatch → AwaitHere.
        let result = rt
            .block_on(engine.execute(conv_ctx(), Value::Null))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting after turn 1 dispatch, got {other:?}"),
        };

        // A plain agent/transport error resumes the flow.
        let result = rt
            .block_on(engine.resume(conv_ctx(), snapshot, agent_error_envelope("boom", None)))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("an error envelope must re-park (Waiting/LoopHere), got {other:?}"),
        };
        assert_eq!(
            snapshot.next_node, "agent",
            "LoopHere re-enters the node itself"
        );
        assert!(
            serde_json::to_string(&result.output)
                .unwrap()
                .contains("boom"),
            "the error message must be surfaced as the reply: {:?}",
            result.output
        );
        assert!(
            !snapshot.state.park_turns.contains_key("agent"),
            "an error response must NOT bump the park-loop cap counter"
        );

        // A user turn in between re-dispatches (as usual).
        let result = rt
            .block_on(engine.resume(conv_ctx(), snapshot, json!({ "text": "hello?" })))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => panic!("expected Waiting (AwaitHere) after user turn, got {other:?}"),
        };
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 2);

        // A timeout-coded envelope (this code sets no deadline of its own —
        // this shape would only arrive from a flow-authored deadline or some
        // other upstream source) behaves identically to a plain error.
        let result = rt
            .block_on(engine.resume(
                conv_ctx(),
                snapshot,
                agent_error_envelope("timeout waiting for agent response", Some("timeout")),
            ))
            .unwrap();
        let snapshot = match result.status {
            FlowStatus::Waiting(w) => w.snapshot,
            other => {
                panic!("a timeout envelope must also re-park (Waiting/LoopHere), got {other:?}")
            }
        };
        assert!(
            serde_json::to_string(&result.output)
                .unwrap()
                .contains("timeout waiting for agent response"),
            "the timeout message must be surfaced as the reply: {:?}",
            result.output
        );
        assert!(
            !snapshot.state.park_turns.contains_key("agent"),
            "two error/timeout responses in a row (with an intervening user turn) must still \
             not have bumped the park-loop cap counter"
        );
    }

    /// Scriptable `AgentNodeHandler` stub: returns the next queued payload on
    /// each call, so a single in-process engine can simulate a multi-turn
    /// conversation with a different agent output per turn (unlike
    /// `StubAgentHandler`, which always returns the same fixed payload).
    #[cfg(feature = "agentic-worker")]
    struct ScriptedAgentHandler {
        script: Mutex<std::collections::VecDeque<serde_json::Value>>,
    }
    #[cfg(feature = "agentic-worker")]
    #[async_trait::async_trait]
    impl crate::runner::agent_node::AgentNodeHandler for ScriptedAgentHandler {
        async fn execute(
            &self,
            _tenant_id: &str,
            _env_id: &str,
            _agent_id: &str,
            _session_id: &str,
            _flow_input: &serde_json::Value,
            _conversational: bool,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedAgentHandler: script exhausted"))
        }
    }

    /// Build an in-process engine holding `flow`, wired to a `ScriptedAgentHandler`
    /// so each agent turn can return a different payload. Mirrors `conv_engine`
    /// (which uses a fixed payload for every call).
    #[cfg(feature = "agentic-worker")]
    fn conv_engine_scripted(
        flow: HostFlow,
        handler: std::sync::Arc<ScriptedAgentHandler>,
    ) -> FlowEngine {
        FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "conv.flow".to_string(),
                },
                flow,
            )])),
            default_env: "local".to_string(),
            validation: ValidationConfig {
                mode: ValidationMode::Off,
            },
            cross_pack_resolver: None,
            remote_dispatch_handler: None,
            dw_agent_dispatch: crate::runner::agent_node::DwAgentDispatch::InProcess,
            agent_node_handler: Some(handler),
            graph_node_handler: None,
            mcp_tool_source: None,
            operala_node_handler: None,
        }
    }

    // -----------------------------------------------------------------------
    // operala.call in-process handler wiring
    // -----------------------------------------------------------------------

    struct StubOperalaHandler {
        payload: serde_json::Value,
    }
    #[async_trait::async_trait]
    impl crate::runner::operala_node::OperalaNodeHandler for StubOperalaHandler {
        async fn execute(
            &self,
            _tenant: &str,
            _env: &str,
            _target: &str,
            _operation: &str,
            _session_id: &str,
            _input: &serde_json::Value,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(self.payload.clone())
        }
    }

    /// Build a single-node flow: an `operala.call` node (given `target`) that
    /// ends the flow directly. Mirrors `conversational_dw_flow`'s shape for
    /// the (non-conversational) operala path.
    fn operala_flow(target: &str) -> HostFlow {
        let mut nodes = IndexMap::new();
        let node_id = NodeId::from_str("op").unwrap();
        nodes.insert(
            node_id.clone(),
            HostNode {
                kind: NodeKind::OperalaCall {
                    target: target.to_string(),
                },
                component: "operala.call".to_string(),
                component_id: "operala.call".to_string(),
                operation_name: Some(target.to_string()),
                operation_in_mapping: None,
                payload_expr: json!({ "operation": "", "input": { "goal": "hi" } }),
                routing: Routing::End,
                vars_out: None,
            },
        );
        HostFlow {
            slot_schema: None,
            id: "operala.flow".to_string(),
            start: Some(node_id),
            nodes,
            vars_init: JsonMap::new(),
            required_vars: Vec::new(),
        }
    }

    /// Build an engine holding `flow` with the given optional in-process
    /// operala handler and no NATS `RemoteDispatchHandler` — a `None` handler
    /// exercises the existing NATS-fallback error path.
    fn operala_engine(
        flow: HostFlow,
        handler: Option<std::sync::Arc<dyn crate::runner::operala_node::OperalaNodeHandler>>,
    ) -> FlowEngine {
        FlowEngine {
            rollout_ids: RolloutIds::default(),
            packs: Vec::new(),
            flows: Vec::new(),
            flow_sources: StdHashMap::new(),
            flow_cache: RwLock::new(StdHashMap::from([(
                FlowKey {
                    pack_id: "test-pack".to_string(),
                    flow_id: "operala.flow".to_string(),
                },
                flow,
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
            operala_node_handler: handler,
        }
    }

    fn operala_ctx<'a>() -> FlowContext<'a> {
        FlowContext {
            tenant: "demo",
            pack_id: "test-pack",
            flow_id: "operala.flow",
            node_id: None,
            tool: None,
            action: None,
            session_id: Some("sess-op"),
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
    fn operala_call_with_in_process_handler_completes_inline() {
        let handler = std::sync::Arc::new(StubOperalaHandler {
            payload: json!({ "reply": "stub" }),
        });
        let engine = operala_engine(operala_flow("deep_worker"), Some(handler));
        let execution = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(engine.execute(operala_ctx(), Value::Null))
            .expect("operala.call with an in-process handler must complete");
        assert!(matches!(execution.status, FlowStatus::Completed));
        assert_eq!(execution.output, json!({ "reply": "stub" }));
    }

    #[test]
    fn operala_call_without_handler_or_nats_fails() {
        let engine = operala_engine(operala_flow("deep_worker"), None);
        let execution = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(engine.execute(operala_ctx(), Value::Null))
            .expect("session flow returns a completed error envelope, not an Err");
        // `operala_ctx` carries a `session_id`, so main's retry-exhaustion policy
        // wraps the execution error into a `Completed` flow carrying the
        // `flow_execution_failed` envelope (graceful degradation for session
        // flows) rather than propagating an `Err`. The operala misconfiguration
        // is still surfaced loudly — in the error payload.
        assert!(matches!(execution.status, FlowStatus::Completed));
        let msg = execution.output["metadata"]["error_message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            msg.contains("operala.call node dispatched but no RemoteDispatchHandler configured"),
            "unexpected output: {:?}",
            execution.output
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

/// Look across all node outputs, find the first one that finished with
/// `ok=false`, and lift its `meta.error` fields into
/// `output.metadata.error_kind` / `.error_message` / `.node_id`. Returns the
/// (possibly enriched) output unchanged otherwise.
///
/// This is how the executor "shows" an unhandled flow-node failure to the
/// caller without the flow author having to add error routing: the chat-side
/// provider (messaging-providers `extract_error_envelope`) picks the lifted
/// fields off `output.metadata` and renders a styled error card.
///
/// Takes a borrow of the node-output map rather than the whole
/// `ExecutionState` because the callers have already consumed `state` via
/// `state.finalize_with(...)`; we capture a cheap clone of `state.nodes` up
/// front and pass it in here.
fn lift_first_node_error_from_nodes(output: Value, nodes: &HashMap<String, NodeOutput>) -> Value {
    let Some((node_id, failed)) = nodes.iter().find(|(_, out)| !out.ok) else {
        return output;
    };
    let err_meta = failed.meta.get("error");
    let message = err_meta
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("flow node failed");
    let kind = err_meta
        .and_then(|e| e.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("flow_node_failed");

    let mut output = match output {
        Value::Object(map) => map,
        Value::Null => JsonMap::new(),
        other => {
            let mut wrap = JsonMap::new();
            wrap.insert("payload".to_string(), other);
            wrap
        }
    };
    let metadata_entry = output
        .entry("metadata".to_string())
        .or_insert_with(|| Value::Object(JsonMap::new()));
    let metadata_map = match metadata_entry {
        Value::Object(map) => map,
        _ => {
            *metadata_entry = Value::Object(JsonMap::new());
            metadata_entry.as_object_mut().unwrap()
        }
    };
    metadata_map
        .entry("error_kind".to_string())
        .or_insert(Value::String(kind.to_string()));
    metadata_map
        .entry("error_message".to_string())
        .or_insert(Value::String(message.to_string()));
    metadata_map
        .entry("node_id".to_string())
        .or_insert(Value::String(node_id.clone()));
    Value::Object(output)
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
