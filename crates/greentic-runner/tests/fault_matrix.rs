#![cfg(feature = "fault-injection")]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use greentic_flow::flow_bundle::load_and_validate_bundle_with_flow;
use greentic_types::{
    ComponentCapabilities, ComponentManifest, ComponentProfiles, FlowKind, HostCapabilities,
    PackFlowEntry, PackKind, PackManifest, ResourceHints, StateCapabilities, encode_pack_manifest,
};
use semver::Version;
use serial_test::serial;
use tempfile::TempDir;
use zip::ZipWriter;
use zip::write::FileOptions;

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<str>) -> Self {
        let prev = env::var(key).ok();
        unsafe {
            env::set_var(key, value.as_ref());
        }
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(ref value) = self.prev {
            unsafe {
                env::set_var(self.key, value);
            }
        } else {
            unsafe {
                env::remove_var(self.key);
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn conformance_fault_matrix_passes() -> Result<()> {
    let _backend_guard = EnvGuard::set("SECRETS_BACKEND", "env");
    ensure_state_store_component()?;
    let temp = TempDir::new()?;
    let packs_dir = temp.path().join("packs");
    fs::create_dir_all(&packs_dir)?;
    let pack_path = packs_dir.join("messaging-conformance.gtpack");
    build_conformance_pack(&pack_path)?;

    let report_path = temp.path().join("report.json");
    let faults_path = fixture_path("tests/fixtures/faults/basic.json");
    run_fault_conformance(&packs_dir, &faults_path, &report_path)?;

    let report = fs::read(&report_path).context("read report")?;
    let value: serde_json::Value = serde_json::from_slice(&report)?;
    assert_eq!(value["summary"]["failed"], 0);
    Ok(())
}

fn run_fault_conformance(packs_dir: &Path, faults_path: &Path, report: &Path) -> Result<()> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_greentic-runner"));
    cmd.arg("conformance")
        .arg("--packs")
        .arg(packs_dir)
        .arg("--level")
        .arg("l2")
        .arg("--faults")
        .arg(faults_path)
        .arg("--report")
        .arg(report);
    let status = cmd.status().context("run conformance faults")?;
    if !status.success() {
        anyhow::bail!("conformance faults command failed");
    }
    Ok(())
}

fn build_conformance_pack(pack_path: &Path) -> Result<()> {
    let flow_yaml = r#"
id: demo.flow
type: messaging
start: write
nodes:
  write:
    component.exec:
      component: state.store
      operation: write
      input:
        key: "conformance-key"
        value:
          status: "ok"
    routing:
      - to: read
  read:
    component.exec:
      component: state.store
      operation: read
      input:
        key: "conformance-key"
    routing:
      - out: true
"#;
    let (_bundle, flow) = load_and_validate_bundle_with_flow(flow_yaml, None)?;

    let capabilities = ComponentCapabilities {
        host: HostCapabilities {
            state: Some(StateCapabilities {
                read: true,
                write: true,
            }),
            ..HostCapabilities::default()
        },
        ..ComponentCapabilities::default()
    };

    let manifest = PackManifest {
        agents: Default::default(),
        schema_version: "1.0".into(),
        pack_id: "messaging-conformance".parse()?,
        name: None,
        version: Version::parse("0.0.0")?,
        kind: PackKind::Application,
        publisher: "test".into(),
        components: vec![ComponentManifest {
            id: "state.store".parse()?,
            version: Version::parse("0.1.0")?,
            supports: vec![FlowKind::Messaging],
            world: "greentic:component@0.4.0".into(),
            profiles: ComponentProfiles::default(),
            capabilities,
            configurators: None,
            operations: Vec::new(),
            config_schema: None,
            resources: ResourceHints::default(),
            dev_flows: BTreeMap::new(),
        }],
        flows: vec![PackFlowEntry {
            id: flow.id.clone(),
            kind: flow.kind,
            flow: flow.clone(),
            tags: Vec::new(),
            entrypoints: vec!["default".into()],
        }],
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        signatures: Default::default(),
        secret_requirements: Vec::new(),
        bootstrap: None,
        extensions: None,
    };

    let mut writer =
        ZipWriter::new(fs::File::create(pack_path).context("create conformance pack")?);
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let manifest_bytes = encode_pack_manifest(&manifest)?;
    writer.start_file("manifest.cbor", options)?;
    writer.write_all(&manifest_bytes)?;

    let wasm_path = fixture_path(
        "tests/fixtures/runner-components/target-test/wasm32-wasip2/release/state_store_component.wasm",
    );
    writer.start_file("components/state.store.wasm", options)?;
    let mut file = fs::File::open(&wasm_path as &Path)
        .with_context(|| format!("open component {}", wasm_path.display()))?;
    io::copy(&mut file, &mut writer)?;
    writer.finish().context("finalise conformance pack")?;
    Ok(())
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative)
}

fn ensure_state_store_component() -> Result<PathBuf> {
    let crates_root = fixture_path("tests/fixtures/runner-components");
    let target_root = crates_root.join("target-test");
    let crate_name = "state_store_component";
    let base = target_root.join("wasm32-wasip2").join("release");
    let candidates = [
        base.join(format!("{crate_name}.wasm")),
        base.join("deps").join(format!("{crate_name}.wasm")),
    ];
    if let Some(found) = candidates.iter().find(|p| p.exists()) {
        return Ok(found.clone());
    }

    let crate_dir = crates_root.join(crate_name);
    let status = Command::new("cargo")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", &target_root)
        // wasm32-wasip2 has no `profiler_builtins`; strip any inherited
        // coverage instrumentation flags so the spawned build does not try
        // to link it under `cargo llvm-cov`.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("LLVM_PROFILE_FILE")
        .current_dir(&crate_dir)
        .args([
            "build",
            "--offline",
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .status()
        .with_context(|| format!("failed to build component crate {crate_name}"))?;
    if !status.success() {
        anyhow::bail!("component build failed for {crate_name}");
    }

    candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("component artifact not found after build for {crate_name}"))
}
