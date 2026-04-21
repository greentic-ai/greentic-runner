use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use greentic_runner_host::secrets::{
    DynSecretsManager, read_secret_blocking, scoped_secret_path_for_pack,
};
use greentic_secrets_lib::{SecretError, SecretsManager};
use greentic_types::{EnvId, TenantCtx, TenantId, UserId};
use parking_lot::RwLock;

const TEST_PACK_ID: &str = "scoping";

#[derive(Default)]
struct MemorySecretsManager {
    values: RwLock<HashMap<String, Vec<u8>>>,
}

#[async_trait]
impl SecretsManager for MemorySecretsManager {
    async fn read(&self, path: &str) -> Result<Vec<u8>, SecretError> {
        self.values
            .read()
            .get(path)
            .cloned()
            .ok_or_else(|| SecretError::NotFound(path.to_string()))
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), SecretError> {
        self.values.write().insert(path.to_string(), bytes.to_vec());
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<(), SecretError> {
        self.values.write().remove(path);
        Ok(())
    }
}

fn tenant_ctx(env: &str, tenant: &str, user: Option<&str>) -> TenantCtx {
    let env_id = EnvId::new(env).expect("env");
    let tenant_id = TenantId::new(tenant).expect("tenant");
    let mut ctx = TenantCtx::new(env_id, tenant_id);
    if let Some(user) = user {
        let user_id = UserId::new(user).expect("user");
        ctx = ctx.with_user(Some(user_id));
    }
    ctx
}

#[test]
fn tenant_scoped_secret_reads_do_not_cross() -> Result<()> {
    let manager_impl: Arc<MemorySecretsManager> = Arc::new(MemorySecretsManager::default());
    let manager: DynSecretsManager = manager_impl.clone();
    let key = "API_KEY";
    let ctx_a = tenant_ctx("local", "tenant-a", None);
    let ctx_b = tenant_ctx("local", "tenant-b", None);

    let scoped_a = scoped_secret_path_for_pack(&ctx_a, TEST_PACK_ID, key)?;
    manager_impl
        .values
        .write()
        .insert(scoped_a.clone(), b"alpha".to_vec());

    let value = read_secret_blocking(&manager, &ctx_a, TEST_PACK_ID, key)?;
    assert_eq!(value, b"alpha".to_vec());
    assert!(read_secret_blocking(&manager, &ctx_b, TEST_PACK_ID, key).is_err());
    Ok(())
}

#[test]
fn user_context_does_not_change_provider_secret_scope() -> Result<()> {
    let manager_impl: Arc<MemorySecretsManager> = Arc::new(MemorySecretsManager::default());
    let manager: DynSecretsManager = manager_impl.clone();
    let key = "SESSION_TOKEN";
    let ctx = tenant_ctx("local", "tenant-a", Some("user-1"));

    let scoped = scoped_secret_path_for_pack(&ctx, TEST_PACK_ID, key)?;
    manager_impl
        .values
        .write()
        .insert(scoped.clone(), b"token".to_vec());

    let value = read_secret_blocking(&manager, &ctx, TEST_PACK_ID, key)?;
    assert_eq!(value, b"token".to_vec());
    Ok(())
}
