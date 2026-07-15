use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use greentic_interfaces_wasmtime::host_helpers::v1::oauth_broker::OAuthBrokerHost;
use greentic_runner_host::config::HostConfig;
use greentic_runner_host::oauth::OAuthBrokerConfig;
use greentic_runner_host::pack::{self, ComponentState, HostState};
use greentic_runner_host::runtime_wasmtime::{Component, Engine, Linker, Store};
use greentic_runner_host::secrets::default_manager;
use greentic_runner_host::wasi::RunnerWasiPolicy;
use reqwest::blocking::Client as BlockingClient;
use tempfile::NamedTempFile;

// -----------------------------------------------------------------------
// Host trait direct tests — `get_token` / `get_consent_url` / `exchange_code`
//
// WHY there is no success-path / bearer-on-wire test here:
//
// `request_resource_token_blocking` validates that `http_base_url` is HTTPS —
// it will reject any `http://` URL.  The only HTTP test double available in
// this crate is raw TCP stubs (or `wiremock = "0.6"`, which serves plain HTTP).
// Setting up a real TLS listener with a self-signed cert and injecting it into
// reqwest's root-store is disproportionate test infrastructure for a slim proxy
// shim, and weakening the HTTPS guard just to make a test pass would be wrong.
//
// The success path (real POST → admin `/resource-token`, bearer header received)
// is covered by:
//   • code review of the three-line proxy in `pack.rs` (trivially correct),
//   • the admin-side wiremock test of `/resource-token` (Area A), and
//   • the `blocking_request_sends_bearer_header_when_secret_provided` unit test
//     in `oauth.rs` which proves bearer attachment at the `reqwest` call site.
// -----------------------------------------------------------------------

/// Helper: build a `HostState` without an oauth_config.
fn make_host_state_no_oauth() -> Result<HostState> {
    let cfg = load_host_config(false)?;
    HostState::new(
        "test-no-oauth".to_string(),
        Arc::clone(&cfg),
        Arc::new(BlockingClient::builder().build()?),
        None,
        None,
        None,
        default_manager()?,
        None, // no oauth_config
        None,
        None,
        false,
        None,
        None,
    )
}

/// Helper: build a `HostState` with an oauth_config whose `http_base_url` uses
/// the `http` scheme — this will cause `request_resource_token_blocking` to
/// reject the URL and return an error, exercising the error→empty-string path.
fn make_host_state_http_oauth() -> Result<HostState> {
    let cfg = load_host_config(false)?;
    let mut oauth = OAuthBrokerConfig::new("http://admin.example/", "nats://localhost:4222");
    oauth.shared_secret = Some("s3cr3t".to_string());
    HostState::new(
        "test-http-oauth".to_string(),
        Arc::clone(&cfg),
        Arc::new(BlockingClient::builder().build()?),
        None,
        None,
        None,
        default_manager()?,
        Some(oauth),
        None,
        None,
        false,
        None,
        None,
    )
}

#[test]
fn oauth_get_token_returns_empty_when_config_absent() -> Result<()> {
    let mut host = make_host_state_no_oauth()?;
    let result = OAuthBrokerHost::get_token(
        &mut host,
        "demo".to_string(),
        "subject".to_string(),
        vec!["scope".to_string()],
    );
    assert_eq!(
        result, "",
        "get_token must return empty string when oauth_config is None"
    );
    Ok(())
}

#[test]
fn oauth_get_token_returns_empty_on_http_url_validation_failure() -> Result<()> {
    // The http_base_url "http://…" is rejected by the HTTPS-only guard inside
    // `request_resource_token_blocking`.  The impl logs a warning and returns "".
    // This test verifies that the error→empty-string path is reached without
    // panic or secret leakage.
    let mut host = make_host_state_http_oauth()?;
    let result = OAuthBrokerHost::get_token(
        &mut host,
        "demo".to_string(),
        "subject".to_string(),
        vec!["scope".to_string()],
    );
    assert_eq!(
        result, "",
        "get_token must return empty string when base URL fails HTTPS validation"
    );
    Ok(())
}

