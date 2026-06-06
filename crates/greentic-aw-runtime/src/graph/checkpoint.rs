//! Durable-checkpoint abstraction for agent-graph runs.
//!
//! Provides:
//! - [`GraphRunRecord`] — serialisable snapshot of one graph run.
//! - [`CheckpointStore`] — object-safe async trait (manual `Pin<Box<dyn Future>>` returns,
//!   same pattern as [`crate::state::AgentStateStore`]).
//! - [`InMemoryCheckpointStore`] — std::sync::Mutex-backed implementation for tests
//!   and designer swap. The Redis implementation ships in Task 6.
//!
//! # Key format and segment constraints
//!
//! Key segments (`tenant_id`, `env_id`, `run_id`, `node_id`) **MUST NOT contain `':'`**.
//! Using `':'` as a delimiter makes keys unambiguous only when no segment itself contains
//! the delimiter.  Run-id producers use `'__'` as their internal word separator to avoid
//! conflicts.
//!
//! Key format for this in-memory implementation:
//! - Run key:   `"{tenant_id}:{env_id}:{run_id}"`
//! - Visit key: `"{tenant_id}:{env_id}:{run_id}:{node_id}:{attempt}"`
//!
//! The Redis implementation (Task 6) **must additionally** prepend the crate's `aw:` namespace
//! prefix, consistent with [`crate::state_redis::RedisAgentStateStore`] which uses
//! [`crate::tenant::TenantContext::key_prefix()`] (returning `"aw:{tenant_id}:{env_id}"`).
//! This yields Redis keys of the form:
//! - Run key:   `"aw:{tenant_id}:{env_id}:{run_id}"`
//! - Visit key: `"aw:{tenant_id}:{env_id}:{run_id}:{node_id}:{attempt}"`

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::tenant::TenantContext;

// ---------------------------------------------------------------------------
// RunStatus
// ---------------------------------------------------------------------------

/// Current lifecycle state of a graph run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

// ---------------------------------------------------------------------------
// GraphRunRecord
// ---------------------------------------------------------------------------

/// Durable snapshot of one graph run.
///
/// `graph_json` is immutable for the run's lifetime — republishing a graph
/// never mutates an in-flight run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphRunRecord {
    pub run_id: String,
    /// Serialised [`crate::graph::GraphConfig`] captured at run creation.
    pub graph_json: String,
    /// The node id the executor should resume from on restart.
    pub cursor: String,
    /// Serialised [`crate::graph::GraphRunState`].
    pub state_json: String,
    pub status: RunStatus,
    /// Serialised `HashMap<String, u32>` tracking per-node attempt counts.
    pub visits_json: String,
}

// ---------------------------------------------------------------------------
// CheckpointError
// ---------------------------------------------------------------------------

/// Errors produced by [`CheckpointStore`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("checkpoint backend error: {0}")]
    Backend(String),
    #[error("checkpoint serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// NodeVisitOutcome
// ---------------------------------------------------------------------------

/// Outcome of [`CheckpointStore::record_node_visit`].
///
/// - `Recorded`  — first write won; the result was stored.
/// - `Replayed`  — a result for this `(run, node, attempt)` triple already
///   existed; returns the original value so the executor can skip re-running.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeVisitOutcome {
    Recorded,
    Replayed(serde_json::Value),
}

// ---------------------------------------------------------------------------
// CheckpointStore trait
// ---------------------------------------------------------------------------

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Durable storage for graph-run snapshots and per-node visit records.
///
/// Dyn-safe: every method returns `Pin<Box<dyn Future…>>` (same pattern as
/// [`crate::state::AgentStateStore`]).
pub trait CheckpointStore: Send + Sync {
    /// Load the run record for `run_id`, if any.
    fn load<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
    ) -> BoxFut<'a, Result<Option<GraphRunRecord>, CheckpointError>>;

    /// Persist (create or overwrite) the run record.
    fn save<'a>(
        &'a self,
        tenant: &'a TenantContext,
        rec: &'a GraphRunRecord,
    ) -> BoxFut<'a, Result<(), CheckpointError>>;

    /// Insert-if-absent a node visit result, keyed by `(run_id, node_id, attempt)`.
    ///
    /// Returns [`NodeVisitOutcome::Recorded`] on first write,
    /// [`NodeVisitOutcome::Replayed`] with the original value on subsequent
    /// calls for the same key (enabling idempotent resume).
    fn record_node_visit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
        node_id: &'a str,
        attempt: u32,
        result: &'a serde_json::Value,
    ) -> BoxFut<'a, Result<NodeVisitOutcome, CheckpointError>>;

    /// Load a previously recorded node visit result, or `None` if absent.
    fn load_node_visit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
        node_id: &'a str,
        attempt: u32,
    ) -> BoxFut<'a, Result<Option<serde_json::Value>, CheckpointError>>;
}

