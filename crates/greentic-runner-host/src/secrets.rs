use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::runtime::block_on;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use greentic_secrets_lib::env::EnvSecretsManager;
use greentic_secrets_lib::{SecretScope, SecretsManager};
use greentic_types::TenantCtx;
use lru::LruCache;
use parking_lot::Mutex;

/// Shared secrets manager handle used by the host.
pub type DynSecretsManager = Arc<dyn SecretsManager>;

/// Supported secrets backend kinds recognised by the runner.
#[derive(Clone, Debug)]
pub enum SecretsBackend {
    Env,
    /// HTTP secrets broker (greentic-secrets-broker). The `endpoint` is the
    /// base URL (e.g. `http://secrets-broker:8080`) and `token` is the Bearer
    /// auth token (may be empty for unauthenticated local deployments).
    Broker {
        endpoint: String,
        token: String,
    },
}

impl SecretsBackend {
    pub fn from_env(value: Option<String>) -> Result<Self> {
        match value
            .unwrap_or_else(|| "env".into())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "env" => Ok(SecretsBackend::Env),
            "broker" => Self::broker_from_strings(
                "broker",
                std::env::var("SECRETS_BROKER_ENDPOINT")
                    .unwrap_or_default()
                    .as_str(),
                std::env::var("SECRETS_BROKER_TOKEN")
                    .unwrap_or_default()
                    .as_str(),
            ),
            other => Err(anyhow!("unsupported SECRETS_BACKEND `{other}`")),
        }
    }

    /// Pure constructor used by both `from_env` and tests. Validates that
    /// `endpoint` is non-empty; `token` may be empty for unauthenticated use.
    /// `kind` is passed solely for error-message context.
    pub(crate) fn broker_from_strings(kind: &str, endpoint: &str, token: &str) -> Result<Self> {
        let endpoint = endpoint.trim().trim_end_matches('/');
        if endpoint.is_empty() {
            return Err(anyhow!(
                "SECRETS_BACKEND={kind} requires SECRETS_BROKER_ENDPOINT to be set"
            ));
        }
        Ok(SecretsBackend::Broker {
            endpoint: endpoint.to_owned(),
            token: token.to_owned(),
        })
    }

    pub fn from_config(cfg: &greentic_config_types::SecretsBackendRefConfig) -> Result<Self> {
        match cfg.kind.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "env" => Ok(SecretsBackend::Env),
            // `SecretsBackendRefConfig` has no dedicated endpoint/token fields.
            // When kind="broker", use `reference` as the endpoint (if set) and
            // fall back to SECRETS_BROKER_ENDPOINT from the environment.
            // Token always comes from SECRETS_BROKER_TOKEN env.
            "broker" => {
                let endpoint = cfg
                    .reference
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .trim_end_matches('/');
                let endpoint = if endpoint.is_empty() {
                    std::env::var("SECRETS_BROKER_ENDPOINT").unwrap_or_default()
                } else {
                    endpoint.to_owned()
                };
                let token = std::env::var("SECRETS_BROKER_TOKEN").unwrap_or_default();
                Self::broker_from_strings("broker", &endpoint, &token)
            }
            other => Err(anyhow!("unsupported secrets backend `{other}`")),
        }
    }

    pub fn build_manager(&self) -> Result<DynSecretsManager> {
        let inner: DynSecretsManager = match self {
            SecretsBackend::Env => {
                ensure_env_secrets_allowed()?;
                Arc::new(EnvSecretsManager)
            }
            SecretsBackend::Broker { endpoint, token } => Arc::new(
                crate::secrets_broker::BrokerSecretsManager::new(endpoint, token),
            ),
        };
        Ok(CachingSecretsManager::wrap(inner))
    }
}

pub fn default_manager() -> Result<DynSecretsManager> {
    SecretsBackend::Env.build_manager()
}

// ---------------------------------------------------------------------------
// Caching wrapper
// ---------------------------------------------------------------------------

const DEFAULT_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_CACHE_MAX_ENTRIES: usize = 512;

