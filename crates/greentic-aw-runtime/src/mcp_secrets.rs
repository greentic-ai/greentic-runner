//! Adapts the runner's `SecretsManager` to the `SecretsStore` trait
//! `greentic-mcp-exec` expects, so a `local-wasm` MCP component's
//! `secret_get` resolves instead of returning `secrets-unavailable`.
//!
//! The URI shape is dictated by greentic-designer-admin, which writes these
//! secrets: `secrets://<env>/<tenant>/<team>/mcp/<name>`
//! (parity source: `greentic-designer-admin/src/secrets/scope.rs`
//! `SecretScope::uri`). That `mcp`-category writer emits the name and team
//! **verbatim** — it does NOT canonicalize. This reader matches it exactly:
//! the key and team pass through unchanged, and an absent team becomes `_`
//! (`team.unwrap_or("_")`). Canonicalizing here would read a different URI
//! than admin wrote — and admin already keys the http-transport auth token by
//! a hyphenated UUID that lowercasing/`_`-substitution would corrupt. Only
//! admin's separate *pack* path canonicalizes; the `mcp` category does not.

use std::sync::Arc;

use greentic_mcp_exec::SecretsStore;
use greentic_secrets_lib::SecretsManager;
use greentic_types::TenantCtx;

/// Category segment admin uses for MCP secrets.
const MCP_CATEGORY: &str = "mcp";

/// Env segment for MCP secrets. Admin pins ALL MCP secrets to `default`
/// regardless of the tenant's flow env — both when sealing (`mcp_scope`) and
/// resolving (`ResolveCtx`) in greentic-designer-admin. The reader must match,
/// or it would look under the flow env (`prod`/`local`) and silently miss
/// admin's write.
const MCP_ENV_SEGMENT: &str = "default";

/// Team segment admin writes for a secret that is not scoped to a team.
pub const MCP_TENANT_DEFAULT_TEAM: &str = "_";

/// Build the secret URI for an MCP key, byte-for-byte compatible with admin's
/// `SecretScope::uri` for the `mcp` category: env pinned to `default`, key and
/// team emitted verbatim, absent team becomes `_`. An empty key is rejected as
/// a caller error rather than producing a trailing-slash URI.
///
/// THE one builder for this shape. `greentic-runner-host`'s flow MCP node had a
/// byte-for-byte second copy (`runner::mcp_node::aw::pack_route_secret_uri`),
/// whose own doc comment said so; two copies is how the flow path and the agent
/// path start resolving different URIs for the same server with nothing
/// failing. Both now call this.
pub fn mcp_secret_uri(tenant: &str, team: Option<&str>, key: &str) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err("secret key must not be empty".to_string());
    }
    Ok(format!(
        "secrets://{}/{}/{}/{}/{}",
        MCP_ENV_SEGMENT,
        tenant,
        team.unwrap_or(MCP_TENANT_DEFAULT_TEAM),
        MCP_CATEGORY,
        key
    ))
}

/// [`mcp_secret_uri`] with the tenant and team read off a [`TenantCtx`].
pub(crate) fn mcp_secret_uri_for_ctx(ctx: &TenantCtx, key: &str) -> Result<String, String> {
    let team = ctx
        .team_id
        .as_ref()
        .or(ctx.team.as_ref())
        .map(|value| value.as_str());
    mcp_secret_uri(ctx.tenant.as_str(), team, key)
}

/// Every credential URI [`read_mcp_secret`] will try, in order: the caller's
/// team scope first, then the tenant-default `_` scope.
///
/// Mirrors greentic-designer-admin's own `AdminSecretResolver` precedence, so a
/// deployed pack resolves the same value the composer resolved when the author
/// bound the tool. A caller with no team, or one already naming `_`, yields the
/// single tenant-default URI — exactly today's behaviour.
fn mcp_secret_uri_candidates(
    tenant: &str,
    team: Option<&str>,
    key: &str,
) -> Result<Vec<String>, String> {
    let team = team.filter(|t| !t.is_empty() && *t != MCP_TENANT_DEFAULT_TEAM);
    let mut uris = Vec::with_capacity(2);
    if let Some(team) = team {
        uris.push(mcp_secret_uri(tenant, Some(team), key)?);
    }
    uris.push(mcp_secret_uri(tenant, None, key)?);
    Ok(uris)
}

/// No credential was readable at any scope [`read_mcp_secret`] tried.
///
/// Carries the URIs rather than an opaque `NotFound` because on this path the
/// two are indistinguishable to an operator: a missing secret and an
/// unregistered server look identical, and the most common cause is a backend
/// that cannot resolve a `secrets://` URI at all.
#[derive(Debug, Clone)]
pub struct McpSecretMiss {
    pub uris: Vec<String>,
    pub error: String,
}

