//! Flow handling, parsing, and conversion for packs.

use super::helpers::read_zip_entry;
use super::metadata::PackMetadata;
use anyhow::{Context, Result};
use greentic_flow::model::FlowDoc;
use greentic_pack::builder as legacy_pack;
use greentic_types::flow::FlowHasher;
use greentic_types::{
    ComponentId, ExtensionRef, Flow, FlowComponentRef, FlowId, FlowKind, FlowMetadata,
    InputMapping, Node, NodeId, OutputMapping, Routing, TelemetryHints,
    pack_manifest::ExtensionInline,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;
use std::str::FromStr;
use tracing::warn;
use zip::ZipArchive;

use crate::runner::flow_adapter::{flow_doc_to_ir, flow_ir_to_flow};

/// Descriptor for a flow within a pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDescriptor {
    pub id: String,
    #[serde(rename = "type")]
    pub flow_type: String,
    pub pack_id: String,
    pub profile: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Cached flows from a pack.
pub(crate) struct PackFlows {
    pub descriptors: Vec<FlowDescriptor>,
    pub flows: HashMap<String, Flow>,
    pub metadata: PackMetadata,
}

/// Loaded manifest with flows.
pub(crate) enum ManifestLoad {
    New {
        manifest: Box<greentic_types::PackManifest>,
        flows: PackFlows,
    },
    Legacy {
        manifest: Box<legacy_pack::PackManifest>,
        flows: PackFlows,
    },
}

const RUNTIME_FLOW_EXTENSION_IDS: [&str; 3] = [
    "greentic.pack.runtime_flow",
    "greentic.pack.flow_runtime",
    "greentic.pack.runtime_flows",
];

#[derive(Debug, Deserialize)]
struct RuntimeFlowBundle {
    flows: Vec<RuntimeFlow>,
}

#[derive(Debug, Deserialize)]
struct RuntimeFlow {
    id: String,
    #[serde(alias = "flow_type")]
    kind: FlowKind,
    #[serde(default)]
    schema_version: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    entrypoints: BTreeMap<String, Value>,
    nodes: BTreeMap<String, RuntimeNode>,
    #[serde(default)]
    metadata: Option<FlowMetadata>,
}

#[derive(Debug, Deserialize)]
struct RuntimeNode {
    #[serde(alias = "component")]
    component_id: String,
    #[serde(default, alias = "operation")]
    operation_name: Option<String>,
    #[serde(default, alias = "payload", alias = "input")]
    operation_payload: Value,
    #[serde(default)]
    routing: Option<Routing>,
    #[serde(default)]
    telemetry: Option<TelemetryHints>,
}

impl PackFlows {
    /// Create flows from a new-style manifest.
    pub fn from_manifest(manifest: greentic_types::PackManifest) -> Self {
        if let Some(flows) = flows_from_runtime_extension(&manifest) {
            return flows;
        }
        let descriptors = manifest
            .flows
            .iter()
            .map(|entry| FlowDescriptor {
                id: entry.id.as_str().to_string(),
                flow_type: flow_kind_to_str(entry.kind).to_string(),
                pack_id: manifest.pack_id.as_str().to_string(),
                profile: manifest.pack_id.as_str().to_string(),
                version: manifest.version.to_string(),
                description: None,
            })
            .collect();
        let mut flows = HashMap::new();
        for entry in &manifest.flows {
            flows.insert(entry.id.as_str().to_string(), entry.flow.clone());
        }
        Self {
            metadata: PackMetadata::from_manifest(&manifest),
            descriptors,
            flows,
        }
    }
}

/// Load manifest and flows from an archive.
pub(crate) fn load_manifest_and_flows(path: &Path) -> Result<ManifestLoad> {
    let mut archive = ZipArchive::new(File::open(path)?)
        .with_context(|| format!("{} is not a valid gtpack", path.display()))?;
    let bytes = read_zip_entry(&mut archive, "manifest.cbor")
        .with_context(|| format!("missing manifest.cbor in {}", path.display()))?;
    match greentic_types::decode_pack_manifest(&bytes) {
        Ok(manifest) => {
            let cache = PackFlows::from_manifest(manifest.clone());
            Ok(ManifestLoad::New {
                manifest: Box::new(manifest),
                flows: cache,
            })
        }
        Err(err) => {
            tracing::debug!(
                error = %err,
                pack = %path.display(),
                "decode_pack_manifest failed for archive; falling back to legacy manifest"
            );
            let legacy: legacy_pack::PackManifest = serde_cbor::from_slice(&bytes)
                .context("failed to decode legacy pack manifest from manifest.cbor")?;
            let flows = load_legacy_flows_from_archive(&mut archive, path, &legacy)?;
            Ok(ManifestLoad::Legacy {
                manifest: Box::new(legacy),
                flows,
            })
        }
    }
}

/// Load manifest and flows from a materialized directory.
pub(crate) fn load_manifest_and_flows_from_dir(root: &Path) -> Result<ManifestLoad> {
    let manifest_path = root.join("manifest.cbor");
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("missing manifest.cbor in {}", root.display()))?;
    match greentic_types::decode_pack_manifest(&bytes) {
        Ok(manifest) => {
            let cache = PackFlows::from_manifest(manifest.clone());
            Ok(ManifestLoad::New {
                manifest: Box::new(manifest),
                flows: cache,
            })
        }
        Err(err) => {
            tracing::debug!(
                error = %err,
                pack = %root.display(),
                "decode_pack_manifest failed for materialized pack; trying legacy manifest"
            );
            let legacy: legacy_pack::PackManifest = serde_cbor::from_slice(&bytes)
                .context("failed to decode legacy pack manifest from manifest.cbor")?;
            let flows = load_legacy_flows_from_dir(root, &legacy)?;
            Ok(ManifestLoad::Legacy {
                manifest: Box::new(legacy),
                flows,
            })
        }
    }
}

