//! Long-term (episodic) memory wiring for the agentic-worker runtime.
//!
//! The long-term tier uses the episodic [`LongTermMemory`] contract from
//! `greentic-dw-memory-long-term` (ingest episodes, semantic recall of facts) —
//! a deliberately different shape from the key-value
//! [`crate::memory::MemoryProvider`] seam, which suits the short-term/working
//! tier. The concrete backend (Chronicle / graphiti over a graph store) is
//! injected at the runner-host edge; this crate depends only on the lightweight
//! trait so it stays buildable without graph infrastructure.

use crate::tenant::TenantContext;
use greentic_types::{EnvId, TenantCtx, TenantId};

pub use greentic_dw_memory_long_term::{
    EpisodeIngest, EpisodeSource, IngestOutcome, LongTermMemory, LongTermMemoryError, RecallQuery,
    RecalledFact,
};

/// Convert the runtime's [`TenantContext`] into the `greentic-types`
/// [`TenantCtx`] expected by [`LongTermMemory`]. Validation failures (an id that
/// doesn't satisfy the shared tenant/env format) map to
/// [`LongTermMemoryError::InvalidTenant`] rather than panicking.
pub(crate) fn to_types_tenant(ctx: &TenantContext) -> Result<TenantCtx, LongTermMemoryError> {
    let env = EnvId::try_from(ctx.env_id.as_str())
        .map_err(|e| LongTermMemoryError::InvalidTenant(format!("env_id '{}': {e}", ctx.env_id)))?;
    let tenant = TenantId::try_from(ctx.tenant_id.as_str()).map_err(|e| {
        LongTermMemoryError::InvalidTenant(format!("tenant_id '{}': {e}", ctx.tenant_id))
    })?;
    Ok(TenantCtx::new(env, tenant))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn to_types_tenant_maps_valid_ids() {
        let ctx = TenantContext::new("acme", "dev");
        let resolved = to_types_tenant(&ctx).expect("valid tenant converts");
        // Round-trips back to the same string ids.
        assert_eq!(resolved.tenant.as_str(), "acme");
        assert_eq!(resolved.env.as_str(), "dev");
    }

    #[test]
    fn to_types_tenant_rejects_empty_tenant() {
        let ctx = TenantContext::new("", "dev");
        let err = to_types_tenant(&ctx).expect_err("empty tenant id is invalid");
        assert!(matches!(err, LongTermMemoryError::InvalidTenant(_)));
    }

    // A minimal in-crate `LongTermMemory` to prove the trait + tenant conversion
    // line up end-to-end without a graph backend (the real backend is Chronicle,
    // injected at the runner-host edge and exercised in integration tests).
    struct CountingMemory;

    #[async_trait::async_trait]
    impl LongTermMemory for CountingMemory {
        async fn ingest_episode(
            &self,
            _tenant: &TenantCtx,
            episode: EpisodeIngest,
        ) -> Result<IngestOutcome, LongTermMemoryError> {
            Ok(IngestOutcome {
                episode_id: format!("ep-{}", episode.name),
                fact_count: 1,
                entity_count: 1,
            })
        }

        async fn recall(
            &self,
            _tenant: &TenantCtx,
            query: RecallQuery,
        ) -> Result<Vec<RecalledFact>, LongTermMemoryError> {
            Ok(vec![RecalledFact {
                fact: format!("recalled for: {}", query.query),
                relation: "about".into(),
                valid_at: None,
                invalid_at: None,
                source_episode_ids: vec![],
            }])
        }
    }

    #[tokio::test]
    async fn trait_object_drives_ingest_and_recall_through_converted_tenant() {
        let memory: Arc<dyn LongTermMemory> = Arc::new(CountingMemory);
        let ctx = to_types_tenant(&TenantContext::new("acme", "dev")).unwrap();

        let outcome = memory
            .ingest_episode(
                &ctx,
                EpisodeIngest {
                    name: "turn-1".into(),
                    body: "Alice prefers dark mode".into(),
                    source: EpisodeSource::Message,
                    source_description: None,
                    reference_time: chrono::Utc::now(),
                },
            )
            .await
            .unwrap();
        assert_eq!(outcome.episode_id, "ep-turn-1");

        let facts = memory
            .recall(
                &ctx,
                RecallQuery {
                    query: "preferences".into(),
                    limit: Some(3),
                },
            )
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].fact, "recalled for: preferences");
    }
}