impl std::fmt::Display for McpSecretMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no credential at {} ({}). Note that MCP requires SECRETS_BACKEND=broker; \
             the env backend cannot resolve a secrets:// URI.",
            self.uris.join(" or "),
            self.error
        )
    }
}

/// Read an MCP secret, trying the caller's team scope before the tenant-default
/// `_` scope (see [`mcp_secret_uri_candidates`]).
///
/// The team half is what makes a team-scoped MCP server's token reachable at
/// run time at all: admin seals the token at the server ROW's team scope, and
/// the deployed runtime carries no team of its own — so every lane resolved
/// `_` only and a team-scoped server silently had no credential.
///
/// There is no case where a previously-resolving token stops resolving: `_` is
/// still tried, and only after the team scope misses.
pub async fn read_mcp_secret(
    secrets: &dyn SecretsManager,
    tenant: &str,
    team: Option<&str>,
    key: &str,
) -> Result<Vec<u8>, McpSecretMiss> {
    let uris = mcp_secret_uri_candidates(tenant, team, key).map_err(|error| McpSecretMiss {
        uris: Vec::new(),
        error,
    })?;
    let mut last_error = String::from("no scope tried");
    for uri in &uris {
        match secrets.read(uri).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => last_error = e.to_string(),
        }
    }
    Err(McpSecretMiss {
        uris,
        error: last_error,
    })
}

/// `SecretsStore` backed by the runner's secrets manager.
pub struct McpSecretsStore {
    secrets: Arc<dyn SecretsManager>,
}

impl McpSecretsStore {
    pub fn new(secrets: Arc<dyn SecretsManager>) -> Self {
        Self { secrets }
    }
}

