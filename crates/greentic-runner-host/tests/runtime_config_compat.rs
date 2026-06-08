//! C4.2 — `RuntimeConfigHost` compat-shim behavior.
//!
//! When the producer has not yet materialized a `PackConfig.non_secret`
//! map (C4.3), the runtime-config host falls back to the secrets-store
//! and logs a once-per-process-per-key deprecation warning. These tests
//! verify the three-arm decision tree: non_secret hit → no fallback,
//! non_secret miss + secrets-store hit → compat value returned, both
//! miss → `not-found`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use greentic_interfaces_wasmtime::host_helpers::v1::runtime_config::{
    ConfigError, RuntimeConfigHost,
};
use greentic_runner_host::config::HostConfig;
use greentic_runner_host::pack::HostState;
use greentic_runner_host::secrets::DynSecretsManager;
use greentic_secrets_lib::{SecretError, SecretsManager};
use reqwest::blocking::Client as BlockingClient;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;

/// Secrets manager that returns a fixed value for a specific canonicalized
/// path suffix, and `NotFound` for everything else.
#[derive(Default)]
struct StubSecretsManager {
    /// Suffix (post-canonicalization) → value bytes.
    values: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl StubSecretsManager {
    fn with(suffix: &str, value: &[u8]) -> Arc<Self> {
        let mgr = Arc::new(Self::default());
        mgr.values
            .lock()
            .expect("values lock")
            .insert(suffix.to_string(), value.to_vec());
        mgr
    }
}

#[async_trait]
impl SecretsManager for StubSecretsManager {
    async fn read(&self, path: &str) -> Result<Vec<u8>, SecretError> {
        let values = self.values.lock().expect("values lock");
        for (suffix, bytes) in values.iter() {
            if path.contains(suffix) {
                return Ok(bytes.clone());
            }
        }
        Err(SecretError::NotFound("missing".into()))
    }
    async fn write(&self, _path: &str, _bytes: &[u8]) -> Result<(), SecretError> {
        Ok(())
    }
    async fn delete(&self, _path: &str) -> Result<(), SecretError> {
        Ok(())
    }
}

fn write_minimal_config() -> Result<Arc<HostConfig>> {
    let temp = TempDir::new()?;
    let path = temp.path().join("bindings.yaml");
    std::fs::write(
        &path,
        r#"
tenant: demo
flow_type_bindings: {}
rate_limits: {}
retry: {}
timers: []
"#,
    )?;
    let mut cfg = HostConfig::load_from_path(&path)?;
    cfg.secrets_policy = greentic_runner_host::config::SecretsPolicy::allow_all();
    Ok(Arc::new(cfg))
}

struct EnvGuard {
    key: String,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            key: key.to_string(),
            prev,
        }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(val) = self.prev.clone() {
            unsafe {
                std::env::set_var(&self.key, val);
            }
        } else {
            unsafe {
                std::env::remove_var(&self.key);
            }
        }
    }
}

fn make_host_state(
    pack_id: &str,
    secrets: DynSecretsManager,
    non_secret: Option<Arc<BTreeMap<String, serde_json::Value>>>,
) -> Result<HostState> {
    let config = write_minimal_config()?;
    HostState::new(
        pack_id.to_string(),
        Arc::clone(&config),
        Arc::new(BlockingClient::builder().build()?),
        None,
        None,
        None,
        secrets,
        None,
        None,
        Some("component.alpha".to_string()),
        false,
        non_secret,
    )
}

#[test]
#[serial]
fn non_secret_hit_returns_json_string_no_secrets_lookup() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let mut map = BTreeMap::new();
    map.insert(
        "openai_base_url".to_string(),
        json!("https://api.openai.com/v1"),
    );
    map.insert("retry_max".to_string(), json!(3));
    let map = Arc::new(map);

    // Secrets manager would error if called; we expect it NOT to be hit.
    let secrets: DynSecretsManager = Arc::new(StubSecretsManager::default());
    let mut host = make_host_state("non-secret-hit", secrets, Some(Arc::clone(&map)))?;

    let url = RuntimeConfigHost::get(&mut host, "openai_base_url".to_string())
        .expect("non_secret hit must not error");
    assert_eq!(url.as_deref(), Some("\"https://api.openai.com/v1\""));

    let retry = RuntimeConfigHost::get(&mut host, "retry_max".to_string())
        .expect("non_secret hit (number) must not error");
    assert_eq!(retry.as_deref(), Some("3"));
    Ok(())
}

#[test]
#[serial]
fn fallback_to_secrets_when_non_secret_missing_then_found_returns_value() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    // No non_secret map present. canonicalize_secret_key lowercases.
    let secrets: DynSecretsManager =
        StubSecretsManager::with("legacy_key", b"resolved-from-secret");
    let mut host = make_host_state("compat-fallback", secrets, None)?;

    let value = RuntimeConfigHost::get(&mut host, "LEGACY_KEY".to_string())
        .expect("compat fallback should succeed");
    assert_eq!(value.as_deref(), Some("resolved-from-secret"));
    Ok(())
}

#[test]
#[serial]
fn both_missing_returns_not_found() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let map = Arc::new(BTreeMap::new());
    let secrets: DynSecretsManager = Arc::new(StubSecretsManager::default());
    let mut host = make_host_state("both-missing", secrets, Some(map))?;

    let value =
        RuntimeConfigHost::get(&mut host, "absent".to_string()).expect("not-found is Ok(None)");
    assert!(
        value.is_none(),
        "missing key returns Ok(None), got {value:?}"
    );
    Ok(())
}

#[test]
#[serial]
fn empty_key_returns_invalid_key() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let secrets: DynSecretsManager = Arc::new(StubSecretsManager::default());
    let mut host = make_host_state("empty-key", secrets, None)?;

    let err = RuntimeConfigHost::get(&mut host, "   ".to_string())
        .expect_err("blank key must be invalid");
    assert!(matches!(err, ConfigError::InvalidKey));
    Ok(())
}

#[test]
#[serial]
fn non_secret_takes_precedence_over_secrets_store() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    // Same key lives in both channels; non_secret wins, no compat warn fires.
    let mut map = BTreeMap::new();
    map.insert("shared_key".to_string(), json!("from-non-secret"));
    let map = Arc::new(map);
    let secrets: DynSecretsManager = StubSecretsManager::with("shared_key", b"from-secrets");
    let mut host = make_host_state("precedence", secrets, Some(map))?;

    let value = RuntimeConfigHost::get(&mut host, "shared_key".to_string())
        .expect("precedence path must succeed");
    assert_eq!(value.as_deref(), Some("\"from-non-secret\""));
    Ok(())
}
