//! Adapts the runner's `SecretsManager` to the `SecretsStore` trait
//! `greentic-mcp-exec` expects, so a `local-wasm` MCP component's
//! `secret_get` resolves instead of returning `secrets-unavailable`.
//!
//! The URI shape is dictated by greentic-designer-admin, which writes these
//! secrets: `secrets://<env>/<tenant>/<team>/mcp/<name>`
//! (parity source: `greentic-designer-admin/src/secrets/scope.rs`).
//! Team and key normalization mirror
//! `greentic-runner-host/src/secrets.rs` verbatim.

use std::sync::Arc;

use greentic_mcp_exec::SecretsStore;
use greentic_secrets_lib::SecretsManager;
use greentic_types::TenantCtx;

/// Category segment admin uses for MCP secrets.
const MCP_CATEGORY: &str = "mcp";

/// Lowercase and replace every character outside `[a-z0-9_]` with `_`.
fn canonicalize_secret_key(raw: &str) -> String {
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

/// Absent, empty, or `default` team collapses to the `_` segment.
fn normalize_team_segment(team: Option<&str>) -> String {
    match team
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("default"))
    {
        Some(value) => value.to_string(),
        None => "_".to_string(),
    }
}

/// Build the admin-compatible secret URI for an MCP component's key.
pub(crate) fn mcp_secret_uri(ctx: &TenantCtx, key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("secret key must not be empty".to_string());
    }
    let team = ctx.team_id.as_ref().or(ctx.team.as_ref());
    let team_segment = normalize_team_segment(team.map(|value| value.as_str()));
    Ok(format!(
        "secrets://{}/{}/{}/{}/{}",
        ctx.env.as_str(),
        ctx.tenant.as_str(),
        team_segment,
        MCP_CATEGORY,
        canonicalize_secret_key(key)
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
        assert_eq!(
            mcp_secret_uri(&ctx(), "EXAMPLE_KEY").unwrap(),
            "secrets://prod/acme/_/mcp/example_key"
        );
    }

    #[test]
    fn uri_canonicalizes_punctuation_in_key() {
        assert_eq!(
            mcp_secret_uri(&ctx(), "petstore-api.key").unwrap(),
            "secrets://prod/acme/_/mcp/petstore_api_key"
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
            &["secrets://prod/acme/_/mcp/example_key".to_string()]
        );
    }
}
