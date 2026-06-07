//! Redis-backed [`CheckpointStore`] implementation.
//!
//! Uses the same `aw:*` key namespace and [`redis::aio::ConnectionManager`]
//! conventions as [`crate::state_redis::RedisAgentStateStore`].
//!
//! # Key format
//!
//! [`crate::tenant::TenantContext::key_prefix()`] returns `"aw:{tenant_id}:{env_id}"`.
//!
//! - Run key:   `"aw:{tenant_id}:{env_id}:{run_id}:graph"`
//! - Visit key: `"aw:{tenant_id}:{env_id}:{run_id}:graph:visit:{node_id}:{attempt}"`
//!
//! Both keys carry a rolling 7-day TTL (refreshed on every write).

use std::future::Future;
use std::pin::Pin;

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::graph::checkpoint::{
    CheckpointError, CheckpointStore, GraphRunRecord, NodeVisitOutcome, check_segment,
};
use crate::tenant::TenantContext;

/// Graph-run checkpoint TTL — matches the conversation-state TTL in
/// [`crate::state_redis`].
const GRAPH_TTL_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

// ---------------------------------------------------------------------------
// Key helpers
// ---------------------------------------------------------------------------

fn run_key(t: &TenantContext, run_id: &str) -> String {
    format!("{}:{}:graph", t.key_prefix(), run_id)
}

fn visit_key(t: &TenantContext, run_id: &str, node_id: &str, attempt: u32) -> String {
    format!(
        "{}:{}:graph:visit:{}:{}",
        t.key_prefix(),
        run_id,
        node_id,
        attempt
    )
}

// ---------------------------------------------------------------------------
// RedisCheckpointStore
// ---------------------------------------------------------------------------

/// Production [`CheckpointStore`] backed by a multiplexed Redis
/// [`ConnectionManager`].
///
/// The manager is `Clone` (cheap, reference-counted) so per-call clones are
/// intentional and create no new connections.  All writes refresh a 7-day
/// rolling TTL so long-lived runs do not expire mid-flight.
pub struct RedisCheckpointStore {
    manager: ConnectionManager,
}

impl RedisCheckpointStore {
    /// Wrap an already-established connection manager.
    pub fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }

    /// Expose a clone of the underlying connection manager so callers can
    /// share the same multiplexed connection with other stores.
    pub fn manager(&self) -> ConnectionManager {
        self.manager.clone()
    }

    /// Open a client at `url` and establish a multiplexed connection.
    pub async fn connect(url: &str) -> Result<Self, CheckpointError> {
        let client =
            redis::Client::open(url).map_err(|e| CheckpointError::Backend(format!("open: {e}")))?;
        let manager = ConnectionManager::new(client)
            .await
            .map_err(|e| CheckpointError::Backend(format!("connect: {e}")))?;
        Ok(Self { manager })
    }
}

impl CheckpointStore for RedisCheckpointStore {
    fn load<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<GraphRunRecord>, CheckpointError>> + Send + 'a>>
    {
        Box::pin(async move {
            let key = run_key(tenant, run_id);
            let mut conn = self.manager.clone();
            let raw: Option<String> = conn
                .get(&key)
                .await
                .map_err(|e| CheckpointError::Backend(format!("get: {e}")))?;
            let Some(json) = raw else {
                return Ok(None);
            };
            let record: GraphRunRecord =
                serde_json::from_str(&json).map_err(CheckpointError::Serde)?;
            Ok(Some(record))
        })
    }

    fn save<'a>(
        &'a self,
        tenant: &'a TenantContext,
        rec: &'a GraphRunRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), CheckpointError>> + Send + 'a>> {
        Box::pin(async move {
            check_segment("run_id", &rec.run_id)?;
            let key = run_key(tenant, &rec.run_id);
            let json = serde_json::to_string(rec).map_err(CheckpointError::Serde)?;
            let mut conn = self.manager.clone();
            let _: () = conn
                .set_ex(&key, json, GRAPH_TTL_SECS)
                .await
                .map_err(|e| CheckpointError::Backend(format!("set_ex: {e}")))?;
            Ok(())
        })
    }

