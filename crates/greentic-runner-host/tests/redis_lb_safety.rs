#[cfg(feature = "session-redis")]
mod redis_lb {
    use std::env;
    use std::sync::Arc;

    use anyhow::Result;
    use greentic_runner_host::engine::host::SessionKey;
    use greentic_runner_host::engine::runtime::{FlowResumeStore, IngressEnvelope};
    use greentic_runner_host::runner::engine::{ExecutionState, FlowSnapshot, FlowWait};
    use greentic_runner_host::storage::{DynStateStore, state_host_from};
    use greentic_session::{SessionBackendConfig, create_session_store};
    use greentic_state::redis_store::RedisStateStore;
    use greentic_types::{EnvId, ReplyScope, TenantCtx, TenantId};
    use serde_json::json;

    fn envelope() -> IngressEnvelope {
        IngressEnvelope {
            tenant: "demo".into(),
            env: Some("local".into()),
            pack_id: Some("pack.redis".into()),
            flow_id: "flow.main".into(),
            flow_type: Some("messaging".into()),
            action: Some("messaging".into()),
            session_hint: Some("demo:provider:chan:conv:user".into()),
            provider: Some("provider".into()),
            channel: Some("conv".into()),
            conversation: Some("conv".into()),
            user: Some("user".into()),
            activity_id: Some("activity-redis".into()),
            timestamp: None,
            payload: json!({ "text": "hi" }),
            metadata: None,
            reply_scope: Some(ReplyScope {
                conversation: "conv".into(),
                thread: None,
                reply_to: None,
                correlation: None,
            }),
        }
        .canonicalize()
    }

    fn wait_snapshot(next_node: &str) -> FlowWait {
        let state: ExecutionState = serde_json::from_value(json!({
            "input": { "text": "hi" },
            "nodes": {},
            "egress": []
        }))
        .expect("state");
        FlowWait {
            reason: Some("await-user".into()),
            snapshot: FlowSnapshot {
                pack_id: "pack.redis".into(),
                flow_id: "flow.main".into(),
                next_flow: None,
                next_node: next_node.into(),
                state,
            },
        }
    }

    fn redis_url() -> Result<String> {
        match env::var("REDIS_URL") {
            Ok(value) => Ok(value),
            Err(_) => Ok(String::new()),
        }
    }

    fn resume_store(redis_url: &str) -> Result<FlowResumeStore> {
        let store = create_session_store(SessionBackendConfig::RedisUrl(redis_url.to_string()))?;
        Ok(FlowResumeStore::new(Arc::from(store)))
    }

    #[tokio::test]
    async fn redis_resume_survives_restart() -> Result<()> {
        let redis_url = redis_url()?;
        if redis_url.trim().is_empty() {
            eprintln!("REDIS_URL not set; skipping redis resume test");
            return Ok(());
        }

        let envelope = envelope();
        let wait = wait_snapshot("node-a");
        let store_a = resume_store(&redis_url)?;
        let _ = store_a.save(&envelope, &wait)?;

        let store_b = resume_store(&redis_url)?;
        let snapshot = store_b.fetch(&envelope)?.expect("snapshot missing");
        assert_eq!(snapshot.next_node, "node-a");
        store_b.clear(&envelope)?;

        let env = EnvId::new("local")?;
        let tenant = TenantId::new("redis-test")?;
        let ctx = TenantCtx::new(env, tenant);
        let key = SessionKey::new(&ctx, "pack.redis", "flow.main", Some("session".into()));

        let state_store: DynStateStore = Arc::new(RedisStateStore::from_url(&redis_url)?);
        let state_host = state_host_from(state_store);
        state_host.set_json(&key, json!({"value": 1})).await?;

        let state_store_restart: DynStateStore = Arc::new(RedisStateStore::from_url(&redis_url)?);
        let state_host_restart = state_host_from(state_store_restart);
        let value = state_host_restart.get_json(&key).await?;
        assert_eq!(value, Some(json!({"value": 1})));
        state_host_restart.del(&key).await?;
        Ok(())
    }
}

#[cfg(not(feature = "session-redis"))]
#[test]
fn redis_resume_survives_restart_disabled() {
    eprintln!("session-redis feature disabled; skipping redis resume test");
}
