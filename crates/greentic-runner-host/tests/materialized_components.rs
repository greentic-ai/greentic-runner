use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use greentic_pack::builder as legacy_pack;
use greentic_runner_host::config::{
    FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
    WebhookPolicy,
};
use greentic_runner_host::pack::{ComponentResolution, PackRuntime};
use greentic_runner_host::secrets::default_manager;
use greentic_runner_host::trace::TraceConfig;
use greentic_runner_host::validate::ValidationConfig;
use greentic_runner_host::wasi::RunnerWasiPolicy;
use greentic_types::decode_pack_manifest;
use serde::Deserialize;
use serde_yaml_bw as serde_yaml;
use tempfile::TempDir;
use tokio::runtime::Runtime;
use zip::CompressionMethod;
use zip::ZipArchive;
use zip::ZipWriter;
use zip::write::FileOptions;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn host_config(bindings_path: &Path) -> HostConfig {
    HostConfig {
        tenant: "demo".into(),
        bindings_path: bindings_path.to_path_buf(),
        flow_type_bindings: HashMap::new(),
        rate_limits: RateLimits::default(),
        retry: FlowRetryConfig::default(),
        http_enabled: false,
        secrets_policy: SecretsPolicy::allow_all(),
        state_store_policy: StateStorePolicy::default(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
        oauth: None,
        mocks: None,
        pack_bindings: Vec::new(),
        env_passthrough: Vec::new(),
        trace: TraceConfig::from_env(),
        validation: ValidationConfig::from_env(),
        operator_policy: OperatorPolicy::allow_all(),
        #[cfg(feature = "agentic-worker")]
        agents: std::collections::HashMap::new(),
        #[cfg(feature = "agentic-worker")]
        graphs: std::collections::HashMap::new(),
    }
}

#[derive(Clone)]
struct ComponentInfo {
    id: String,
    legacy_path: Option<String>,
}

#[derive(Clone)]
struct FlowInfo {
    json_path: String,
    yaml_source: Option<String>,
}

struct FixtureManifest {
    bytes: Vec<u8>,
    components: Vec<ComponentInfo>,
    flows: Vec<FlowInfo>,
}

fn fixtures_pack_root() -> PathBuf {
    workspace_root().join("tests/fixtures/packs/runner-components")
}

fn read_fixture_manifest_bytes() -> Result<Vec<u8>> {
    let manifest_path = fixtures_pack_root().join("manifest.cbor");
    match fs::read(&manifest_path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let archive_path = fixtures_pack_root().join("runner-components.gtpack");
            let file = fs::File::open(&archive_path).with_context(|| {
                format!("failed to open fixture archive {}", archive_path.display())
            })?;
            let mut archive = ZipArchive::new(file)?;
            let mut entry = archive
                .by_name("manifest.cbor")
                .context("missing manifest.cbor in fixture archive")?;
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        Err(err) => Err(err.into()),
    }
}

#[derive(Deserialize)]
struct FixtureManifestJson {
    components: Vec<FixtureComponentJson>,
    flows: Vec<FixtureFlowJson>,
}

#[derive(Deserialize)]
struct FixtureComponentJson {
    name: String,
    file_wasm: String,
}

#[derive(Deserialize)]
struct FixtureFlowJson {
    id: String,
    file_json: String,
}

#[derive(Deserialize)]
struct FixturePackYaml {
    flows: Vec<FixturePackFlowYaml>,
}

#[derive(Deserialize)]
struct FixturePackFlowYaml {
    id: String,
    file_yaml: String,
}

fn load_pack_yaml_flows() -> Result<HashMap<String, String>> {
    let path = fixtures_pack_root().join("pack.yaml");
    let pack: FixturePackYaml = serde_yaml::from_slice(&fs::read(&path)?)?;
    Ok(pack
        .flows
        .into_iter()
        .map(|flow| (flow.id, flow.file_yaml))
        .collect())
}

fn load_manifest_json(
    yaml_flows: &HashMap<String, String>,
) -> Result<(Vec<ComponentInfo>, Vec<FlowInfo>)> {
    let archive_path = fixtures_pack_root().join("runner-components.gtpack");
    let mut archive =
        ZipArchive::new(fs::File::open(&archive_path).with_context(|| {
            format!("failed to open fixture archive {}", archive_path.display())
        })?)?;
    let mut manifest_entry = archive
        .by_name("manifest.json")
        .context("missing manifest.json in fixture archive")?;
    let mut contents = String::new();
    manifest_entry.read_to_string(&mut contents)?;

    let manifest: FixtureManifestJson = serde_json::from_str(&contents)?;
    let components = manifest
        .components
        .into_iter()
        .map(|component| ComponentInfo {
            id: component.name,
            legacy_path: Some(component.file_wasm),
        })
        .collect();
    let flows = manifest
        .flows
        .into_iter()
        .map(|flow| FlowInfo {
            json_path: flow.file_json,
            yaml_source: yaml_flows.get(&flow.id).cloned(),
        })
        .collect();
    Ok((components, flows))
}

fn load_fixture_manifest() -> Result<FixtureManifest> {
    let bytes = read_fixture_manifest_bytes()?;

    let yaml_flows = load_pack_yaml_flows().unwrap_or_default();

    if let Ok((components, flows)) = load_manifest_json(&yaml_flows) {
        return Ok(FixtureManifest {
            bytes,
            components,
            flows,
        });
    }

    if let Ok(manifest) = decode_pack_manifest(&bytes) {
        return Ok(FixtureManifest {
            bytes,
            components: manifest
                .components
                .into_iter()
                .map(|component| ComponentInfo {
                    id: component.id.as_str().to_string(),
                    legacy_path: None,
                })
                .collect(),
            flows: Vec::new(),
        });
    }

    let legacy: legacy_pack::PackManifest = serde_cbor::from_slice(&bytes)
        .map_err(|err| anyhow!("failed to decode fixture manifest: {err}"))?;
    let flows = legacy
        .flows
        .iter()
        .map(|flow| FlowInfo {
            json_path: flow.file_json.clone(),
            yaml_source: yaml_flows.get(&flow.id).cloned(),
        })
        .collect();
    Ok(FixtureManifest {
        bytes,
        components: legacy
            .components
            .into_iter()
            .map(|component| ComponentInfo {
                id: component.name,
                legacy_path: Some(component.file_wasm),
            })
            .collect(),
        flows,
    })
}

fn component_path(component: &ComponentInfo) -> String {
    component
        .legacy_path
        .clone()
        .unwrap_or_else(|| format!("components/{}.wasm", component.id))
}

fn copy_fixture_components(root: &Path, components: &[ComponentInfo]) -> Result<()> {
    let fixtures = fixtures_pack_root().join("components");
    let archive_path = fixtures_pack_root().join("runner-components.gtpack");
    for component in components {
        let relative_path = component_path(component);
        let target = root.join(&relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let source_file = component
            .legacy_path
            .as_ref()
            .and_then(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| format!("{}.wasm", component.id.replace('.', "_")));
        let mut source = fixtures.join(&source_file);
        if !source.exists() {
            source = fixtures.join(format!("{}.wasm", component.id.replace('.', "_")));
        }
        if source.exists() {
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "failed to copy fixture component {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
            continue;
        }

        // CI does not check in the components dir; fall back to the archive payload.
        let file = fs::File::open(&archive_path).with_context(|| {
            format!(
                "failed to open fixture archive {} to read {}",
                archive_path.display(),
                relative_path
            )
        })?;
        let mut archive = ZipArchive::new(file)?;
        let mut entry = archive.by_name(&relative_path).with_context(|| {
            format!(
                "missing fixture component {} in archive {}",
                relative_path,
                archive_path.display()
            )
        })?;
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        fs::write(&target, &bytes)?;
    }
    Ok(())
}

fn read_flow_file(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    if let Some(ext) = path.extension().and_then(|ext| ext.to_str())
        && matches!(ext, "yaml" | "yml")
    {
        let doc: serde_yaml::Value = serde_yaml::from_slice(&bytes)?;
        return serde_json::to_vec(&doc).context("failed to convert flow yaml to json");
    }
    Ok(bytes)
}

fn flow_entries(flows: &[FlowInfo]) -> Result<Vec<(String, Vec<u8>)>> {
    let fixtures_root = fixtures_pack_root();
    let archive_path = fixtures_root.join("runner-components.gtpack");
    let mut archive =
        ZipArchive::new(fs::File::open(&archive_path).with_context(|| {
            format!("failed to open fixture archive {}", archive_path.display())
        })?)?;

    let mut entries = Vec::new();
    for flow in flows {
        let disk_path = fixtures_root.join(&flow.json_path);
        let bytes = if let Some(yaml_source) = &flow.yaml_source {
            read_flow_file(&fixtures_root.join(yaml_source))?
        } else if disk_path.exists() {
            read_flow_file(&disk_path)?
        } else {
            let mut file = archive
                .by_name(&flow.json_path)
                .with_context(|| format!("missing fixture flow {}", flow.json_path))?;
            let mut data = Vec::new();
            file.read_to_end(&mut data)?;
            data
        };
        entries.push((flow.json_path.clone(), bytes));
    }
    Ok(entries)
}

fn materialize_flows(root: &Path, entries: &[(String, Vec<u8>)]) -> Result<()> {
    for (path, bytes) in entries {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, bytes)?;
    }
    Ok(())
}

fn write_archive(
    path: &Path,
    manifest: &FixtureManifest,
    components: &[(String, Vec<u8>)],
    flows: &[(String, Vec<u8>)],
) -> Result<()> {
    let file = fs::File::create(path)
        .with_context(|| format!("failed to create archive {}", path.display()))?;
    let mut writer = ZipWriter::new(file);
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(CompressionMethod::Stored);

    writer.start_file("manifest.cbor", options)?;
    writer.write_all(&manifest.bytes)?;

    for (path, bytes) in components {
        writer.start_file(path, options)?;
        writer.write_all(bytes)?;
    }

    for (path, bytes) in flows {
        writer.start_file(path, options)?;
        writer.write_all(bytes)?;
    }

    writer.finish()?;
    Ok(())
}

#[test]
fn prefers_materialized_components_over_archive() -> Result<()> {
    let rt = Runtime::new().context("test runtime")?;
    let temp = TempDir::new()?;
    let manifest = load_fixture_manifest()?;
    let flow_entries = flow_entries(&manifest.flows)?;

    let materialized_root = temp.path().join("materialized");
    fs::create_dir_all(&materialized_root)?;
    fs::write(materialized_root.join("manifest.cbor"), &manifest.bytes)?;
    copy_fixture_components(&materialized_root, &manifest.components)?;
    materialize_flows(&materialized_root, &flow_entries)?;

    let invalid_archive = temp.path().join("invalid.gtpack");
    let invalid_components: Vec<(String, Vec<u8>)> = manifest
        .components
        .iter()
        .map(|info| (component_path(info), b"not-wasm".to_vec()))
        .collect();
    write_archive(
        &invalid_archive,
        &manifest,
        &invalid_components,
        &flow_entries,
    )?;

    let bindings_path = temp.path().join("bindings.yaml");
    fs::write(&bindings_path, b"tenant: demo")?;
    let config = Arc::new(host_config(&bindings_path));

    let pack = rt.block_on(PackRuntime::load(
        &invalid_archive,
        Arc::clone(&config),
        None,
        Some(invalid_archive.as_path()),
        None,
        None,
        Arc::new(RunnerWasiPolicy::new()),
        default_manager()?,
        None,
        false,
        ComponentResolution {
            materialized_root: Some(materialized_root.clone()),
            overrides: HashMap::new(),
            ..ComponentResolution::default()
        },
    ))?;

    let flows = rt.block_on(pack.list_flows())?;
    assert!(!flows.is_empty());
    Ok(())
}

#[test]
fn falls_back_to_archive_when_materialized_missing_components() -> Result<()> {
    let rt = Runtime::new().context("test runtime")?;
    let temp = TempDir::new()?;
    let manifest = load_fixture_manifest()?;
    let flow_entries = flow_entries(&manifest.flows)?;

    let materialized_root = temp.path().join("materialized");
    fs::create_dir_all(&materialized_root)?;
    fs::write(materialized_root.join("manifest.cbor"), &manifest.bytes)?;
    materialize_flows(&materialized_root, &flow_entries)?;

    // Do not copy components into the materialized directory to force archive fallback.
    let archive_path =
        workspace_root().join("tests/fixtures/packs/runner-components/runner-components.gtpack");

    let bindings_path = temp.path().join("bindings.yaml");
    fs::write(&bindings_path, b"tenant: demo")?;
    let config = Arc::new(host_config(&bindings_path));

    let pack = rt.block_on(PackRuntime::load(
        &materialized_root,
        Arc::clone(&config),
        None,
        Some(archive_path.as_path()),
        None,
        None,
        Arc::new(RunnerWasiPolicy::new()),
        default_manager()?,
        None,
        false,
        ComponentResolution {
            materialized_root: Some(materialized_root.clone()),
            overrides: HashMap::new(),
            ..ComponentResolution::default()
        },
    ))?;

    assert!(!rt.block_on(pack.list_flows())?.is_empty());
    assert!(!manifest.components.is_empty());
    Ok(())
}

#[test]
fn errors_when_components_missing_from_all_sources() -> Result<()> {
    let rt = Runtime::new().context("test runtime")?;
    let temp = TempDir::new()?;
    let manifest = load_fixture_manifest()?;
    let flow_entries = flow_entries(&manifest.flows)?;

    let materialized_root = temp.path().join("materialized");
    fs::create_dir_all(&materialized_root)?;
    fs::write(materialized_root.join("manifest.cbor"), &manifest.bytes)?;
    materialize_flows(&materialized_root, &flow_entries)?;

    // Archive contains only manifest to force missing-component error.
    let archive_path = temp.path().join("manifest-only.gtpack");
    let no_components: Vec<(String, Vec<u8>)> = Vec::new();
    write_archive(&archive_path, &manifest, &no_components, &flow_entries)?;

    let bindings_path = temp.path().join("bindings.yaml");
    fs::write(&bindings_path, b"tenant: demo")?;
    let config = Arc::new(host_config(&bindings_path));

    let err = rt
        .block_on(PackRuntime::load(
            &materialized_root,
            Arc::clone(&config),
            None,
            Some(archive_path.as_path()),
            None,
            None,
            Arc::new(RunnerWasiPolicy::new()),
            default_manager()?,
            None,
            false,
            ComponentResolution {
                materialized_root: Some(materialized_root.clone()),
                overrides: HashMap::new(),
                ..ComponentResolution::default()
            },
        ))
        .err()
        .expect("pack load should fail when components are missing everywhere");

    let message = err.to_string();
    assert!(
        message.contains("components missing"),
        "error should mention missing components: {message}"
    );
    assert!(
        manifest
            .components
            .iter()
            .any(|info| message.contains(&info.id)),
        "error should name a missing component: {message}"
    );
    Ok(())
}
