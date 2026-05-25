//! Integration tests against a real Redis instance.
//!
//! The whole module skips gracefully if `REDIS_URL` is not set.
//! CI sets `REDIS_URL=redis://localhost:6379/15` (DB 15 to avoid
//! stepping on shared dev state).

use greentic_aw_runtime::state::{AgentStateStore, ChatMessage, ConversationState};
use greentic_aw_runtime::state_redis::RedisAgentStateStore;
use greentic_aw_runtime::tenant::TenantContext;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

async fn make_store() -> RedisAgentStateStore {
    let url = redis_url().expect("REDIS_URL must be set for integration tests");
    RedisAgentStateStore::connect(&url)
        .await
        .expect("redis connect")
}

#[tokio::test]
#[allow(clippy::panic)] // diagnostic else-branch in test — intentional
async fn save_then_load_roundtrips_state() {
    let Some(_) = redis_url() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };
    let store = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("sess-{}", uuid::Uuid::new_v4());

    let mut state = ConversationState::empty(&tc, &session);
    state.messages.push(ChatMessage::User {
        content: "hello".into(),
    });
    store.save(&tc, &session, &state).await.unwrap();

    let loaded = store.load(&tc, &session).await.unwrap();
    assert_eq!(loaded.messages.len(), 1);
    if let ChatMessage::User { content } = &loaded.messages[0] {
        assert_eq!(content, "hello");
    } else {
        panic!("expected User message");
    }
}

#[tokio::test]
async fn load_returns_empty_state_when_no_record_exists() {
    let Some(_) = redis_url() else {
        return;
    };
    let store = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("never-existed-{}", uuid::Uuid::new_v4());

    let loaded = store.load(&tc, &session).await.unwrap();
    assert_eq!(loaded.schema_version, 1);
    assert_eq!(loaded.session_id, session);
    assert!(loaded.messages.is_empty());
}