// ---------------------------------------------------------------------------
// Key helpers
// ---------------------------------------------------------------------------

/// Reject any key segment that contains `':'`.
///
/// See module-level doc for the key-segment contract.
fn check_segment(name: &str, value: &str) -> Result<(), CheckpointError> {
    if value.contains(':') {
        Err(CheckpointError::Backend(format!(
            "invalid key segment: {name} '{value}' must not contain ':'"
        )))
    } else {
        Ok(())
    }
}

fn run_key(tenant: &TenantContext, run_id: &str) -> String {
    format!("{}:{}:{}", tenant.tenant_id, tenant.env_id, run_id)
}

fn visit_key(tenant: &TenantContext, run_id: &str, node_id: &str, attempt: u32) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        tenant.tenant_id, tenant.env_id, run_id, node_id, attempt
    )
}

// ---------------------------------------------------------------------------
// InMemoryCheckpointStore
// ---------------------------------------------------------------------------

/// In-memory [`CheckpointStore`] backed by `std::sync::Mutex`.
///
/// Intended for tests and the designer development swap. Not suitable for
/// multi-process deployments — use the Redis implementation for production.
///
/// Lock scopes are kept as short as possible (acquire → clone/insert → drop).
/// Poisoned locks are surfaced as [`CheckpointError::Backend`] rather than
/// unwrapped.
#[derive(Debug, Default)]
pub struct InMemoryCheckpointStore {
    runs: Mutex<HashMap<String, GraphRunRecord>>,
    visits: Mutex<HashMap<String, serde_json::Value>>,
}

