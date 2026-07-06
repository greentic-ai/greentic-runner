//! Integration test for the Redis-backed idempotency ledger against a
//! real Redis instance.
//!
//! Skips gracefully when `REDIS_URL` is unset. CI sets
//! `REDIS_URL=redis://localhost:6379/15`.

use greentic_aw_runtime::state_redis::RedisAgentStateStore;
use greentic_aw_runtime::tenant::TenantContext;
use greentic_aw_runtime::tools::{RedisToolLedger, ToolLedger};

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

#[tokio::test]
async fn redis_ledger_records_and_reuses_result_no_redispatch() {
    // Acceptance §8.5 #10: the ledger records a tool result by call_id so
    // a `step()` replay reuses it instead of re-dispatching the side effect.
    let Some(url) = redis_url() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };
    let store = RedisAgentStateStore::connect(&url).await.unwrap();
    let ledger = RedisToolLedger::new(store.manager());
    let tenant = TenantContext::new("acme", "prod");
    let session = format!("ledger-{}", uuid::Uuid::new_v4());
    let call_id = "call-1";

    // Initially absent.
    assert!(
        ledger
            .get(&tenant, &session, call_id)
            .await
            .unwrap()
            .is_none()
    );

    // Record a result.
    let result = serde_json::json!({ "sent": true, "to": "a@b.com" });
    ledger
        .record(&tenant, &session, call_id, result.clone())
        .await
        .unwrap();

    // Now present + identical → a replay would reuse this instead of
    // re-dispatching.
    let got = ledger.get(&tenant, &session, call_id).await.unwrap();
    assert_eq!(got, Some(result));

    // A different call_id is independent.
    assert!(
        ledger
            .get(&tenant, &session, "call-2")
            .await
            .unwrap()
            .is_none()
    );
}