#[test]
fn oauth_get_consent_url_returns_empty() -> Result<()> {
    // Operator-time flow — not supported at runtime; always returns "".
    let mut host = make_host_state_no_oauth()?;
    let result = OAuthBrokerHost::get_consent_url(
        &mut host,
        "demo".to_string(),
        "subject".to_string(),
        vec!["scope".to_string()],
        "/callback".to_string(),
        "{}".to_string(),
    );
    assert_eq!(
        result, "",
        "get_consent_url must return empty string (operator-time only)"
    );
    Ok(())
}

#[test]
fn oauth_exchange_code_returns_empty() -> Result<()> {
    // Operator-time flow — not supported at runtime; always returns "".
    let mut host = make_host_state_no_oauth()?;
    let result = OAuthBrokerHost::exchange_code(
        &mut host,
        "demo".to_string(),
        "subject".to_string(),
        "auth-code-xyz".to_string(),
        "/callback".to_string(),
    );
    assert_eq!(
        result, "",
        "exchange_code must return empty string (operator-time only)"
    );
    Ok(())
}

#[test]
fn oauth_world_instantiates_when_enabled() -> Result<()> {
    let wasm = build_fixture()?;
    let host_cfg = load_host_config(true)?;
    instantiate_component(&wasm, host_cfg).context("component should instantiate")
}

fn instantiate_component(wasm: &Path, config: Arc<HostConfig>) -> Result<()> {
    let engine = Engine::default();
    let component = Component::from_file(&engine, wasm)
        .map_err(|err| anyhow!("failed to load {}: {err}", wasm.display()))?;
    let host_state = HostState::new(
        "oauth-test".to_string(),
        Arc::clone(&config),
        Arc::new(BlockingClient::builder().build()?),
        None,
        None,
        None,
        default_manager()?,
        config.oauth_broker_config(),
        None,
        None,
        false,
        None,
        None,
    )?;
    let policy = Arc::new(RunnerWasiPolicy::default());
    let state = ComponentState::new(host_state, policy)?;
    let mut store = Store::new(&engine, state);
    let mut linker = Linker::new(&engine);
    pack::register_all(&mut linker, false)?;
    linker
        .instantiate(&mut store, &component)
        .map_err(|err| anyhow!("component instantiation failed: {err}"))?;
    Ok(())
}

fn load_host_config(enable_oauth: bool) -> Result<Arc<HostConfig>> {
    let file = NamedTempFile::new()?;
    let oauth_block = if enable_oauth {
        r#"
oauth:
  http_base_url: "https://oauth.example"
  nats_url: "nats://localhost:4222"
  provider: "demo"
"#
    } else {
        ""
    };
    let contents = format!(
        r#"
tenant: test-tenant
flow_type_bindings: {{}}
mcp:
  store: {{}}
  security: {{}}
  runtime: {{}}
rate_limits: {{}}
timers: []
{oauth_block}
"#
    );
    fs::write(file.path(), contents)?;
    let cfg = HostConfig::load_from_path(file.path())?;
    Ok(Arc::new(cfg))
}

fn build_fixture() -> Result<PathBuf> {
    let workspace = workspace_root();
    let manifest = workspace.join("tests/fixtures/oauth-broker-component/Cargo.toml");
    let target_dir = workspace.join("target/oauth-broker-fixture");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            manifest
                .to_str()
                .ok_or_else(|| anyhow!("fixture manifest path not valid utf-8"))?,
            "--target",
            "wasm32-wasip2",
            "--release",
            "--target-dir",
            target_dir
                .to_str()
                .ok_or_else(|| anyhow!("fixture target dir not valid utf-8"))?,
        ])
        .status()
        .context("failed to build oauth broker fixture")?;
    if !status.success() {
        anyhow::bail!("failed to build oauth broker fixture");
    }
    Ok(target_dir.join("wasm32-wasip2/release/oauth_broker_component.wasm"))
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir")
        .parent()
        .expect("workspace dir")
        .to_path_buf()
}
