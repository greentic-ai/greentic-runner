use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use greentic_interfaces_wasmtime::host_helpers::v1::secrets_store::{
    SecretsError, SecretsStoreHost,
};
use greentic_runner_host::config::HostConfig;
use greentic_runner_host::pack::HostState;
use greentic_runner_host::secrets::DynSecretsManager;
use greentic_secrets_lib::{SecretError, SecretsManager};
use reqwest::blocking::Client as BlockingClient;
use serial_test::serial;
use tempfile::TempDir;

#[derive(Default)]
struct RecordingReadSecretsManager {
    reads: Mutex<Vec<String>>,
}

#[async_trait]
impl SecretsManager for RecordingReadSecretsManager {
    async fn read(&self, path: &str) -> Result<Vec<u8>, SecretError> {
        let mut reads = self.reads.lock().expect("reads lock");
        reads.push(path.to_string());
        Err(SecretError::NotFound("missing".into()))
    }

    async fn write(&self, _path: &str, _bytes: &[u8]) -> Result<(), SecretError> {
        Ok(())
    }

    async fn delete(&self, _path: &str) -> Result<(), SecretError> {
        Ok(())
    }
}

#[test]
#[serial]
fn host_gets_canonical_key() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let config = write_minimal_config()?;
    let manager = Arc::new(RecordingReadSecretsManager::default());
    let secrets: DynSecretsManager = manager.clone();
    let mut host_state = HostState::new(
        "secrets-canonicalization".to_string(),
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
        None,
    )?;

    let result = SecretsStoreHost::get(&mut host_state, "TELEGRAM_BOT_TOKEN".to_string());
    assert!(matches!(result, Err(SecretsError::NotFound)));

    let reads = manager.reads.lock().expect("reads lock");
    assert_eq!(reads.len(), 1);
    assert!(
        reads[0].contains("telegram_bot_token"),
        "expected canonical key in path, got {}",
        reads[0]
    );
    Ok(())
}

#[test]
#[serial]
fn host_direct_get_secret_uses_canonical_key() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let config = write_minimal_config()?;
    let manager = Arc::new(RecordingReadSecretsManager::default());
    let secrets: DynSecretsManager = manager.clone();
    let host_state = HostState::new(
        "secrets-canonicalization".to_string(),
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
        None,
    )?;

    let result = host_state.get_secret("OLLAMA_API_KEY");
    assert!(result.is_err());

    let reads = manager.reads.lock().expect("reads lock");
    assert_eq!(reads.len(), 1);
    assert!(
        reads[0].contains("ollama_api_key"),
        "expected canonical key in path, got {}",
        reads[0]
    );
    Ok(())
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
        EnvGuard {
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
