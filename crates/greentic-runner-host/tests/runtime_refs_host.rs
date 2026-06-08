//! C5 — `RuntimeConfigHost::get` runtime-refs channel.
//!
//! Verifies the three-arm precedence (non_secret → runtime_refs →
//! compat-shim), the resolver error mapping (Invalid→InvalidKey,
//! Internal→Internal), and the per-pack `refs` map gating which keys this
//! channel claims.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use greentic_interfaces_wasmtime::host_helpers::v1::runtime_config::{
    ConfigError, RuntimeConfigHost,
};
use greentic_runner_host::config::HostConfig;
use greentic_runner_host::pack::HostState;
use greentic_runner_host::runtime_refs::{
    RuntimeRefResolver, RuntimeRefResolverError, RuntimeRefsInjection,
};
use greentic_runner_host::secrets::DynSecretsManager;
use greentic_secrets_lib::{SecretError, SecretsManager};
use reqwest::blocking::Client as BlockingClient;
use serde_json::{Value, json};
use serial_test::serial;
use tempfile::TempDir;

/// Test resolver. Constructed with a `key → outcome` map; outcomes can be
/// `Ok(Some(value))`, `Ok(None)`, or any `RuntimeRefResolverError`.
#[derive(Debug)]
struct StubResolver {
    outcomes: Mutex<BTreeMap<String, ResolveOutcome>>,
}

#[derive(Debug, Clone)]
enum ResolveOutcome {
    Found(Value),
    Missing,
    Invalid(String),
    Internal(String),
}

impl StubResolver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(BTreeMap::new()),
        })
    }

    fn set(&self, uri: &str, outcome: ResolveOutcome) {
        self.outcomes
            .lock()
            .expect("outcomes lock")
            .insert(uri.to_string(), outcome);
    }
}

impl RuntimeRefResolver for StubResolver {
    fn resolve(&self, runtime_ref: &str) -> Result<Option<Value>, RuntimeRefResolverError> {
        let outcomes = self.outcomes.lock().expect("outcomes lock");
        match outcomes.get(runtime_ref).cloned() {
            Some(ResolveOutcome::Found(v)) => Ok(Some(v)),
            Some(ResolveOutcome::Missing) | None => Ok(None),
            Some(ResolveOutcome::Invalid(msg)) => Err(RuntimeRefResolverError::Invalid(msg)),
            Some(ResolveOutcome::Internal(msg)) => Err(RuntimeRefResolverError::Internal(msg)),
        }
    }
}

#[derive(Default)]
struct EmptySecretsManager;

#[async_trait]
impl SecretsManager for EmptySecretsManager {
    async fn read(&self, _path: &str) -> Result<Vec<u8>, SecretError> {
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
    non_secret: Option<Arc<BTreeMap<String, Value>>>,
    runtime_refs: Option<RuntimeRefsInjection>,
) -> Result<HostState> {
    let config = write_minimal_config()?;
    let secrets: DynSecretsManager = Arc::new(EmptySecretsManager);
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
        runtime_refs,
    )
}

fn injection(refs: &[(&str, &str)], resolver: Arc<StubResolver>) -> RuntimeRefsInjection {
    let mut map = BTreeMap::new();
    for (k, uri) in refs {
        map.insert((*k).to_string(), (*uri).to_string());
    }
    RuntimeRefsInjection {
        refs: Arc::new(map),
        resolver: resolver as Arc<dyn RuntimeRefResolver>,
    }
}

#[test]
#[serial]
fn runtime_refs_hit_resolves_via_resolver() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let resolver = StubResolver::new();
    resolver.set(
        "runtime://local/discovered/alb_dns",
        ResolveOutcome::Found(json!("alb.example.com")),
    );
    let injection = injection(
        &[("alb_dns", "runtime://local/discovered/alb_dns")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("alb-host", None, Some(injection))?;

    let value =
        RuntimeConfigHost::get(&mut host, "alb_dns".to_string()).expect("runtime-refs must hit");
    assert_eq!(value.as_deref(), Some("\"alb.example.com\""));
    Ok(())
}

#[test]
#[serial]
fn runtime_refs_miss_falls_through_to_compat_shim() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let resolver = StubResolver::new();
    // refs map carries only `alb_dns`; lookup for a different key must skip
    // the runtime-refs channel entirely (resolver never called).
    let injection = injection(
        &[("alb_dns", "runtime://local/discovered/alb_dns")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("fall-through", None, Some(injection))?;

    let value = RuntimeConfigHost::get(&mut host, "other_key".to_string())
        .expect("fall-through to secrets shim is Ok(None) when secrets miss");
    assert!(value.is_none());
    Ok(())
}

#[test]
#[serial]
fn non_secret_takes_precedence_over_runtime_refs() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let mut non_secret = BTreeMap::new();
    non_secret.insert("shared".to_string(), json!("from-non-secret"));
    let resolver = StubResolver::new();
    resolver.set(
        "runtime://local/discovered/shared",
        ResolveOutcome::Found(json!("from-runtime-refs")),
    );
    let injection = injection(
        &[("shared", "runtime://local/discovered/shared")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("precedence", Some(Arc::new(non_secret)), Some(injection))?;

    let value = RuntimeConfigHost::get(&mut host, "shared".to_string()).expect("non_secret wins");
    assert_eq!(value.as_deref(), Some("\"from-non-secret\""));
    Ok(())
}

#[test]
#[serial]
fn resolver_ok_none_returns_ok_none() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let resolver = StubResolver::new();
    resolver.set(
        "runtime://local/discovered/alb_dns",
        ResolveOutcome::Missing,
    );
    let injection = injection(
        &[("alb_dns", "runtime://local/discovered/alb_dns")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("ok-none", None, Some(injection))?;

    let value =
        RuntimeConfigHost::get(&mut host, "alb_dns".to_string()).expect("Ok(None) preserved");
    assert!(value.is_none());
    Ok(())
}

#[test]
#[serial]
fn resolver_invalid_returns_invalid_key() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let resolver = StubResolver::new();
    resolver.set(
        "runtime://other/discovered/alb_dns",
        ResolveOutcome::Invalid("env mismatch".into()),
    );
    let injection = injection(
        &[("alb_dns", "runtime://other/discovered/alb_dns")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("invalid", None, Some(injection))?;

    let err =
        RuntimeConfigHost::get(&mut host, "alb_dns".to_string()).expect_err("Invalid must surface");
    assert!(matches!(err, ConfigError::InvalidKey));
    Ok(())
}

#[test]
#[serial]
fn resolver_internal_returns_internal() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let resolver = StubResolver::new();
    resolver.set(
        "runtime://local/discovered/alb_dns",
        ResolveOutcome::Internal("snapshot read failed".into()),
    );
    let injection = injection(
        &[("alb_dns", "runtime://local/discovered/alb_dns")],
        Arc::clone(&resolver),
    );
    let mut host = make_host_state("internal", None, Some(injection))?;

    let err = RuntimeConfigHost::get(&mut host, "alb_dns".to_string())
        .expect_err("Internal must surface");
    assert!(matches!(err, ConfigError::Internal));
    Ok(())
}

#[test]
#[serial]
fn no_injection_falls_through_to_compat_shim() -> Result<()> {
    let _guard = EnvGuard::set("GREENTIC_PROVIDER_CORE_ONLY", "0");
    let mut host = make_host_state("no-injection", None, None)?;

    let value = RuntimeConfigHost::get(&mut host, "anything".to_string())
        .expect("no injection → compat shim returns Ok(None) when secrets miss");
    assert!(value.is_none());
    Ok(())
}
