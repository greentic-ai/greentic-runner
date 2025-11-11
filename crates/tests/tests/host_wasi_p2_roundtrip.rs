use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use greentic_runner_host::imports;
use greentic_runner_host::pack::{ComponentState, HostState};
use greentic_runner_host::runtime_wasmtime::{Component, Engine, Linker, Store};
use greentic_runner_host::{HostConfig, PreopenSpec, RunnerWasiPolicy};
use serial_test::serial;
use tempfile::TempDir;

#[test]
#[serial]
fn wasi_preview2_policy_enforced() -> Result<()> {
    if is_offline() {
        eprintln!("skipping wasm fixture test in offline mode");
        return Ok(());
    }
    let wasm_path = match build_fixture() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("skipping wasm fixture test (fixture build failed: {err:?})");
            return Ok(());
        }
    };
    let workspace = workspace_root();
    let bindings = workspace.join("examples/bindings/default.bindings.yaml");
    let config = Arc::new(HostConfig::load_from_path(&bindings)?);

    let tempdir = TempDir::new()?;
    let data_file = tempdir.path().join("hello.txt");
    fs::write(&data_file, "wasi preview-2 preopen")?;
    let _env = EnvGuard::set("GT_TEST", "ok");

    let base_policy = RunnerWasiPolicy::new()
        .allow_env("GT_TEST")
        .with_preopen(PreopenSpec::new(tempdir.path(), "/data").read_only(true));
    run_component(&wasm_path, Arc::clone(&config), base_policy.clone())?;

    let missing_env = RunnerWasiPolicy::new()
        .with_preopen(PreopenSpec::new(tempdir.path(), "/data").read_only(true));
    assert!(run_component(&wasm_path, Arc::clone(&config), missing_env).is_err());

    let missing_preopen = RunnerWasiPolicy::new().allow_env("GT_TEST");
    assert!(run_component(&wasm_path, config, missing_preopen).is_err());

    Ok(())
}

fn is_offline() -> bool {
    matches!(
        std::env::var("CARGO_NET_OFFLINE"),
        Ok(val) if val == "true" || val == "1"
    )
}

fn run_component(wasm: &Path, config: Arc<HostConfig>, policy: RunnerWasiPolicy) -> Result<()> {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm)
        .with_context(|| format!("failed to load {}", wasm.display()))?;
    let host_state = HostState::new(Arc::clone(&config), None, None, None)?;
    let store_state = ComponentState::new(host_state, Arc::new(policy))?;
    let mut store = Store::new(&engine, store_state);
    let mut linker = Linker::new(&engine);
    imports::register_all(&mut linker)?;
    let instance = linker.instantiate(&mut store, &component)?;
    let run = instance
        .get_typed_func::<(), ()>(&mut store, "wasi:cli/run#run")
        .context("component missing cli run export")?;
    run.call(&mut store, ())
        .context("component execution failed")?;
    Ok(())
}

fn build_fixture() -> Result<PathBuf> {
    let workspace = workspace_root();
    let manifest = workspace.join("tests/fixtures/wasi-p2-smoke/Cargo.toml");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            manifest.to_str().expect("utf8 path"),
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .status()
        .context("failed to build wasi fixture")?;
    if !status.success() {
        bail!("wasi fixture build failed")
    }
    let artifact = workspace
        .join("tests/fixtures/wasi-p2-smoke/target/wasm32-wasip2/release/wasi_p2_smoke.wasm");
    if !artifact.exists() {
        bail!("wasi fixture artifact missing: {}", artifact.display());
    }
    Ok(artifact)
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .expect("tests crate to live under crates/")
}

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.prev {
            unsafe {
                std::env::set_var(self.key, value);
            }
        } else {
            unsafe {
                std::env::remove_var(self.key);
            }
        }
    }
}