fn load_legacy_flows_from_dir(
    root: &Path,
    manifest: &legacy_pack::PackManifest,
) -> Result<PackFlows> {
    build_legacy_flows(manifest, |rel_path| {
        let path = root.join(rel_path);
        std::fs::read(&path).with_context(|| format!("missing flow json {}", path.display()))
    })
}

fn load_legacy_flows_from_archive(
    archive: &mut ZipArchive<File>,
    pack_path: &Path,
    manifest: &legacy_pack::PackManifest,
) -> Result<PackFlows> {
    build_legacy_flows(manifest, |rel_path| {
        read_zip_entry(archive, rel_path)
            .with_context(|| format!("missing flow json {} in {}", rel_path, pack_path.display()))
    })
}

fn build_legacy_flows(
    manifest: &legacy_pack::PackManifest,
    mut read_json: impl FnMut(&str) -> Result<Vec<u8>>,
) -> Result<PackFlows> {
    let mut flows = HashMap::new();
    let mut descriptors = Vec::new();

    for entry in &manifest.flows {
        let bytes = read_json(&entry.file_json)
            .with_context(|| format!("missing flow json {}", entry.file_json))?;
        let doc = parse_flow_doc_with_legacy_aliases(&bytes, &entry.file_json)?;
        let normalized = normalize_flow_doc(doc);
        let flow_ir = flow_doc_to_ir(normalized)?;
        let flow = flow_ir_to_flow(flow_ir)?;

        descriptors.push(FlowDescriptor {
            id: entry.id.clone(),
            flow_type: entry.kind.clone(),
            pack_id: manifest.meta.pack_id.clone(),
            profile: manifest.meta.pack_id.clone(),
            version: manifest.meta.version.to_string(),
            description: None,
        });
        flows.insert(entry.id.clone(), flow);
    }

    let mut entry_flows = manifest.meta.entry_flows.clone();
    if entry_flows.is_empty() {
        entry_flows = manifest.flows.iter().map(|f| f.id.clone()).collect();
    }
    let metadata = PackMetadata {
        pack_id: manifest.meta.pack_id.clone(),
        version: manifest.meta.version.to_string(),
        entry_flows,
        secret_requirements: Vec::new(),
    };

    Ok(PackFlows {
        descriptors,
        flows,
        metadata,
    })
}

