use super::super::error::GResult;
use super::super::host::{SessionKey, StateHost};
use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct InMemoryStateHost {
    store: RwLock<HashMap<String, Value>>,
}

impl InMemoryStateHost {
    pub fn new() -> Self {
        Self {
            store: RwLock::new(HashMap::new()),
        }
    }

    fn key_of(session_key: &SessionKey) -> String {
        format!(
            "{}:{}:{}",
            session_key.tenant_key, session_key.pack_id, session_key.flow_id
        )
    }
}

#[async_trait]
impl StateHost for InMemoryStateHost {
    async fn get_json(&self, key: &SessionKey) -> GResult<Option<Value>> {
        Ok(self.store.read().get(&Self::key_of(key)).cloned())
    }

    async fn set_json(&self, key: &SessionKey, value: Value) -> GResult<()> {
        self.store.write().insert(Self::key_of(key), value);
        Ok(())
    }

    async fn del(&self, key: &SessionKey) -> GResult<()> {
        self.store.write().remove(&Self::key_of(key));
        Ok(())
    }

    async fn del_prefix(&self, key_prefix: &str) -> GResult<()> {
        let mut guard = self.store.write();
        let keys: Vec<String> = guard
            .keys()
            .filter(|k| k.starts_with(key_prefix))
            .cloned()
            .collect();
        for key in keys {
            guard.remove(&key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::host::SessionKey;
    use greentic_types::{EnvId, TenantCtx, TenantId};
    use serde_json::json;

    fn key(flow_id: &str) -> SessionKey {
        let tenant = TenantCtx::new(
            EnvId::new("local").expect("env"),
            TenantId::new("tenant-a").expect("tenant"),
        );
        SessionKey::new(&tenant, "pack-a", flow_id, None)
    }

    #[tokio::test]
    async fn state_host_round_trips_and_deletes_by_prefix() {
        let host = InMemoryStateHost::new();
        let first = key("flow-1");
        let second = key("flow-2");

        host.set_json(&first, json!({"count": 1}))
            .await
            .expect("set first");
        host.set_json(&second, json!({"count": 2}))
            .await
            .expect("set second");

        assert_eq!(
            host.get_json(&first).await.expect("get first"),
            Some(json!({"count": 1}))
        );

        host.del_prefix("local::tenant-a:pack-a:flow-")
            .await
            .expect("delete prefix");

        assert_eq!(host.get_json(&first).await.expect("get first"), None);
        assert_eq!(host.get_json(&second).await.expect("get second"), None);
    }

    #[tokio::test]
    async fn state_host_delete_removes_single_key() {
        let host = InMemoryStateHost::new();
        let key = key("flow-1");
        host.set_json(&key, json!({"ready": true}))
            .await
            .expect("set");

        host.del(&key).await.expect("delete");

        assert_eq!(host.get_json(&key).await.expect("get"), None);
    }
}
