//! Integration test for RedisTokenMeter against a real Redis.

use greentic_aw_runtime::cost::{RedisTokenMeter, TokenMeter};
use greentic_aw_runtime::tenant::TenantContext;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

#[tokio::test]
async fn add_then_current_accumulates_per_tenant_day() {
    let Some(url) = redis_url() else {
        return;
    };
    let client = redis::Client::open(url.as_str()).unwrap();
    let manager = redis::aio::ConnectionManager::new(client).await.unwrap();
    let meter = RedisTokenMeter::new(manager);
    // Unique tenant per run to avoid cross-run accumulation.
    let tc = TenantContext::new(format!("cost-{}", uuid::Uuid::new_v4()), "test");

    let start = meter.current(&tc).await.unwrap();
    assert_eq!(start, 0, "fresh tenant starts at 0");

    meter.add(&tc, 100).await.unwrap();
    meter.add(&tc, 50).await.unwrap();
    let total = meter.current(&tc).await.unwrap();
    assert_eq!(total, 150);
}