fn parse_flow_doc_with_legacy_aliases(bytes: &[u8], path: &str) -> Result<FlowDoc> {
    let mut value: Value = serde_json::from_slice(bytes)
        .with_context(|| format!("failed to decode flow doc {}", path))?;
    if let Some(map) = value.as_object_mut()
        && !map.contains_key("type")
        && let Some(flow_type) = map.remove("flow_type")
    {
        map.insert("type".to_string(), flow_type);
    }
    serde_json::from_value(value).with_context(|| format!("failed to decode flow doc {}", path))
}

pub(crate) fn normalize_flow_doc(mut doc: FlowDoc) -> FlowDoc {
    for node in doc.nodes.values_mut() {
        let Some((component_ref, payload)) = node
            .raw
            .iter()
            .next()
            .map(|(key, value)| (key.clone(), value.clone()))
        else {
            continue;
        };
        if component_ref.starts_with("emit.") {
            node.operation = Some(component_ref);
            node.payload = payload;
            node.raw.clear();
            continue;
        }
        let (target_component, operation, input, config) =
            infer_component_exec(&payload, &component_ref);
        let mut payload_obj = serde_json::Map::new();
        payload_obj.insert("component".into(), Value::String(target_component));
        payload_obj.insert("operation".into(), Value::String(operation));
        payload_obj.insert("input".into(), input);
        if let Some(cfg) = config {
            payload_obj.insert("config".into(), cfg);
        }
        node.operation = Some("component.exec".to_string());
        node.payload = Value::Object(payload_obj);
        node.raw.clear();
    }
    doc
}

fn infer_component_exec(
    payload: &Value,
    component_ref: &str,
) -> (String, String, Value, Option<Value>) {
    let default_op = if component_ref.starts_with("templating.") {
        "render"
    } else {
        "invoke"
    }
    .to_string();

    if let Value::Object(map) = payload {
        let op = map
            .get("op")
            .or_else(|| map.get("operation"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| default_op.clone());

        let mut input = map.clone();
        let config = input.remove("config");
        let component = input
            .get("component")
            .or_else(|| input.get("component_ref"))
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| component_ref.to_string());
        input.remove("component");
        input.remove("component_ref");
        input.remove("op");
        input.remove("operation");
        return (component, op, Value::Object(input), config);
    }

    (component_ref.to_string(), default_op, payload.clone(), None)
}

fn flows_from_runtime_extension(manifest: &greentic_types::PackManifest) -> Option<PackFlows> {
    let extensions = manifest.extensions.as_ref()?;
    let extension = extensions.iter().find_map(|(key, ext)| {
        if RUNTIME_FLOW_EXTENSION_IDS
            .iter()
            .any(|candidate| candidate == key)
        {
            Some(ext)
        } else {
            None
        }
    })?;
    let runtime_flows = match decode_runtime_flow_extension(extension) {
        Some(flows) if !flows.is_empty() => flows,
        _ => return None,
    };

    let descriptors = runtime_flows
        .iter()
        .map(|flow| FlowDescriptor {
            id: flow.id.as_str().to_string(),
            flow_type: flow_kind_to_str(flow.kind).to_string(),
            pack_id: manifest.pack_id.as_str().to_string(),
            profile: manifest.pack_id.as_str().to_string(),
            version: manifest.version.to_string(),
            description: None,
        })
        .collect::<Vec<_>>();
    let flows = runtime_flows
        .into_iter()
        .map(|flow| (flow.id.as_str().to_string(), flow))
        .collect();

    Some(PackFlows {
        metadata: PackMetadata::from_manifest(manifest),
        descriptors,
        flows,
    })
}

fn decode_runtime_flow_extension(extension: &ExtensionRef) -> Option<Vec<Flow>> {
    let value = match extension.inline.as_ref()? {
        ExtensionInline::Other(value) => value.clone(),
        _ => return None,
    };

    if let Ok(bundle) = serde_json::from_value::<RuntimeFlowBundle>(value.clone()) {
        return Some(collect_runtime_flows(bundle.flows));
    }
    if let Ok(flows) = serde_json::from_value::<Vec<RuntimeFlow>>(value.clone()) {
        return Some(collect_runtime_flows(flows));
    }
    if let Ok(flows) = serde_json::from_value::<Vec<Flow>>(value) {
        return Some(flows);
    }

    warn!(
        extension = %extension.kind,
        version = %extension.version,
        "runtime flow extension present but could not be decoded"
    );
    None
}

