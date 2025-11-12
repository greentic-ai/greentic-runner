use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::{collections::HashSet, fs, path::Path};
use url::Url;

use self::component::ComponentFeatures;

pub mod component;

#[derive(Debug)]
pub struct PackMetadata {
    pub name: String,
    pub flows: Vec<FlowMetadata>,
    pub hints: BindingsHints,
}

#[derive(Debug)]
pub struct FlowMetadata {
    pub name: String,
    pub document: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BindingsHints {
    #[serde(default)]
    pub network: NetworkHints,
    #[serde(default)]
    pub secrets: SecretsHints,
    #[serde(default)]
    pub env: EnvHints,
    #[serde(default)]
    pub mcp: McpHints,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NetworkHints {
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SecretsHints {
    #[serde(default)]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EnvHints {
    #[serde(default)]
    pub passthrough: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpHints {
    #[serde(default)]
    pub servers: Vec<McpServer>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpServer {
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    #[serde(default)]
    pub caps: Vec<String>,
}

pub fn load_pack(pack_dir: &Path) -> Result<PackMetadata> {
    if !pack_dir.is_dir() {
        anyhow::bail!("pack directory {} does not exist", pack_dir.display());
    }

    let manifest = pack_dir.join("pack.yaml");
    let content = fs::read_to_string(&manifest)
        .with_context(|| format!("failed to read {}", manifest.display()))?;
    let pack_manifest: PackManifest =
        serde_yaml::from_str(&content).with_context(|| "failed to parse pack manifest")?;

    let hints_path = pack_dir.join("bindings.hints.yaml");
    let hints = if hints_path.exists() {
        serde_yaml::from_reader(fs::File::open(&hints_path)?)
            .with_context(|| format!("failed to read hints {}", hints_path.display()))?
    } else {
        BindingsHints::default()
    };

    let flows_dir = pack_dir.join("flows");
    let mut flows = Vec::new();
    if flows_dir.is_dir() {
        for entry in fs::read_dir(&flows_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
                let flow = load_flow(&path)?;
                flows.push(flow);
            }
        }
    }

    let name = pack_manifest
        .name
        .or_else(|| {
            pack_dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "pack".to_string());

    Ok(PackMetadata { name, flows, hints })
}

fn load_flow(path: &Path) -> Result<FlowMetadata> {
    let content = fs::read_to_string(path)?;
    let parsed: Value = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse flow {}", path.display()))?;
    let name = parsed
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "<unknown>".to_string());
    Ok(FlowMetadata {
        name,
        document: parsed,
    })
}

#[derive(Debug, Deserialize)]
struct PackManifest {
    name: Option<String>,
}

#[derive(Clone, Default)]
pub struct GeneratorOptions {
    pub strict: bool,
    pub complete: bool,
    pub component: Option<ComponentFeatures>,
}

#[derive(Debug, Serialize)]
pub struct GeneratedBindings {
    pub tenant: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_passthrough: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub network_allow: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets_required: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<FlowHint>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Serialize)]
pub struct FlowHint {
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mcp_components: Vec<String>,
}

impl FlowHint {
    fn from_flow(flow: &FlowMetadata) -> Self {
        let mut bindings = collect_flow_bindings(&flow.document);
        let meta = find_meta_bindings(&flow.document);
        bindings.urls.extend(meta.urls);
        bindings.secrets.extend(meta.secrets);
        bindings.env.extend(meta.env);
        bindings.mcp_components.extend(meta.mcp_components);
        FlowHint {
            name: flow.name.clone(),
            urls: bindings.urls.clone(),
            secrets: bindings.secrets.clone(),
            env: bindings.env.clone(),
            mcp_components: bindings.mcp_components.clone(),
        }
    }
}

#[derive(Default)]
struct FlowBindings {
    urls: Vec<String>,
    secrets: Vec<String>,
    env: Vec<String>,
    mcp_components: Vec<String>,
}

fn collect_flow_bindings(doc: &Value) -> FlowBindings {
    let mut bindings = FlowBindings::default();
    scan_value_for_placeholders(doc, &mut bindings);
    collect_mcp_components(doc, &mut bindings);
    bindings
}

fn scan_value_for_placeholders(value: &Value, bindings: &mut FlowBindings) {
    match value {
        Value::String(s) => {
            bindings.secrets.extend(extract_placeholders(s, "secrets."));
            bindings.env.extend(extract_placeholders(s, "env."));
            if let Some(origin) = extract_origin(s) {
                bindings.urls.push(origin);
            }
        }
        Value::Sequence(seq) => {
            for item in seq {
                scan_value_for_placeholders(item, bindings);
            }
        }
        Value::Mapping(map) => {
            for (_, item) in map {
                scan_value_for_placeholders(item, bindings);
            }
        }
        _ => {}
    }
}

fn collect_mcp_components(value: &Value, bindings: &mut FlowBindings) {
    match value {
        Value::Mapping(map) => {
            let exec_key = Value::String("mcp.exec".into());
            let component_key = Value::String("component".into());
            if let Some(Value::Mapping(exec_map)) = map.get(&exec_key)
                && let Some(Value::String(component)) = exec_map.get(&component_key)
            {
                bindings.mcp_components.push(component.clone());
            }
            for (_, v) in map {
                collect_mcp_components(v, bindings);
            }
        }
        Value::Sequence(seq) => {
            for item in seq {
                collect_mcp_components(item, bindings);
            }
        }
        _ => {}
    }
}

fn extract_placeholders(value: &str, prefix: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut start = 0;
    while let Some(idx) = value[start..].find(prefix) {
        let idx = start + idx + prefix.len();
        let rest = &value[idx..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if end > 0 {
            results.push(rest[..end].to_string());
        }
        start = idx + end;
    }
    results
}

fn extract_origin(text: &str) -> Option<String> {
    if let Ok(url) = Url::parse(text)
        && url.scheme().starts_with("http")
        && let Some(host) = url.host_str()
    {
        let port = match url.port() {
            Some(p) => format!(":{}", p),
            None => "".to_string(),
        };
        return Some(format!("{}://{}{}", url.scheme(), host, port));
    }
    None
}

pub fn generate_bindings(
    metadata: &PackMetadata,
    opts: GeneratorOptions,
) -> Result<GeneratedBindings> {
    let mut env = base_env_passthrough();
    env.extend(metadata.hints.env.passthrough.iter().cloned());
    let flows: Vec<FlowHint> = metadata.flows.iter().map(FlowHint::from_flow).collect();
    for flow in &flows {
        env.extend(flow.env.clone());
    }

    let mut env = unique_sorted(env);

    let mut secrets = metadata.hints.secrets.required.clone();
    for flow in &flows {
        secrets.extend(flow.secrets.clone());
    }
    let secrets = unique_sorted(secrets);

    let mut network = metadata.hints.network.allow.clone();
    for flow in &flows {
        network.extend(flow.urls.clone());
    }
    let mut network = unique_sorted(network);

    if opts.complete && network.is_empty() {
        network.push("https://*".to_string());
    }

    if opts.complete {
        for value in base_env_passthrough() {
            if !env.contains(&value) {
                env.push(value);
            }
        }
    }

    let mut mcp_servers = metadata.hints.mcp.servers.clone();
    let mut referenced_components = Vec::new();
    for flow in &flows {
        referenced_components.extend(flow.mcp_components.clone());
    }
    let referenced_components = unique_sorted(referenced_components);
    for component in referenced_components {
        if mcp_servers.iter().any(|server| server.name == component) {
            continue;
        }
        if opts.strict {
            bail!(
                "MCP component '{}' referenced but no server hint; add hints or rerun with --complete",
                component
            );
        }
        mcp_servers.push(McpServer {
            name: component.clone(),
            transport: "websocket".into(),
            endpoint: "ws://localhost:9000".into(),
            caps: Vec::new(),
        });
    }

    if opts.strict {
        if opts.component.as_ref().map(|c| c.http).unwrap_or(false) && network.is_empty() {
            bail!(
                "HTTP capability detected but no network allow rules; add hints or use --complete"
            );
        }
        if opts.component.as_ref().map(|c| c.secrets).unwrap_or(false) && secrets.is_empty() {
            bail!(
                "Secrets capability detected but no secrets.required hints; add hints or use --complete"
            );
        }
    }

    Ok(GeneratedBindings {
        tenant: metadata.name.clone(),
        env_passthrough: env,
        network_allow: network,
        secrets_required: secrets,
        flows,
        mcp_servers,
    })
}

fn base_env_passthrough() -> Vec<String> {
    vec![
        "RUST_LOG".into(),
        "OTEL_EXPORTER_OTLP_ENDPOINT".into(),
        "OTEL_RESOURCE_ATTRIBUTES".into(),
    ]
}

fn unique_sorted(values: Vec<String>) -> Vec<String> {
    let mut set = HashSet::new();
    let mut result = Vec::new();
    for v in values {
        if set.insert(v.clone()) {
            result.push(v);
        }
    }
    result.sort_unstable();
    result
}

fn find_meta_bindings(meta: &Value) -> FlowBindings {
    let mut hints = FlowBindings::default();
    if let Some(bindings) = find_bindings_value(meta) {
        hints.urls.extend(extract_string_list(bindings, "urls"));
        hints
            .secrets
            .extend(extract_string_list(bindings, "secrets"));
        hints.env.extend(extract_string_list(bindings, "env"));
        hints
            .mcp_components
            .extend(extract_string_list(bindings, "mcp_components"));
    }
    hints
}

fn find_bindings_value(meta: &Value) -> Option<&Value> {
    if let Value::Mapping(map) = meta {
        let bindings_key = Value::String("bindings".into());
        if let Some(bindings) = map.get(&bindings_key) {
            return Some(bindings);
        }
        let meta_key = Value::String("meta".into());
        if let Some(Value::Mapping(inner_map)) = map.get(&meta_key) {
            let inner_bindings_key = Value::String("bindings".into());
            return inner_map.get(&inner_bindings_key);
        }
    }
    None
}

fn extract_string_list(bindings: &Value, key: &str) -> Vec<String> {
    if let Value::Mapping(map) = bindings {
        let key_value = Value::String(key.into());
        if let Some(value) = map.get(&key_value) {
            return value_to_list(value);
        }
    }
    Vec::new()
}

fn value_to_list(value: &Value) -> Vec<String> {
    match value {
        Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}