impl CheckpointStore for InMemoryCheckpointStore {
    fn load<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
    ) -> BoxFut<'a, Result<Option<GraphRunRecord>, CheckpointError>> {
        Box::pin(async move {
            let key = run_key(tenant, run_id);
            let guard = self
                .runs
                .lock()
                .map_err(|e| CheckpointError::Backend(format!("lock poisoned: {e}")))?;
            Ok(guard.get(&key).cloned())
        })
    }

    fn save<'a>(
        &'a self,
        tenant: &'a TenantContext,
        rec: &'a GraphRunRecord,
    ) -> BoxFut<'a, Result<(), CheckpointError>> {
        Box::pin(async move {
            check_segment("run_id", &rec.run_id)?;
            let key = run_key(tenant, &rec.run_id);
            let mut guard = self
                .runs
                .lock()
                .map_err(|e| CheckpointError::Backend(format!("lock poisoned: {e}")))?;
            guard.insert(key, rec.clone());
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
    ) -> BoxFut<'a, Result<NodeVisitOutcome, CheckpointError>> {
        Box::pin(async move {
            check_segment("run_id", run_id)?;
            check_segment("node_id", node_id)?;
            let key = visit_key(tenant, run_id, node_id, attempt);
            let mut guard = self
                .visits
                .lock()
                .map_err(|e| CheckpointError::Backend(format!("lock poisoned: {e}")))?;
            if let Some(existing) = guard.get(&key) {
                Ok(NodeVisitOutcome::Replayed(existing.clone()))
            } else {
                guard.insert(key, result.clone());
                Ok(NodeVisitOutcome::Recorded)
            }
        })
    }

    fn load_node_visit<'a>(
        &'a self,
        tenant: &'a TenantContext,
        run_id: &'a str,
        node_id: &'a str,
        attempt: u32,
    ) -> BoxFut<'a, Result<Option<serde_json::Value>, CheckpointError>> {
        Box::pin(async move {
            let key = visit_key(tenant, run_id, node_id, attempt);
            let guard = self
                .visits
                .lock()
                .map_err(|e| CheckpointError::Backend(format!("lock poisoned: {e}")))?;
            Ok(guard.get(&key).cloned())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_record(run_id: &str) -> GraphRunRecord {
        GraphRunRecord {
            run_id: run_id.into(),
            graph_json: "{}".into(),
            cursor: "agent".into(),
            state_json: "{}".into(),
            status: RunStatus::Running,
            visits_json: "{}".into(),
        }
    }

    #[tokio::test]
    async fn record_node_visit_is_insert_if_absent() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");
        let first = store
            .record_node_visit(&t, "r1", "agent", 1, &serde_json::json!({"reply": "a"}))
            .await
            .unwrap();
        assert_eq!(first, NodeVisitOutcome::Recorded);

        let second = store
            .record_node_visit(
                &t,
                "r1",
                "agent",
                1,
                &serde_json::json!({"reply": "DIFFERENT"}),
            )
            .await
            .unwrap();
        assert_eq!(
            second,
            NodeVisitOutcome::Replayed(serde_json::json!({"reply": "a"}))
        );
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");

        assert!(store.load(&t, "r1").await.unwrap().is_none());

        let rec = make_record("r1");
        store.save(&t, &rec).await.unwrap();

        let loaded = store.load(&t, "r1").await.unwrap().unwrap();
        assert_eq!(loaded.cursor, "agent");
        assert_eq!(loaded.status, RunStatus::Running);
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let store = InMemoryCheckpointStore::default();
        let t1 = TenantContext::new("t1", "dev");
        let t2 = TenantContext::new("t2", "dev");

        let rec = make_record("r1");
        store.save(&t1, &rec).await.unwrap();

        assert!(store.load(&t2, "r1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_node_visit_absent_returns_none() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");
        let result = store.load_node_visit(&t, "r1", "agent", 1).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn load_node_visit_after_record_returns_value() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");
        let val = serde_json::json!({"answer": 42});

        store
            .record_node_visit(&t, "r1", "node_a", 0, &val)
            .await
            .unwrap();

        let loaded = store
            .load_node_visit(&t, "r1", "node_a", 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, val);
    }

    #[tokio::test]
    async fn node_visits_are_attempt_scoped() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");

        store
            .record_node_visit(&t, "r1", "agent", 0, &serde_json::json!({"v": 0}))
            .await
            .unwrap();
        store
            .record_node_visit(&t, "r1", "agent", 1, &serde_json::json!({"v": 1}))
            .await
            .unwrap();

        let v0 = store
            .load_node_visit(&t, "r1", "agent", 0)
            .await
            .unwrap()
            .unwrap();
        let v1 = store
            .load_node_visit(&t, "r1", "agent", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v0["v"], 0);
        assert_eq!(v1["v"], 1);
    }

    #[tokio::test]
    async fn save_overwrites_existing_run() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");

        let rec1 = make_record("r1");
        store.save(&t, &rec1).await.unwrap();

        let rec2 = GraphRunRecord {
            run_id: "r1".into(),
            graph_json: "{}".into(),
            cursor: "router".into(),
            state_json: "{}".into(),
            status: RunStatus::Succeeded,
            visits_json: "{}".into(),
        };
        store.save(&t, &rec2).await.unwrap();

        let loaded = store.load(&t, "r1").await.unwrap().unwrap();
        assert_eq!(loaded.cursor, "router");
        assert_eq!(loaded.status, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn run_status_serialises_lowercase() {
        let json = serde_json::to_string(&RunStatus::Succeeded).unwrap();
        assert_eq!(json, r#""succeeded""#);

        let back: RunStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RunStatus::Succeeded);
    }

    #[tokio::test]
    async fn save_rejects_colon_in_run_id() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");
        let rec = make_record("a:b");
        let err = store.save(&t, &rec).await.unwrap_err();
        assert!(
            matches!(err, CheckpointError::Backend(ref msg) if msg.contains("run_id")),
            "expected Backend error mentioning run_id, got {err:?}"
        );
    }

    #[tokio::test]
    async fn record_node_visit_rejects_colon_in_node_id() {
        let store = InMemoryCheckpointStore::default();
        let t = TenantContext::new("t1", "dev");
        let err = store
            .record_node_visit(&t, "r1", "x:y", 0, &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, CheckpointError::Backend(ref msg) if msg.contains("node_id")),
            "expected Backend error mentioning node_id, got {err:?}"
        );
    }
}
