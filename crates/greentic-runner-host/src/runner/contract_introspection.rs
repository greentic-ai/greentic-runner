use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::pack::PackRuntime;

#[derive(Debug, Clone)]
pub struct IntrospectedContract {
    pub selected_operation: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub config_schema: Value,
    pub describe_hash: String,
    pub schema_hash: String,
}

#[derive(Serialize)]
struct DescribeHashMaterial<'a> {
    component_ref: &'a str,
    operation: &'a str,
    world: &'a str,
    input_schema: &'a Value,
    output_schema: &'a Value,
}

#[derive(Serialize)]
struct SchemaHashMaterial<'a> {
    component_ref: &'a str,
    operation: &'a str,
    input_schema: &'a Value,
    output_schema: &'a Value,
    config_schema: &'a Value,
}

pub fn introspect_component_contract(
    pack: &PackRuntime,
    component_ref: &str,
    requested_operation: &str,
) -> Result<Option<IntrospectedContract>> {
    let Some(manifest) = pack.component_manifest(component_ref) else {
        return Ok(None);
    };
    if !manifest.world.contains("greentic:component@0.6.0") {
        return Ok(None);
    }

    let describe_payload = pack
        .describe_component_contract_v0_6(component_ref)?
        .ok_or_else(|| anyhow::anyhow!("component does not export 0.6 describe()"))?;

    let selected = select_operation_from_describe(&describe_payload, requested_operation)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "operation `{}` not present in 0.6 describe payload for component `{}`",
                requested_operation,
                component_ref
            )
        })?;

    let input_schema = extract_operation_schema(&describe_payload, &selected, "input")
        .map(canonicalize_json)
        .unwrap_or(Value::Null);
    let output_schema = extract_operation_schema(&describe_payload, &selected, "output")
        .map(canonicalize_json)
        .unwrap_or(Value::Null);
    let config_schema = extract_config_schema(&describe_payload)
        .map(canonicalize_json)
        .unwrap_or(Value::Null);

    let describe_material = DescribeHashMaterial {
        component_ref,
        operation: selected.as_str(),
        world: manifest.world.as_str(),
        input_schema: &input_schema,
        output_schema: &output_schema,
    };
    let describe_hash = sha256_prefixed(
        &serde_cbor::to_vec(&describe_material).expect("describe hash material serialization"),
    );

    let schema_material = SchemaHashMaterial {
        component_ref,
        operation: selected.as_str(),
        input_schema: &input_schema,
        output_schema: &output_schema,
        config_schema: &config_schema,
    };
    let schema_hash = sha256_prefixed(
        &serde_cbor::to_vec(&schema_material).expect("schema hash material serialization"),
    );

    Ok(Some(IntrospectedContract {
        selected_operation: selected,
        input_schema,
        output_schema,
        config_schema,
        describe_hash,
        schema_hash,
    }))
}

fn select_operation_from_describe(payload: &Value, requested_operation: &str) -> Option<String> {
    let ops = payload.get("operations")?.as_array()?;
    let requested = ops
        .iter()
        .find_map(|entry| operation_name(entry))
        .filter(|name| *name == requested_operation)
        .map(ToString::to_string);
    if requested.is_some() {
        return requested;
    }
    ops.iter()
        .find_map(|entry| operation_name(entry))
        .filter(|name| *name == "run")
        .map(ToString::to_string)
        .or_else(|| {
            ops.first()
                .and_then(operation_name)
                .map(ToString::to_string)
        })
}

fn operation_name(value: &Value) -> Option<&str> {
    value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("name").and_then(Value::as_str))
}

fn extract_operation_schema(payload: &Value, operation: &str, side: &str) -> Option<Value> {
    let ops = payload.get("operations")?.as_array()?;
    let op = ops
        .iter()
        .find(|entry| operation_name(entry) == Some(operation))?;
    let side_obj = op.get(side)?;
    side_obj
        .get("schema")
        .cloned()
        .or_else(|| op.get(format!("{side}_schema")).cloned())
}

fn extract_config_schema(payload: &Value) -> Option<Value> {
    payload.get("config_schema").cloned()
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    format!("sha256:{}", to_hex(&digest))
}

fn to_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut ordered = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                let normalized = map
                    .get(&key)
                    .cloned()
                    .map(canonicalize_json)
                    .unwrap_or(Value::Null);
                ordered.insert(key, normalized);
            }
            Value::Object(ordered)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}