fn cache_ttl() -> Duration {
    let secs = std::env::var("SECRETS_CACHE_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECS);
    Duration::from_secs(secs)
}

fn cache_max_entries() -> NonZeroUsize {
    let n = std::env::var("SECRETS_CACHE_MAX_ENTRIES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_CACHE_MAX_ENTRIES);
    NonZeroUsize::new(n).expect("cache max entries must be > 0")
}

struct CacheEntry {
    data: Vec<u8>,
    inserted_at: Instant,
}

/// A [`SecretsManager`] wrapper that caches read results in an LRU cache with
/// a time-to-live. Writes and deletes invalidate the corresponding entry so
/// subsequent reads always see the latest value.
pub struct CachingSecretsManager {
    inner: DynSecretsManager,
    cache: Mutex<LruCache<String, CacheEntry>>,
    ttl: Duration,
}

impl CachingSecretsManager {
    pub fn wrap(inner: DynSecretsManager) -> DynSecretsManager {
        Self::wrap_with(inner, cache_ttl(), cache_max_entries())
    }

    fn wrap_with(inner: DynSecretsManager, ttl: Duration, max: NonZeroUsize) -> DynSecretsManager {
        if ttl.is_zero() {
            tracing::info!("secrets cache disabled (TTL=0)");
            return inner;
        }
        tracing::info!(
            ttl_secs = ttl.as_secs(),
            max_entries = max.get(),
            "secrets value cache enabled"
        );
        Arc::new(Self {
            inner,
            cache: Mutex::new(LruCache::new(max)),
            ttl,
        })
    }
}

#[async_trait]
impl SecretsManager for CachingSecretsManager {
    async fn read(&self, path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
        // Check cache first.
        {
            let mut cache = self.cache.lock();
            if let Some(entry) = cache.get(path) {
                if entry.inserted_at.elapsed() < self.ttl {
                    return Ok(entry.data.clone());
                }
                // Expired — remove and fall through.
                cache.pop(path);
            }
        }

        let data = self.inner.read(path).await?;

        {
            let mut cache = self.cache.lock();
            cache.put(
                path.to_owned(),
                CacheEntry {
                    data: data.clone(),
                    inserted_at: Instant::now(),
                },
            );
        }
        Ok(data)
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> greentic_secrets_lib::Result<()> {
        self.inner.write(path, bytes).await?;
        // Invalidate so the next read sees the fresh value.
        self.cache.lock().pop(path);
        Ok(())
    }

    async fn delete(&self, path: &str) -> greentic_secrets_lib::Result<()> {
        self.inner.delete(path).await?;
        self.cache.lock().pop(path);
        Ok(())
    }
}

fn normalize_pack_segment(pack_id: &str) -> String {
    pack_id
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            match ch {
                'a'..='z' | '0'..='9' | '_' | '-' => ch,
                _ => '_',
            }
        })
        .collect()
}

pub fn canonicalize_secret_key(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            match ch {
                'a'..='z' | '0'..='9' | '_' => ch,
                _ => '_',
            }
        })
        .collect()
}

fn normalize_team_segment(team: Option<&str>) -> String {
    match team
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
    {
        Some(value) => value.to_string(),
        None => "_".to_string(),
    }
}

pub fn scoped_secret_path_for_pack(ctx: &TenantCtx, pack_id: &str, key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("secret key must not be empty"));
    }
    let safe_key = canonicalize_secret_key(key);
    let team = ctx.team_id.as_ref().or(ctx.team.as_ref());
    let scope = SecretScope {
        env: ctx.env.as_str().to_string(),
        tenant: ctx.tenant.as_str().to_string(),
        team: team.map(|value| value.as_str().to_string()),
    };
    let team_segment = normalize_team_segment(scope.team.as_deref());
    let pack_segment = pack_id.trim();
    if pack_segment.is_empty() {
        return Err(anyhow!("pack_id must not be empty for scoped secrets"));
    }
    let pack_segment = normalize_pack_segment(pack_segment);
    Ok(format!(
        "secrets://{}/{}/{}/{}/{}",
        scope.env, scope.tenant, team_segment, pack_segment, safe_key
    ))
}

