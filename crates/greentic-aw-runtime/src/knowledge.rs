//! Knowledge / RAG (document-corpus) wiring for the agentic-worker runtime.
//!
//! Defines a local [`Knowledge`] seam — ingest pre-chunked document text,
//! hybrid-retrieve ranked chunks — kept deliberately distinct from
//! [`crate::long_term`] (D4): a read-mostly document corpus with auto
//! pre-retrieval, not evolving conversational memory, and from the key-value
//! [`crate::memory::MemoryProvider`] short-term tier.
//!
//! The trait + DTOs mirror the W3 `greentic-dw-knowledge` contract shape exactly,
//! but are defined locally rather than re-exported: the W3 trait crate pulls
//! `greentic-dw-providers-common` (catalog/pack machinery), which would drag a
//! conflicting `greentic-types` and the whole provider stack into this runtime
//! crate. Keeping a thin local seam lets the runtime stay free of graph/provider
//! weight; the concrete Chronicle-backed provider is adapted to this trait at the
//! runner-host edge (W4 4d). When W3 ships a lightweight trait-only crate (as the
//! memory tier does via `greentic-dw-memory-long-term`), this can re-export it.

use crate::AgentConfig;
use crate::tenant::TenantContext;
use greentic_types::{EnvId, TenantCtx, TenantId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A pre-chunked unit of document text to ingest into the knowledge corpus.
/// Mirrors `greentic_dw_knowledge::KnowledgeChunk`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    pub doc_id: String,
    pub chunk_index: usize,
    pub text: String,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Outcome of an ingest: the backend-assigned id for each stored chunk.
/// Mirrors `greentic_dw_knowledge::IngestOutcome`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IngestOutcome {
    pub chunk_ids: Vec<String>,
}

/// A retrieval query over the tenant's knowledge corpus.
/// Mirrors `greentic_dw_knowledge::KnowledgeQuery`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeQuery {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// A ranked chunk returned from retrieval.
/// Mirrors `greentic_dw_knowledge::RetrievedChunk`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetrievedChunk {
    pub text: String,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_index: Option<usize>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Errors returned by knowledge operations.
/// Mirrors `greentic_dw_knowledge::KnowledgeError`.
#[derive(Debug, Error)]
pub enum KnowledgeError {
    /// The underlying storage or RAG backend returned an error.
    #[error("knowledge backend error: {0}")]
    Backend(String),
    /// The tenant identifier is invalid or unknown.
    #[error("invalid tenant: {0}")]
    InvalidTenant(String),
    /// No knowledge backend has been configured.
    #[error("knowledge provider not configured")]
    NotConfigured,
}

/// Convenience result alias for knowledge operations.
pub type KnowledgeResult<T> = Result<T, KnowledgeError>;

/// Contract implemented by knowledge (document-RAG) backends. Object-safe so the
/// concrete backend can be injected as `Arc<dyn Knowledge>` at the runner-host
/// edge. Mirrors `greentic_dw_knowledge::Knowledge`.
#[async_trait::async_trait]
pub trait Knowledge: Send + Sync {
    /// Ingest a batch of pre-chunked document text into the tenant's corpus.
    async fn ingest(
        &self,
        tenant: &TenantCtx,
        chunks: Vec<KnowledgeChunk>,
    ) -> KnowledgeResult<IngestOutcome>;

    /// Retrieve chunks relevant to `query`, ranked by relevance descending and
    /// bounded by `query.limit`.
    async fn search(
        &self,
        tenant: &TenantCtx,
        query: KnowledgeQuery,
    ) -> KnowledgeResult<Vec<RetrievedChunk>>;
}

/// Convert the runtime's [`TenantContext`] into the `greentic-types`
/// [`TenantCtx`] expected by [`Knowledge`]. Validation failures (an id that
/// doesn't satisfy the shared tenant/env format) map to
/// [`KnowledgeError::InvalidTenant`] rather than panicking.
pub(crate) fn to_types_tenant(ctx: &TenantContext) -> Result<TenantCtx, KnowledgeError> {
    let env = EnvId::try_from(ctx.env_id.as_str())
        .map_err(|e| KnowledgeError::InvalidTenant(format!("env_id '{}': {e}", ctx.env_id)))?;
    let tenant = TenantId::try_from(ctx.tenant_id.as_str()).map_err(|e| {
        KnowledgeError::InvalidTenant(format!("tenant_id '{}': {e}", ctx.tenant_id))
    })?;
    Ok(TenantCtx::new(env, tenant))
}

/// Number of chunks auto-retrieved and injected each turn, read from the agent's
/// knowledge binding (falls back to the default, clamped to at least 1 so a stray
/// `0` never disables retrieval silently).
pub(crate) fn auto_top_k(config: &AgentConfig) -> usize {
    config
        .knowledge
        .as_ref()
        .map(|k| k.top_k)
        .unwrap_or_else(crate::config::default_knowledge_top_k)
        .max(1)
}

/// Whether the knowledge tier is active for this turn: a backend is wired AND the
/// agent's config carries an enabled knowledge provider binding.
pub(crate) fn knowledge_active(has_provider: bool, config: &AgentConfig) -> bool {
    has_provider
        && config
            .knowledge
            .as_ref()
            .and_then(|k| k.knowledge.as_ref())
            .is_some()
}

