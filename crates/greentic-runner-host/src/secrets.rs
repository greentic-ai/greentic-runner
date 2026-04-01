use std::sync::Arc;

use crate::runtime::block_on;
use anyhow::{Result, anyhow};
use greentic_secrets_lib::env::EnvSecretsManager;
use greentic_secrets_lib::{SecretScope, SecretsManager};
use greentic_types::TenantCtx;

/// Shared secrets manager handle used by the host.
pub type DynSecretsManager = Arc<dyn SecretsManager>;

/// Supported secrets backend kinds recognised by the runner.
#[derive(Clone, Debug)]
pub enum SecretsBackend {
    Env,
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
            other => Err(anyhow!("unsupported SECRETS_BACKEND `{other}`")),
        }
    }

    pub fn from_config(cfg: &greentic_config_types::SecretsBackendRefConfig) -> Result<Self> {
        match cfg.kind.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "env" => Ok(SecretsBackend::Env),
            other => Err(anyhow!("unsupported secrets backend `{other}`")),
        }
    }

    pub fn build_manager(&self) -> Result<DynSecretsManager> {
        match self {
            SecretsBackend::Env => {
                ensure_env_secrets_allowed()?;
                Ok(Arc::new(EnvSecretsManager) as DynSecretsManager)
            }
        }
    }
}

pub fn default_manager() -> Result<DynSecretsManager> {
    SecretsBackend::Env.build_manager()
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

pub fn scoped_secret_path_for_pack(ctx: &TenantCtx, pack_id: &str, key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(anyhow!("secret key must not be empty"));
    }
    let safe_key = key.replace('/', ".").replace(' ', "_");
    let user = ctx.user_id.as_ref().or(ctx.user.as_ref());
    let name = if let Some(user_id) = user {
        format!("user.{}.{}", user_id.as_str(), safe_key)
    } else {
        safe_key
    };
    let team = ctx.team_id.as_ref().or(ctx.team.as_ref());
    let scope = SecretScope {
        env: ctx.env.as_str().to_string(),
        tenant: ctx.tenant.as_str().to_string(),
        team: team.map(|value| value.as_str().to_string()),
    };
    let team_segment = scope.team.as_deref().unwrap_or("_");
    let pack_segment = pack_id.trim();
    if pack_segment.is_empty() {
        return Err(anyhow!("pack_id must not be empty for scoped secrets"));
    }
    let pack_segment = normalize_pack_segment(pack_segment);
    Ok(format!(
        "secrets://{}/{}/{}/{}/{}",
        scope.env, scope.tenant, team_segment, pack_segment, name
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
            "secrets://local/tenant-a/team-a/my_pack_prod/user.user-a.API.KEY_value"
        );
    }

    #[test]
    fn scoped_secret_path_rejects_empty_inputs() {
        let ctx = tenant_ctx();
        assert!(scoped_secret_path_for_pack(&ctx, "demo", "   ").is_err());
        assert!(scoped_secret_path_for_pack(&ctx, "   ", "key").is_err());
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
}
