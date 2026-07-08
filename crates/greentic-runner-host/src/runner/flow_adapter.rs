use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use greentic_flow::model::{FlowDoc, NodeDoc};
use greentic_types::flow::FlowHasher;
use greentic_types::{
    ComponentId, Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata, InputMapping, Node,
    NodeId, OutputMapping, Routing, TelemetryHints,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowIR {
    pub id: String,
    pub flow_type: String,
    pub start: Option<String>,
    pub parameters: Value,
    pub nodes: IndexMap<String, NodeIR>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIR {
    pub component: String,
    pub payload_expr: Value,
    pub routes: Vec<RouteIR>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteIR {
    pub to: Option<String>,
    #[serde(default)]
    pub out: bool,
}

const FLOW_SCHEMA_VERSION: &str = "1.0";

pub fn flow_doc_to_ir(doc: FlowDoc) -> Result<FlowIR> {
    let mut nodes: IndexMap<String, NodeIR> = IndexMap::new();
    for (id, node) in doc.nodes {
        let (component, payload_expr) = extract_node_payload(&node)
            .with_context(|| format!("missing component for node `{id}`"))?;
        let routes = parse_routes(node.routing)?;
        nodes.insert(
            id.clone(),
            NodeIR {
                component,
                payload_expr,
                routes,
            },
        );
    }

    Ok(FlowIR {
        id: doc.id,
        flow_type: doc.flow_type,
        start: doc.start,
        parameters: doc.parameters,
        nodes,
    })
}

fn extract_node_payload(node: &NodeDoc) -> Result<(String, Value)> {
    if let Some(operation) = node.operation.as_deref() {
        return Ok((operation.to_string(), node.payload.clone()));
    }
    let Some((component, payload)) = node
        .raw
        .iter()
        .next()
        .map(|(key, value)| (key.clone(), value.clone()))
    else {
        return Err(anyhow!("node missing operation payload"));
    };
    Ok((component, payload))
}

pub fn flow_ir_to_flow(flow_ir: FlowIR) -> Result<Flow> {
    let id = FlowId::from_str(&flow_ir.id)
        .with_context(|| format!("invalid flow id `{}`", flow_ir.id))?;
    let kind = map_flow_kind(&flow_ir.flow_type)?;

    let mut entrypoints = BTreeMap::new();
    if let Some(start) = &flow_ir.start {
        entrypoints.insert("default".to_string(), Value::String(start.clone()));
    }

    let nodes = map_nodes(flow_ir.nodes)?;
    let metadata = build_metadata(flow_ir.start, flow_ir.parameters);

    Ok(Flow {
        schema_version: FLOW_SCHEMA_VERSION.to_string(),
        id,
        kind,
        entrypoints,
        nodes,
        metadata,
    })
}

fn map_flow_kind(kind: &str) -> Result<FlowKind> {
    match kind {
        "messaging" => Ok(FlowKind::Messaging),
        "event" | "events" => Ok(FlowKind::Event),
        "component-config" => Ok(FlowKind::ComponentConfig),
        "job" => Ok(FlowKind::Job),
        "http" => Ok(FlowKind::Http),
        other => Err(anyhow!("unknown flow kind `{other}`")),
    }
}

fn map_nodes(nodes: IndexMap<String, NodeIR>) -> Result<IndexMap<NodeId, Node, FlowHasher>> {
    let mut mapped: IndexMap<NodeId, Node, FlowHasher> = IndexMap::default();
    for (raw_id, node_ir) in nodes {
        let node_id =
            NodeId::from_str(&raw_id).with_context(|| format!("invalid node id `{raw_id}`"))?;
        let node = map_node(node_id.clone(), node_ir)?;
        mapped.insert(node_id, node);
    }
    Ok(mapped)
}

fn map_node(node_id: NodeId, node_ir: NodeIR) -> Result<Node> {
    let component_id = ComponentId::from_str(&node_ir.component).with_context(|| {
        format!(
            "invalid component ref `{}` for node {}",
            node_ir.component,
            node_id.as_str()
        )
    })?;
    let component = FlowComponentRef {
        id: component_id,
        pack_alias: None,
        operation: None,
    };
    let routing = map_routing(&node_ir.routes)?;
    Ok(Node {
        id: node_id,
        component,
        input: InputMapping {
            mapping: node_ir.payload_expr,
        },
        output: OutputMapping {
            mapping: Value::Object(JsonMap::new()),
        },
        err_map: None,
        routing,
        telemetry: TelemetryHints::default(),
        conversational: false,
    })
}

fn map_routing(routes: &[RouteIR]) -> Result<Routing> {
    if routes.is_empty() {
        return Ok(Routing::End);
    }

    if routes.len() == 1 {
        let route = &routes[0];
        if route.out || route.to.as_deref() == Some("out") {
            return Ok(Routing::End);
        }
        if let Some(to) = &route.to {
            let node_id =
                NodeId::from_str(to).with_context(|| format!("invalid route target `{to}`"))?;
            return Ok(Routing::Next { node_id });
        }
    }

    serde_json::to_value(routes)
        .map(Routing::Custom)
        .map_err(|err| anyhow!(err))
}

fn build_metadata(start: Option<String>, parameters: Value) -> FlowMetadata {
    let mut extra = JsonMap::new();
    if let Some(start) = start {
        extra.insert("start".into(), Value::String(start));
    }
    if !parameters.is_null() {
        extra.insert("parameters".into(), parameters);
    }
    FlowMetadata {
        title: None,
        description: None,
        tags: BTreeSet::new(),
        extra: Value::Object(extra),
    }
}

fn parse_routes(raw: Value) -> Result<Vec<RouteIR>> {
    if raw.is_null() {
        return Ok(Vec::new());
    }
    serde_json::from_value::<Vec<RouteIR>>(raw.clone()).map_err(|err| {
        anyhow!("failed to parse routes from node routing: {err}; value was {raw:?}")
    })
}
