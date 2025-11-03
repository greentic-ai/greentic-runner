#![cfg(feature = "schema")]

use greentic_types::policy::{AllowList, NetworkPolicy};
use schemars::{JsonSchema, SchemaGenerator};
use serde_json::json;

#[allow(dead_code)]
#[derive(JsonSchema)]
struct ProviderMetaSchema {
    name: String,
    version: String,
    capabilities: Vec<String>,
    allow_list: AllowList,
    network_policy: NetworkPolicy,
}

#[test]
fn provider_meta_schema_snapshot() {
    let schema = SchemaGenerator::default().into_root_schema_for::<ProviderMetaSchema>();
    insta::assert_json_snapshot!("provider_meta_schema", json!(schema));
}
