use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Write, copy};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result};
use greentic_runner_host::pack::{ComponentResolution, PackRuntime};
use greentic_runner_host::runner::contract_introspection::introspect_component_contract;
use greentic_runner_host::{
    RunnerWasiPolicy,
    config::{HostConfig, OperatorPolicy, SecretsPolicy},
    secrets::default_manager,
    storage::{new_session_store, new_state_store},
    trace::TraceConfig,
    validate::ValidationConfig,
};
use greentic_types::{
    ComponentCapabilities, ComponentManifest, ComponentProfiles, PackKind, PackManifest,
    ResourceHints, encode_pack_manifest,
};
use semver::Version;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::FileOptions;

#[tokio::test]
async fn introspect_component_v06_contract_from_wasm_describe() -> Result<()> {
    let workspace = TempDir::new()?;
    let config = minimal_config(workspace.path())?;
    let pack_path = workspace.path().join("component-v06.gtpack");
    let component_path = build_component_v06_fixture()?;
    build_component_pack_v06(&component_path, &pack_path)?;

    let pack = Arc::new(
        PackRuntime::load(
            &pack_path,
            Arc::clone(&config),
            None,
            Some(&pack_path),
            Some(new_session_store()),
            Some(new_state_store()),
            Arc::new(RunnerWasiPolicy::new()),
            default_manager()?,
            None,
            false,
            ComponentResolution::default(),
        )
        .await?,
    );

    let contract = introspect_component_contract(pack.as_ref(), "v06.describe", "run")?
        .context("expected 0.6 contract introspection to return a contract")?;
    assert_eq!(contract.selected_operation, "run");
    assert!(contract.input_schema.is_object());
    assert!(contract.output_schema.is_object());
    assert!(contract.config_schema.is_object());
    assert!(contract.describe_hash.starts_with("sha256:"));
    assert!(contract.schema_hash.starts_with("sha256:"));
    Ok(())
}

fn minimal_config(workspace: &Path) -> Result<Arc<HostConfig>> {
    let bindings_path = workspace.join("bindings.yaml");
    std::fs::write(
        &bindings_path,
        r#"
tenant: demo
flow_type_bindings: {}
rate_limits: {}
retry: {}
timers: []
"#,
    )?;
    let mut config =
        HostConfig::load_from_path(&bindings_path).context("load minimal host bindings")?;
    config.secrets_policy = SecretsPolicy::allow_all();
    config.operator_policy = OperatorPolicy::allow_all();
    config.trace = TraceConfig::from_env();
    config.validation = ValidationConfig::from_env();
    Ok(Arc::new(config))
}

fn build_component_pack_v06(component_path: &Path, pack_path: &Path) -> Result<()> {
    let manifest = PackManifest {
        schema_version: "1.0".into(),
        pack_id: "component.v06".parse()?,
        name: Some("component.v06".into()),
        version: Version::parse("0.1.0")?,
        kind: PackKind::Application,
        publisher: "test".into(),
        components: vec![ComponentManifest {
            id: "v06.describe".parse()?,
            version: Version::parse("0.1.0")?,
            supports: Vec::new(),
            world: "greentic:component@0.6.0".into(),
            profiles: ComponentProfiles::default(),
            capabilities: ComponentCapabilities::default(),
            configurators: None,
            operations: Vec::new(),
            config_schema: None,
            resources: ResourceHints::default(),
            dev_flows: BTreeMap::new(),
        }],
        flows: Vec::new(),
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        signatures: Default::default(),
        secret_requirements: Vec::new(),
        bootstrap: None,
        extensions: None,
    };

    let mut writer = ZipWriter::new(File::create(pack_path).context("create v0.6 pack archive")?);
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let manifest_bytes = encode_pack_manifest(&manifest)?;
    writer.start_file("manifest.cbor", options)?;
    writer.write_all(&manifest_bytes)?;

    writer.start_file("components/v06.describe.wasm", options)?;
    let mut component_file =
        File::open(component_path).with_context(|| format!("Open {:?}", component_path))?;
    copy(&mut component_file, &mut writer)?;
    writer.finish().context("finalise v0.6 pack")?;
    Ok(())
}

fn build_component_v06_fixture() -> Result<PathBuf> {
    let root = fixture_path("tests/assets/component-v0-6-dummy");
    let workspace_wasm =
        fixture_path("target/wasm32-wasip2/release/component_v0_6_dummy.wasm");
    let fixture_wasm = root.join("target/wasm32-wasip2/release/component_v0_6_dummy.wasm");
    let wasm = if workspace_wasm.exists() {
        workspace_wasm
    } else {
        fixture_wasm
    };
    if !wasm.exists() {
        let offline = std::env::var("CARGO_NET_OFFLINE").ok();
        let mut cmd = Command::new("cargo");
        let mut args: Vec<String> = vec![
            "build".into(),
            "--release".into(),
            "--target".into(),
            "wasm32-wasip2".into(),
            "--manifest-path".into(),
            root.join("Cargo.toml")
                .to_str()
                .expect("manifest path")
                .into(),
        ];
        if matches!(offline.as_deref(), Some("true")) {
            args.insert(1, "--offline".into());
        }
        if let Some(val) = &offline {
            cmd.env("CARGO_NET_OFFLINE", val);
        }
        let status = cmd.args(&args).status().context("build v0.6 component")?;
        if !status.success() {
            anyhow::bail!("failed to build component-v0-6 fixture");
        }
    }
    Ok(wasm)
}

fn fixture_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join(relative)
}