pub fn read_secret_blocking(
    manager: &DynSecretsManager,
    ctx: &TenantCtx,
    pack_id: &str,
    key: &str,
) -> Result<Vec<u8>> {
    let scoped_key = scoped_secret_path_for_pack(ctx, pack_id, key)?;
    let bytes =
        block_on(manager.read(scoped_key.as_str())).map_err(|err| anyhow!(err.to_string()))?;
    Ok(bytes)
}

pub fn write_secret_blocking(
    manager: &DynSecretsManager,
    ctx: &TenantCtx,
    pack_id: &str,
    key: &str,
    value: &[u8],
) -> Result<()> {
    let scoped_key = scoped_secret_path_for_pack(ctx, pack_id, key)?;
    block_on(manager.write(scoped_key.as_str(), value)).map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}

fn ensure_env_secrets_allowed() -> Result<()> {
    let env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
    let env = env.trim().to_ascii_lowercase();
    if matches!(env.as_str(), "local" | "dev" | "test") {
        Ok(())
    } else {
        Err(anyhow!(
            "env secrets backend is disabled for env '{env}' (dev/test only)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_config_types::SecretsBackendRefConfig;
    use greentic_types::{EnvId, TeamId, TenantId, UserId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tenant_ctx() -> greentic_types::TenantCtx {
        greentic_types::TenantCtx::new(
            EnvId::new("local").expect("env"),
            TenantId::new("tenant-a").expect("tenant"),
        )
        .with_team(Some(TeamId::new("team-a").expect("team")))
        .with_user(Some(UserId::new("user-a").expect("user")))
    }

    #[test]
    fn scoped_secret_path_normalizes_pack_and_key_segments() {
        let path =
            scoped_secret_path_for_pack(&tenant_ctx(), "My Pack/Prod", " API/KEY value ").unwrap();
        assert_eq!(
            path,
            "secrets://local/tenant-a/team-a/my_pack_prod/api_key_value"
        );
    }

    #[test]
    fn scoped_secret_path_rejects_empty_inputs() {
        let ctx = tenant_ctx();
        assert!(scoped_secret_path_for_pack(&ctx, "demo", "   ").is_err());
        assert!(scoped_secret_path_for_pack(&ctx, "   ", "key").is_err());
    }

    #[test]
    fn scoped_secret_path_maps_default_team_to_underscore() {
        let ctx = greentic_types::TenantCtx::new(
            EnvId::new("dev").expect("env"),
            TenantId::new("demo").expect("tenant"),
        )
        .with_team(Some(TeamId::new("default").expect("team")));
        let path = scoped_secret_path_for_pack(&ctx, "ollama-runtime-repro", "ollama_api_key")
            .expect("scoped secret path");
        assert_eq!(
            path,
            "secrets://dev/demo/_/ollama-runtime-repro/ollama_api_key"
        );
    }

    #[test]
    fn scoped_secret_path_canonicalizes_provider_secret_keys() {
        let ctx = greentic_types::TenantCtx::new(
            EnvId::new("dev").expect("env"),
            TenantId::new("demo").expect("tenant"),
        )
        .with_team(Some(TeamId::new("default").expect("team")))
        .with_user(Some(UserId::new("operator").expect("user")));
        let path = scoped_secret_path_for_pack(&ctx, "ollama-runtime-repro", "OLLAMA_API_KEY")
            .expect("scoped secret path");
        assert_eq!(
            path,
            "secrets://dev/demo/_/ollama-runtime-repro/ollama_api_key"
        );
    }

    #[test]
    fn from_env_parses_broker() {
        // Test purely via the internal helper — avoids unsafe env mutation
        // (crate uses #![deny(unsafe_code)]).
        let b = SecretsBackend::broker_from_strings("broker", "http://localhost:9", "").unwrap();
        assert!(matches!(b, SecretsBackend::Broker { .. }));
    }

    #[test]
    fn from_env_broker_rejects_empty_endpoint() {
        let err = SecretsBackend::broker_from_strings("broker", "", "").unwrap_err();
        assert!(
            err.to_string().contains("SECRETS_BROKER_ENDPOINT"),
            "error was: {err}"
        );
    }

    #[test]
    fn from_config_parses_broker() {
        let b = SecretsBackend::from_config(&SecretsBackendRefConfig {
            kind: "broker".into(),
            reference: Some("http://localhost:9".into()),
        })
        .unwrap();
        assert!(matches!(b, SecretsBackend::Broker { .. }));
    }

    #[test]
    fn backend_parsers_reject_unknown_kinds() {
        let err =
            SecretsBackend::from_env(Some("vault".into())).expect_err("backend should be rejected");
        assert!(err.to_string().contains("unsupported SECRETS_BACKEND"));

        let err = SecretsBackend::from_config(&SecretsBackendRefConfig {
            kind: "vault".into(),
            reference: None,
        })
        .expect_err("backend config should be rejected");
        assert!(err.to_string().contains("unsupported secrets backend"));
    }

    #[test]
    fn backend_parsers_accept_default_aliases() {
        assert!(matches!(
            SecretsBackend::from_env(Some("".into())).unwrap(),
            SecretsBackend::Env
        ));
        assert!(matches!(
            SecretsBackend::from_config(&SecretsBackendRefConfig {
                kind: "none".into(),
                reference: None,
            })
            .unwrap(),
            SecretsBackend::Env
        ));
    }

    // -----------------------------------------------------------------------
    // CachingSecretsManager tests
    // -----------------------------------------------------------------------

    /// In-memory mock that counts how many times `read` is called.
    struct CountingManager {
        read_count: AtomicUsize,
    }

    impl CountingManager {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                read_count: AtomicUsize::new(0),
            })
        }

        fn reads(&self) -> usize {
            self.read_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SecretsManager for CountingManager {
        async fn read(&self, _path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
            self.read_count.fetch_add(1, Ordering::SeqCst);
            Ok(b"secret-value".to_vec())
        }

        async fn write(&self, _path: &str, _bytes: &[u8]) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }

        async fn delete(&self, _path: &str) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }
    }

    fn make_cached(inner: Arc<CountingManager>) -> CachingSecretsManager {
        CachingSecretsManager {
            inner: inner as DynSecretsManager,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())),
            ttl: Duration::from_secs(300),
        }
    }

    #[tokio::test]
    async fn cached_read_avoids_backend_on_second_call() {
        let inner = CountingManager::new();
        let cached = make_cached(Arc::clone(&inner));

        let v1 = cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        let v2 = cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        assert_eq!(v1, v2);
        assert_eq!(inner.reads(), 1, "second read should hit cache");
    }

    #[tokio::test]
    async fn write_invalidates_cache() {
        let inner = CountingManager::new();
        let cached = make_cached(Arc::clone(&inner));

        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        assert_eq!(inner.reads(), 1);

        cached
            .write("secrets://dev/t/team/pack/key", b"new")
            .await
            .unwrap();
        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        assert_eq!(inner.reads(), 2, "read after write should bypass cache");
    }

    #[tokio::test]
    async fn delete_invalidates_cache() {
        let inner = CountingManager::new();
        let cached = make_cached(Arc::clone(&inner));

        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        cached
            .delete("secrets://dev/t/team/pack/key")
            .await
            .unwrap();
        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        assert_eq!(inner.reads(), 2, "read after delete should bypass cache");
    }

    #[tokio::test]
    async fn expired_entry_triggers_backend_read() {
        let inner = CountingManager::new();
        let cached = CachingSecretsManager {
            inner: Arc::clone(&inner) as DynSecretsManager,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(64).unwrap())),
            ttl: Duration::from_millis(1), // expires almost immediately
        };

        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        std::thread::sleep(Duration::from_millis(5));
        cached.read("secrets://dev/t/team/pack/key").await.unwrap();
        assert_eq!(inner.reads(), 2, "expired entry should re-fetch");
    }

    #[test]
    fn wrap_with_zero_ttl_returns_inner_directly() {
        let inner: DynSecretsManager = CountingManager::new();
        let ptr_before = Arc::as_ptr(&inner);
        let wrapped = CachingSecretsManager::wrap_with(
            Arc::clone(&inner),
            Duration::ZERO,
            NonZeroUsize::new(64).unwrap(),
        );
        let ptr_after = Arc::as_ptr(&wrapped);
        // When TTL is 0, wrap should return the same Arc (no caching layer).
        assert_eq!(ptr_before, ptr_after);
    }
}