fn collect_runtime_flows(flows: Vec<RuntimeFlow>) -> Vec<Flow> {
    flows
        .into_iter()
        .filter_map(|flow| match runtime_flow_to_flow(flow) {
            Ok(flow) => Some(flow),
            Err(err) => {
                warn!(error = %err, "failed to decode runtime flow");
                None
            }
        })
        .collect()
}

fn runtime_flow_to_flow(runtime: RuntimeFlow) -> Result<Flow> {
    let flow_id = FlowId::from_str(&runtime.id)
        .with_context(|| format!("invalid flow id `{}`", runtime.id))?;
    let mut entrypoints = runtime.entrypoints;
    if entrypoints.is_empty()
        && let Some(start) = &runtime.start
    {
        entrypoints.insert("default".into(), Value::String(start.clone()));
    }

    let mut nodes: IndexMap<NodeId, Node, FlowHasher> = IndexMap::default();
    for (id, node) in runtime.nodes {
        let node_id = NodeId::from_str(&id).with_context(|| format!("invalid node id `{id}`"))?;
        let component_id = ComponentId::from_str(&node.component_id)
            .with_context(|| format!("invalid component id `{}`", node.component_id))?;
        let component = FlowComponentRef {
            id: component_id,
            pack_alias: None,
            operation: node.operation_name,
        };
        let routing = node.routing.unwrap_or(Routing::End);
        let telemetry = node.telemetry.unwrap_or_default();
        nodes.insert(
            node_id.clone(),
            Node {
                id: node_id,
                component,
                input: InputMapping {
                    mapping: node.operation_payload,
                },
                output: OutputMapping {
                    mapping: Value::Null,
                },
                routing,
                telemetry,
            },
        );
    }

    Ok(Flow {
        schema_version: runtime.schema_version.unwrap_or_else(|| "1.0".to_string()),
        id: flow_id,
        kind: runtime.kind,
        entrypoints,
        nodes,
        metadata: runtime.metadata.unwrap_or_default(),
    })
}

pub(crate) fn flow_kind_to_str(kind: FlowKind) -> &'static str {
    match kind {
        FlowKind::Messaging => "messaging",
        FlowKind::Event => "event",
        FlowKind::ComponentConfig => "component-config",
        FlowKind::Job => "job",
        FlowKind::Http => "http",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_flow::model::NodeDoc;
    use indexmap::IndexMap;
    use serde_json::json;

    #[test]
    fn normalizes_raw_component_to_component_exec() {
        let mut nodes = IndexMap::new();
        let mut raw = IndexMap::new();
        raw.insert(
            "templating.handlebars".into(),
            json!({ "template": "Hi {{name}}" }),
        );
        nodes.insert(
            "start".into(),
            NodeDoc {
                raw,
                routing: json!([{"out": true}]),
                ..Default::default()
            },
        );
        let doc = FlowDoc {
            id: "welcome".into(),
            title: None,
            description: None,
            flow_type: "messaging".into(),
            start: Some("start".into()),
            parameters: json!({}),
            tags: Vec::new(),
            schema_version: None,
            entrypoints: IndexMap::new(),
            meta: None,
            nodes,
        };

        let normalized = normalize_flow_doc(doc);
        let node = normalized.nodes.get("start").expect("node exists");
        assert_eq!(node.operation.as_deref(), Some("component.exec"));
        assert!(node.raw.is_empty());
        let payload = node.payload.as_object().expect("payload object");
        assert_eq!(
            payload.get("component"),
            Some(&Value::String("templating.handlebars".into()))
        );
        assert_eq!(
            payload.get("operation"),
            Some(&Value::String("render".into()))
        );
        let input = payload.get("input").unwrap();
        assert_eq!(input, &json!({ "template": "Hi {{name}}" }));
    }
}
