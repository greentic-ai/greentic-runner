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

/// Build the secret URI for an MCP component's key, byte-for-byte compatible
/// with admin's `SecretScope::uri` for the `mcp` category: env pinned to
/// `default`, key and team emitted verbatim, absent team becomes `_`. An empty
/// key is rejected as a caller error rather than producing a trailing-slash URI.
pub(crate) fn mcp_secret_uri(ctx: &TenantCtx, key: &str) -> Result<String, String> {
    if key.trim().is_empty() {
        return Err("secret key must not be empty".to_string());
    }
    let team = ctx
        .team_id
        .as_ref()
        .or(ctx.team.as_ref())
        .map(|value| value.as_str())
        .unwrap_or("_");
    Ok(format!(
        "secrets://{}/{}/{}/{}/{}",
        MCP_ENV_SEGMENT,
        ctx.tenant.as_str(),
        team,
        MCP_CATEGORY,
        key
    ))
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
        let uri = mcp_secret_uri(scope, name)?;
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
            mcp_secret_uri(&ctx(), "EXAMPLE_KEY").unwrap(),
            "secrets://default/acme/_/mcp/EXAMPLE_KEY"
        );
    }

    #[test]
    fn uri_pins_env_to_default_ignoring_flow_env() {
        // `ctx()` has env `prod`, but admin seals MCP secrets at env `default`
        // regardless of the tenant's flow env. Teeth guard against reading at
        // `ctx.env`, which would silently miss admin's write.
        let uri = mcp_secret_uri(&ctx(), "K").unwrap();
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
            mcp_secret_uri(&ctx(), "Petstore-API.key").unwrap(),
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
            mcp_secret_uri(&with_team, "K").unwrap(),
            "secrets://default/acme/Sales/mcp/K"
        );
    }

    #[test]
    fn uri_rejects_empty_key() {
        assert!(mcp_secret_uri(&ctx(), "   ").is_err());
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
}
