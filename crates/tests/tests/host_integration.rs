use std::fs;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use greentic_runner_host::watcher;
use greentic_runner_host::{Activity, HostBuilder, HostConfig, RunnerHost};
use runner_core::env::PackConfig;
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::sleep;

#[tokio::test]
#[serial]
async fn host_executes_demo_pack_flow() -> Result<()> {
    let _secret_guard = EnvGuard::set("TELEGRAM_BOT_TOKEN", "test-token");
    let _backend_guard = EnvGuard::set("SECRETS_BACKEND", "env");

    let bindings = fixture_path("examples/bindings/default.bindings.yaml");
    let config = HostConfig::load_from_path(&bindings)?;
    let host = HostBuilder::new().with_config(config).build()?;
    host.start().await?;

    let pack_path = fixture_path("examples/packs/demo.gtpack");
    host.load_pack("acme", pack_path.as_path()).await?;

    let activity = Activity::text("hello from integration")
        .with_tenant("acme")
        .from_user("user-1");
    let replies = host.handle_activity("acme", activity).await?;
    assert!(
        !replies.is_empty(),
        "demo pack should emit at least one activity"
    );

    host.stop().await?;
    Ok(())
}

#[tokio::test]
#[serial]
async fn pack_watcher_resolves_index_and_reloads() -> Result<()> {
    let cache_dir = TempDir::new()?;
    let _source = EnvGuard::set("PACK_SOURCE", "fs");
    let index = fixture_path("examples/index.json");
    let _index = EnvGuard::set("PACK_INDEX_URL", index.display().to_string());
    let _cache = EnvGuard::set("PACK_CACHE_DIR", cache_dir.path().to_string_lossy());
    let _backend_guard = EnvGuard::set("SECRETS_BACKEND", "env");

    let pack_cfg = PackConfig::from_env()?;
    let bindings = fixture_path("examples/bindings/default.bindings.yaml");
    let config = HostConfig::load_from_path(&bindings)?;
    let host = Arc::new(HostBuilder::new().with_config(config).build()?);
    host.start().await?;

    let (watcher_guard, reload) =
        watcher::start_pack_watcher(Arc::clone(&host), pack_cfg, Duration::from_millis(250))
            .await?;

    wait_for(|| host.active_packs().len() == 1, Duration::from_secs(5)).await?;
    reload.trigger().await?;
    wait_for(
        || host.health_state().snapshot().last_reload.is_some(),
        Duration::from_secs(5),
    )
    .await?;

    drop(watcher_guard);
    host.stop().await?;
    Ok(())
}

#[tokio::test]
#[serial]
async fn pack_watcher_handles_overlays() -> Result<()> {
    let temp = TempDir::new()?;
    let cache_dir = temp.path().join("cache");
    fs::create_dir_all(&cache_dir)?;
    let index_path = temp.path().join("index-overlay.json");
    write_overlay_index(&index_path, true)?;

    let _source = EnvGuard::set("PACK_SOURCE", "fs");
    let _index = EnvGuard::set("PACK_INDEX_URL", index_path.display().to_string());
    let _cache = EnvGuard::set("PACK_CACHE_DIR", cache_dir.to_string_lossy());
    let _backend_guard = EnvGuard::set("SECRETS_BACKEND", "env");

    let pack_cfg = PackConfig::from_env()?;
    let bindings = fixture_path("examples/bindings/default.bindings.yaml");
    let config = HostConfig::load_from_path(&bindings)?;
    let host = Arc::new(HostBuilder::new().with_config(config).build()?);
    host.start().await?;

    let (watcher_guard, reload) =
        watcher::start_pack_watcher(Arc::clone(&host), pack_cfg, Duration::from_millis(250))
            .await?;

    let host_for_initial = Arc::clone(&host);
    wait_for_async(
        move || {
            let host = Arc::clone(&host_for_initial);
            async move {
                tenant_overlay_count(&host, "acme")
                    .await
                    .map(|count| count == 1)
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(5),
    )
    .await?;

    write_overlay_index(&index_path, false)?;
    reload.trigger().await?;
    let host_for_reload = Arc::clone(&host);
    wait_for_async(
        move || {
            let host = Arc::clone(&host_for_reload);
            async move {
                tenant_overlay_count(&host, "acme")
                    .await
                    .map(|count| count == 0)
                    .unwrap_or(false)
            }
        },
        Duration::from_secs(5),
    )
    .await?;

    drop(watcher_guard);
    host.stop().await?;
    Ok(())
}

struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<str>) -> Self {
        let prev = std::env::var(key).ok();
        unsafe {
            std::env::set_var(key, value.as_ref());
        }
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(ref value) = self.prev {
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

async fn wait_for<F>(mut predicate: F, timeout: Duration) -> Result<()>
where
    F: FnMut() -> bool,
{
    let step = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        if predicate() {
            return Ok(());
        }
        sleep(step).await;
        elapsed += step;
    }
    bail!("condition not met within {:?}", timeout);
}

async fn wait_for_async<F, Fut>(mut predicate: F, timeout: Duration) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let step = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < timeout {
        if predicate().await {
            return Ok(());
        }
        sleep(step).await;
        elapsed += step;
    }
    bail!("condition not met within {:?}", timeout);
}

fn fixture_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative)
}

fn write_overlay_index(path: &std::path::Path, include_overlay: bool) -> Result<()> {
    const DEMO_DIGEST: &str =
        "sha256:c6ba298a9a4154f8fff7486b6594b4235a771b525824bfe48e691ed16ff8ab37";
    let pack_path = fixture_path("examples/packs/demo.gtpack");
    let mut overlays = Vec::new();
    if include_overlay {
        overlays.push(serde_json::json!({
            "name": "ai.greentic.runner.overlay",
            "version": "0.1.0",
            "locator": pack_path.display().to_string(),
            "digest": DEMO_DIGEST
        }));
    }
    let index = serde_json::json!({
        "acme": {
            "main_pack": {
                "name": "ai.greentic.runner.example",
                "version": "0.1.0",
                "locator": pack_path.display().to_string(),
                "digest": DEMO_DIGEST
            },
            "overlays": overlays
        }
    });
    fs::write(path, serde_json::to_vec_pretty(&index)?)?;
    Ok(())
}

async fn tenant_overlay_count(host: &Arc<RunnerHost>, tenant: &str) -> Option<usize> {
    host.tenant(tenant)
        .await
        .map(|handle| handle.overlays().len())
}