    fn record_node_visit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
        node_id: &'a str,
        attempt: u32,
        result: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<NodeVisitOutcome, CheckpointError>> + Send + 'a>> {
        Box::pin(async move {
            check_segment("run_id", run_id)?;
            check_segment("node_id", node_id)?;
            let key = visit_key(tenant, run_id, node_id, attempt);
            let value = serde_json::to_string(result).map_err(CheckpointError::Serde)?;
            let mut conn = self.manager.clone();
            // SET key value NX EX ttl — returns "OK" when written, nil when key exists.
            let res: Option<String> = redis::cmd("SET")
                .arg(&key)
                .arg(&value)
                .arg("NX")
                .arg("EX")
                .arg(GRAPH_TTL_SECS)
                .query_async(&mut conn)
                .await
                .map_err(|e| CheckpointError::Backend(format!("set nx: {e}")))?;
            if res.is_some() {
                // NX succeeded — we wrote the first result.
                return Ok(NodeVisitOutcome::Recorded);
            }
            // NX failed — key already exists; fetch the stored value.
            let existing_raw: Option<String> = conn
                .get(&key)
                .await
                .map_err(|e| CheckpointError::Backend(format!("get after nx miss: {e}")))?;
            let existing_json = existing_raw.ok_or_else(|| {
                CheckpointError::Backend(
                    "record_node_visit: NX reported key exists but GET returned nil \
                     (race or eviction)"
                        .into(),
                )
            })?;
            let existing: serde_json::Value =
                serde_json::from_str(&existing_json).map_err(CheckpointError::Serde)?;
            Ok(NodeVisitOutcome::Replayed(existing))
        })
    }

    fn load_node_visit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
        node_id: &'a str,
        attempt: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Option<serde_json::Value>, CheckpointError>> + Send + 'a>>
    {
        Box::pin(async move {
            let key = visit_key(tenant, run_id, node_id, attempt);
            let mut conn = self.manager.clone();
            let raw: Option<String> = conn
                .get(&key)
                .await
                .map_err(|e| CheckpointError::Backend(format!("get visit: {e}")))?;
            let Some(json) = raw else {
                return Ok(None);
            };
            let val: serde_json::Value =
                serde_json::from_str(&json).map_err(CheckpointError::Serde)?;
            Ok(Some(val))
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::graph::checkpoint::{GraphRunRecord, RunStatus};

    fn make_record(run_id: &str) -> GraphRunRecord {
        GraphRunRecord {
            run_id: run_id.into(),
            graph_json: "{}".into(),
            cursor: "agent".into(),
            state_json: "{}".into(),
            status: RunStatus::Running,
            visits_json: "{}".into(),
            frontier_json: None,
        }
    }

    async fn connect(url: &str) -> RedisCheckpointStore {
        RedisCheckpointStore::connect(url)
            .await
            .expect("redis connect")
    }

    #[tokio::test]
    async fn redis_checkpoint_round_trip_and_nx_semantics() {
        let Ok(url) = std::env::var("REDIS_URL") else {
            eprintln!("skipped: no REDIS_URL");
            return;
        };
        let store = connect(&url).await;

        // Use a timestamp-based suffix so reruns against a shared Redis
        // do not collide with stale data from a previous test run.
        let unique = uuid::Uuid::new_v4().to_string();
        let run_id = format!("test-run-{unique}");
        let node_id = "agent__node";

        let tenant = TenantContext::new("test-tenant", "test-env");

        // 1. load absent → None
        let loaded = store.load(&tenant, &run_id).await.unwrap();
        assert!(loaded.is_none(), "expected None for absent run");

        // 2. save + load round-trip (cursor and status preserved)
        let mut rec = make_record(&run_id);
        rec.cursor = "router".into();
        rec.status = RunStatus::Running;
        store.save(&tenant, &rec).await.unwrap();

        let loaded = store.load(&tenant, &run_id).await.unwrap().unwrap();
        assert_eq!(loaded.cursor, "router");
        assert_eq!(loaded.status, RunStatus::Running);

        // Overwrite and verify updated fields.
        let mut rec2 = rec.clone();
        rec2.cursor = "end".into();
        rec2.status = RunStatus::Succeeded;
        store.save(&tenant, &rec2).await.unwrap();

        let loaded2 = store.load(&tenant, &run_id).await.unwrap().unwrap();
        assert_eq!(loaded2.cursor, "end");
        assert_eq!(loaded2.status, RunStatus::Succeeded);

        // 3. record_node_visit twice (same key, different values) → Recorded then Replayed(first)
        let first_val = serde_json::json!({"reply": "first-answer"});
        let second_val = serde_json::json!({"reply": "second-answer-should-not-win"});

        let outcome1 = store
            .record_node_visit(&tenant, &run_id, node_id, 0, &first_val)
            .await
            .unwrap();
        assert_eq!(
            outcome1,
            NodeVisitOutcome::Recorded,
            "first write must be Recorded"
        );

        let outcome2 = store
            .record_node_visit(&tenant, &run_id, node_id, 0, &second_val)
            .await
            .unwrap();
        assert_eq!(
            outcome2,
            NodeVisitOutcome::Replayed(first_val.clone()),
            "second write must replay the first value"
        );

        // 4. load_node_visit returns the recorded (first) value
        let loaded_visit = store
            .load_node_visit(&tenant, &run_id, node_id, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded_visit, first_val,
            "load_node_visit must return the first-written value"
        );

        // 5. tenant isolation: a different env_id cannot see this run
        let other_tenant = TenantContext::new("test-tenant", "other-env");
        let isolated = store.load(&other_tenant, &run_id).await.unwrap();
        assert!(
            isolated.is_none(),
            "different env_id must not see the run from the original tenant"
        );

        // 6. save with run_id containing ':' → Err(Backend)
        let bad_rec = make_record(&format!("bad:run:{unique}"));
        let err = store.save(&tenant, &bad_rec).await.unwrap_err();
        assert!(
            matches!(err, CheckpointError::Backend(ref msg) if msg.contains("run_id")),
            "expected Backend error mentioning run_id, got {err:?}"
        );
    }
}
