//! Memory provider seam for the agentic-worker runtime.
//!
//! aw-runtime owns its own memory abstraction (it does not depend on
//! `greentic-dw-runtime`). A provider is bound per memory tier via
//! [`crate::config::MemorySettings`]; the runtime invokes [`MemoryProvider`]
//! to persist and retrieve records. This module ships the trait, the portable
//! record/query types, and an always-available in-memory provider used by
//! tests and the designer playground until the real extension-backed provider
//! lands.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::tenant::TenantContext;

/// A single memory record exchanged with a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub key: String,
    pub value: String,
}

/// Lookup query for [`MemoryProvider::recall`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQuery {
    pub key: String,
}

/// Backend abstraction for a memory tier. Mirrors the async-trait idiom used
/// by [`crate::state::AgentStateStore`] and [`crate::llm::LlmBackend`]
/// (manual `Pin<Box<dyn Future>>` so the trait stays object-safe).
pub trait MemoryProvider: Send + Sync {
    fn remember<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        record: MemoryRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send + 'a>>;

    fn recall<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        query: &'a MemoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Option<MemoryRecord>, MemoryError>> + Send + 'a>>;
}

/// In-memory provider keyed by `(tenant, session, key)`. Always compiled (like
/// [`crate::config_provider::InMemoryConfigProvider`]); used by tests and the
/// designer playground. Lock-poisoning maps to [`MemoryError::Backend`] — no
/// `unwrap`/`expect` in this non-test code.
#[derive(Default)]
pub struct InMemoryMemoryProvider {
    entries: Mutex<HashMap<(String, String, String), MemoryRecord>>,
}

impl InMemoryMemoryProvider {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl MemoryProvider for InMemoryMemoryProvider {
    fn remember<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        record: MemoryRecord,
    ) -> Pin<Box<dyn Future<Output = Result<(), MemoryError>> + Send + 'a>> {
        let key = (
            tenant.key_prefix(),
            session_id.to_string(),
            record.key.clone(),
        );
        let result = self
            .entries
            .lock()
            .map_err(|_| MemoryError::Backend("memory mutex poisoned".to_string()))
            .map(|mut entries| {
                entries.insert(key, record);
            });
        Box::pin(async move { result })
    }

    fn recall<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        query: &'a MemoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Option<MemoryRecord>, MemoryError>> + Send + 'a>> {
        let key = (
            tenant.key_prefix(),
            session_id.to_string(),
            query.key.clone(),
        );
        let result = self
            .entries
            .lock()
            .map_err(|_| MemoryError::Backend("memory mutex poisoned".to_string()))
            .map(|entries| entries.get(&key).cloned());
        Box::pin(async move { result })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tenant() -> TenantContext {
        TenantContext::new("acme", "prod")
    }

    #[tokio::test]
    async fn remember_then_recall_roundtrips() {
        let provider = InMemoryMemoryProvider::new();
        let t = tenant();
        provider
            .remember(
                &t,
                "sess-1",
                MemoryRecord {
                    key: "fav_color".to_string(),
                    value: "green".to_string(),
                },
            )
            .await
            .unwrap();

        let got = provider
            .recall(
                &t,
                "sess-1",
                &MemoryQuery {
                    key: "fav_color".to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(MemoryRecord {
                key: "fav_color".to_string(),
                value: "green".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn recall_missing_key_returns_none() {
        let provider = InMemoryMemoryProvider::new();
        let got = provider
            .recall(
                &tenant(),
                "sess-1",
                &MemoryQuery {
                    key: "nope".to_string(),
                },
            )
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn recall_is_isolated_across_tenants() {
        let provider = InMemoryMemoryProvider::new();
        provider
            .remember(
                &TenantContext::new("acme", "prod"),
                "sess-1",
                MemoryRecord {
                    key: "fav_color".to_string(),
                    value: "green".to_string(),
                },
            )
            .await
            .unwrap();

        // Same session id + key, different tenant -> must not leak.
        let other = provider
            .recall(
                &TenantContext::new("globex", "prod"),
                "sess-1",
                &MemoryQuery {
                    key: "fav_color".to_string(),
                },
            )
            .await
            .unwrap();
        assert!(other.is_none());
    }
}