/// Build the system prompt for a turn: the base prompt followed by a delimited
/// `<knowledge>` block listing the retrieved chunks. Returns the base prompt
/// unchanged when there are no chunks (no empty block).
pub(crate) fn augment_system_prompt(base: &str, chunks: &[RetrievedChunk]) -> String {
    if chunks.is_empty() {
        return base.to_string();
    }
    let mut out = String::with_capacity(base.len() + 128 * chunks.len());
    out.push_str(base);
    out.push_str("\n\n<knowledge>\nRelevant passages retrieved from the agent's knowledge base:\n");
    for c in chunks {
        out.push_str("- ");
        out.push_str(c.text.trim());
        out.push('\n');
    }
    out.push_str("</knowledge>");
    out
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
        assert_eq!(resolved.tenant.as_str(), "acme");
        assert_eq!(resolved.env.as_str(), "dev");
    }

    #[test]
    fn to_types_tenant_rejects_empty_tenant() {
        let ctx = TenantContext::new("", "dev");
        let err = to_types_tenant(&ctx).expect_err("empty tenant id is invalid");
        assert!(matches!(err, KnowledgeError::InvalidTenant(_)));
    }

    fn cfg_with_knowledge(top_k: Option<usize>, with_binding: bool) -> AgentConfig {
        use crate::config::{KnowledgeSettings, MemoryProviderRef};
        let binding = with_binding.then(|| MemoryProviderRef {
            provider: "provider.knowledge.chronicle".into(),
            capability: "cap://dw.knowledge".into(),
            params: serde_json::Map::new(),
            credential_ref: None,
        });
        AgentConfig {
            agent_id: "a".into(),
            system_prompt: "s".into(),
            tools: vec![],
            llm: crate::LlmProviderRef {
                provider: "m".into(),
                model: "m".into(),
                credential_ref: None,
            },
            limits: crate::AgentLimits::default(),
            memory: None,
            knowledge: Some(KnowledgeSettings {
                knowledge: binding,
                embedding: None,
                top_k: top_k.unwrap_or_else(crate::config::default_knowledge_top_k),
            }),
            guardrails: vec![],
        }
    }

    #[test]
    fn knowledge_active_requires_provider_and_enabled_binding() {
        let cfg = cfg_with_knowledge(None, true);
        assert!(knowledge_active(true, &cfg));
        assert!(!knowledge_active(false, &cfg));

        // Binding present but no knowledge provider → inactive.
        let cfg_no_binding = cfg_with_knowledge(None, false);
        assert!(!knowledge_active(true, &cfg_no_binding));

        // No knowledge settings at all → inactive.
        let mut bare = cfg_with_knowledge(None, true);
        bare.knowledge = None;
        assert!(!knowledge_active(true, &bare));
    }

    #[test]
    fn auto_top_k_uses_config_then_default_clamped() {
        assert_eq!(auto_top_k(&cfg_with_knowledge(Some(3), true)), 3);
        assert_eq!(auto_top_k(&cfg_with_knowledge(None, true)), 5);
        // A stray 0 clamps up to 1 rather than disabling retrieval.
        assert_eq!(auto_top_k(&cfg_with_knowledge(Some(0), true)), 1);
        let mut bare = cfg_with_knowledge(None, true);
        bare.knowledge = None;
        assert_eq!(auto_top_k(&bare), 5);
    }

    fn chunk(text: &str, score: f64) -> RetrievedChunk {
        RetrievedChunk {
            text: text.into(),
            score,
            doc_id: None,
            chunk_index: None,
            metadata: serde_json::Map::new(),
        }
    }

    #[test]
    fn augment_with_chunks_wraps_a_block() {
        let chunks = vec![
            chunk("Refunds are processed within 5 business days.", 0.9),
            chunk("Premium plans include priority support.", 0.7),
        ];
        let out = augment_system_prompt("base prompt", &chunks);
        assert!(out.starts_with("base prompt"));
        assert!(out.contains("<knowledge>"));
        assert!(out.contains("</knowledge>"));
        assert!(out.contains("Refunds are processed within 5 business days."));
        assert!(out.contains("Premium plans include priority support."));
    }

    #[test]
    fn augment_with_no_chunks_returns_base_unchanged() {
        let out = augment_system_prompt("base prompt", &[]);
        assert_eq!(out, "base prompt");
    }

    // A minimal in-crate `Knowledge` to prove the trait + tenant conversion line
    // up end-to-end without a graph backend (the real backend is Chronicle doc-RAG,
    // injected at the runner-host edge and exercised in integration tests).
    struct StubKnowledge;

    #[async_trait::async_trait]
    impl Knowledge for StubKnowledge {
        async fn ingest(
            &self,
            _tenant: &TenantCtx,
            chunks: Vec<KnowledgeChunk>,
        ) -> KnowledgeResult<IngestOutcome> {
            Ok(IngestOutcome {
                chunk_ids: chunks
                    .iter()
                    .map(|c| format!("{}#{}", c.doc_id, c.chunk_index))
                    .collect(),
            })
        }

        async fn search(
            &self,
            _tenant: &TenantCtx,
            query: KnowledgeQuery,
        ) -> KnowledgeResult<Vec<RetrievedChunk>> {
            Ok(vec![chunk(&format!("retrieved for: {}", query.query), 1.0)])
        }
    }

    #[tokio::test]
    async fn trait_object_drives_ingest_and_search_through_converted_tenant() {
        let kb: Arc<dyn Knowledge> = Arc::new(StubKnowledge);
        let ctx = to_types_tenant(&TenantContext::new("acme", "dev")).unwrap();

        let outcome = kb
            .ingest(
                &ctx,
                vec![KnowledgeChunk {
                    doc_id: "faq".into(),
                    chunk_index: 0,
                    text: "Refunds within 5 days.".into(),
                    metadata: serde_json::Map::new(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(outcome.chunk_ids, vec!["faq#0".to_string()]);

        let hits = kb
            .search(
                &ctx,
                KnowledgeQuery {
                    query: "refund policy".into(),
                    limit: Some(3),
                },
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "retrieved for: refund policy");
    }
}