impl SecretsStore for McpSecretsStore {
    fn read(&self, scope: &TenantCtx, name: &str) -> Result<Vec<u8>, String> {
        let uri = mcp_secret_uri_for_ctx(scope, name)?;
        // `SecretsManager` is async; the mcp-exec host trait is sync and is
        // already invoked from inside `spawn_blocking`, so a current-thread
        // block here does not stall the async runtime.
        futures::executor::block_on(self.secrets.read(&uri)).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use greentic_types::{EnvId, TenantId};
    use std::sync::Mutex;

    struct FakeSecrets {
        seen: Mutex<Vec<String>>,
        value: Vec<u8>,
    }

    #[async_trait]
    impl SecretsManager for FakeSecrets {
        async fn read(&self, path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
            self.seen.lock().unwrap().push(path.to_string());
            Ok(self.value.clone())
        }
        async fn write(&self, _: &str, _: &[u8]) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &str) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }
    }

    fn ctx() -> TenantCtx {
        TenantCtx::new(EnvId::new("prod").unwrap(), TenantId::new("acme").unwrap())
    }

    #[test]
    fn uri_matches_admin_shape_without_team() {
        // env is pinned to `default` (admin's convention), NOT the flow env, and
        // the key is raw — exactly as admin's `SecretScope::uri` (mcp category)
        // writes it.
        assert_eq!(
            mcp_secret_uri_for_ctx(&ctx(), "EXAMPLE_KEY").unwrap(),
            "secrets://default/acme/_/mcp/EXAMPLE_KEY"
        );
    }

    #[test]
    fn uri_pins_env_to_default_ignoring_flow_env() {
        // `ctx()` has env `prod`, but admin seals MCP secrets at env `default`
        // regardless of the tenant's flow env. Teeth guard against reading at
        // `ctx.env`, which would silently miss admin's write.
        let uri = mcp_secret_uri_for_ctx(&ctx(), "K").unwrap();
        assert!(
            uri.starts_with("secrets://default/"),
            "env must be pinned to `default` to match admin, got: {uri}"
        );
    }

    #[test]
    fn uri_preserves_key_case_and_punctuation_verbatim() {
        // The admin `mcp` category writer does NOT canonicalize; canonicalizing
        // here would read a different URI than admin wrote. This is the teeth
        // guard against reintroducing lowercase/`_`-substitution.
        assert_eq!(
            mcp_secret_uri_for_ctx(&ctx(), "Petstore-API.key").unwrap(),
            "secrets://default/acme/_/mcp/Petstore-API.key"
        );
    }

    #[test]
    fn uri_emits_team_segment_verbatim_matching_admin() {
        // Admin uses `team.unwrap_or("_")` — a raw passthrough, no empty/default
        // folding. A team is emitted as-is.
        let mut with_team = ctx();
        with_team.team_id = Some(greentic_types::TeamId::new("Sales").unwrap());
        assert_eq!(
            mcp_secret_uri_for_ctx(&with_team, "K").unwrap(),
            "secrets://default/acme/Sales/mcp/K"
        );
    }

    #[test]
    fn uri_rejects_empty_key() {
        assert!(mcp_secret_uri_for_ctx(&ctx(), "   ").is_err());
    }

    #[test]
    fn read_uses_the_scoped_uri_and_returns_bytes() {
        let fake = Arc::new(FakeSecrets {
            seen: Mutex::new(Vec::new()),
            value: b"sk-live".to_vec(),
        });
        let store = McpSecretsStore::new(fake.clone());
        let got = store.read(&ctx(), "EXAMPLE_KEY").unwrap();
        assert_eq!(got, b"sk-live".to_vec());
        assert_eq!(
            fake.seen.lock().unwrap().as_slice(),
            &["secrets://default/acme/_/mcp/EXAMPLE_KEY".to_string()]
        );
    }

    /// A secrets manager holding exactly the URIs it was given.
    struct MapSecrets {
        entries: std::collections::HashMap<String, Vec<u8>>,
        seen: Mutex<Vec<String>>,
    }

    impl MapSecrets {
        fn with(pairs: &[(&str, &str)]) -> Arc<Self> {
            Arc::new(Self {
                entries: pairs
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.as_bytes().to_vec()))
                    .collect(),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl SecretsManager for MapSecrets {
        async fn read(&self, path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
            self.seen.lock().unwrap().push(path.to_string());
            self.entries
                .get(path)
                .cloned()
                .ok_or_else(|| greentic_secrets_lib::SecretError::NotFound(path.to_string()))
        }
        async fn write(&self, _: &str, _: &[u8]) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }
        async fn delete(&self, _: &str) -> greentic_secrets_lib::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn read_prefers_the_team_scope_when_it_holds_the_token() {
        let secrets = MapSecrets::with(&[
            ("secrets://default/acme/sales/mcp/srv-1", "team-token"),
            ("secrets://default/acme/_/mcp/srv-1", "tenant-token"),
        ]);
        let got = read_mcp_secret(secrets.as_ref(), "acme", Some("sales"), "srv-1")
            .await
            .unwrap();
        assert_eq!(got, b"team-token".to_vec());
        assert_eq!(
            secrets.seen.lock().unwrap().as_slice(),
            &["secrets://default/acme/sales/mcp/srv-1".to_string()],
            "the tenant-default scope must not be read once the team scope hits"
        );
    }

    #[tokio::test]
    async fn read_falls_back_to_the_tenant_default_scope() {
        // The pre-existing shape: a tenant-default server row. Naming a team
        // must never make a token that resolved before stop resolving.
        let secrets = MapSecrets::with(&[("secrets://default/acme/_/mcp/srv-1", "tenant-token")]);
        let got = read_mcp_secret(secrets.as_ref(), "acme", Some("sales"), "srv-1")
            .await
            .unwrap();
        assert_eq!(got, b"tenant-token".to_vec());
        assert_eq!(
            secrets.seen.lock().unwrap().as_slice(),
            &[
                "secrets://default/acme/sales/mcp/srv-1".to_string(),
                "secrets://default/acme/_/mcp/srv-1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn read_with_no_team_tries_only_the_tenant_default_scope() {
        let secrets = MapSecrets::with(&[("secrets://default/acme/_/mcp/srv-1", "tenant-token")]);
        assert!(
            read_mcp_secret(secrets.as_ref(), "acme", None, "srv-1")
                .await
                .is_ok()
        );
        assert_eq!(secrets.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn read_does_not_try_the_underscore_scope_twice() {
        // `_` IS the tenant-default segment; a record naming it explicitly must
        // not produce a duplicate lookup.
        let secrets = MapSecrets::with(&[]);
        let miss = read_mcp_secret(secrets.as_ref(), "acme", Some("_"), "srv-1")
            .await
            .unwrap_err();
        assert_eq!(miss.uris, vec!["secrets://default/acme/_/mcp/srv-1"]);
    }

    #[tokio::test]
    async fn miss_names_every_uri_tried_and_the_broker_requirement() {
        let secrets = MapSecrets::with(&[]);
        let miss = read_mcp_secret(secrets.as_ref(), "acme", Some("sales"), "srv-1")
            .await
            .unwrap_err();
        let rendered = miss.to_string();
        assert!(
            rendered.contains("secrets://default/acme/sales/mcp/srv-1")
                && rendered.contains("secrets://default/acme/_/mcp/srv-1"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("SECRETS_BACKEND=broker"),
            "the operator needs the cause, not an opaque NotFound; got: {rendered}"
        );
    }
}
