//! Operator-configured Knowledge (document-RAG) wiring for the agentic worker.
//! Compiled only with the `knowledge-chronicle` feature.
//!
//! This is the runner-host edge of the W4 Knowledge tier: it builds a concrete
//! Chronicle-backed knowledge provider and adapts it to the runtime's local
//! [`greentic_aw_runtime::knowledge::Knowledge`] seam (the runtime deliberately
//! keeps a thin local trait rather than depending on the provider stack). The
//! agentic loop then auto-retrieves from it each turn (see W4 4a/4b).
//!
//! Knowledge is a SEPARATE tier from long-term memory ([D4]): its own capability,
//! its own embedded store, its own env namespace (`GREENTIC_KNOWLEDGE_*`). It must
//! NOT share the embedded-SurrealDB path with long-term memory — two RocksDB
//! handles on one directory in the same process collide — so it defaults to a
//! distinct path. The graph store is operator-selectable via
//! `GREENTIC_KNOWLEDGE_BACKEND`, defaulting to embedded SurrealDB so document RAG
//! works with no external graph server. The embedding endpoint
//! (`GREENTIC_KNOWLEDGE_EMBED_*`, any OpenAI-compatible API) is the opt-in signal:
//! absent it — or on a config / connection failure — the knowledge tier simply
//! stays disabled; it never fails runtime construction.

use std::sync::Arc;

use chronicle_core::driver::GraphDriver;
use chronicle_driver_surreal::SurrealDriver;
use greentic_aw_runtime::AgentRuntime;
use greentic_aw_runtime::knowledge::{
    IngestOutcome as AwIngestOutcome, Knowledge as AwKnowledge, KnowledgeChunk as AwChunk,
    KnowledgeError as AwKnowledgeError, KnowledgeQuery as AwQuery, KnowledgeResult as AwResult,
    RetrievedChunk as AwRetrievedChunk,
};
use greentic_dw_knowledge::{
    IngestOutcome as DwIngestOutcome, Knowledge as DwKnowledge, KnowledgeChunk as DwChunk,
    KnowledgeQuery as DwQuery, RetrievedChunk as DwRetrievedChunk,
};
use greentic_dw_knowledge_chronicle::{KnowledgeChronicle, KnowledgeConfig};
use greentic_types::TenantCtx;

// Graph-store backend selector + per-backend connection (own namespace, distinct
// from the long-term-memory tier — they must not share an embedded store path).
const ENV_BACKEND: &str = "GREENTIC_KNOWLEDGE_BACKEND";
const ENV_SURREAL_PATH: &str = "GREENTIC_KNOWLEDGE_SURREAL_PATH";
/// Graph store used when `GREENTIC_KNOWLEDGE_BACKEND` is unset: embedded SurrealDB
/// on disk — no external graph server required.
const DEFAULT_BACKEND: &str = "surreal-embedded";
/// Default on-disk location for the embedded SurrealDB knowledge store. Distinct
/// from the long-term-memory default so the two tiers never open the same store.
const DEFAULT_SURREAL_PATH: &str = "/var/lib/greentic/knowledge";
// Embeddings (vector recall) — any OpenAI-compatible endpoint.
const ENV_EMBED_BASE_URL: &str = "GREENTIC_KNOWLEDGE_EMBED_BASE_URL";
const ENV_EMBED_API_KEY: &str = "GREENTIC_KNOWLEDGE_EMBED_API_KEY";
const ENV_EMBED_MODEL: &str = "GREENTIC_KNOWLEDGE_EMBED_MODEL";
const ENV_EMBED_DIM: &str = "GREENTIC_KNOWLEDGE_EMBED_DIM";

const DEFAULT_EMBEDDING_DIM: usize = 1024;

/// Attach an operator-configured Chronicle knowledge (document-RAG) backend to
/// `runtime`. The embedding endpoint env is the opt-in signal (Chronicle needs an
/// embedder to retrieve); the graph backend is selected by
/// `GREENTIC_KNOWLEDGE_BACKEND`, defaulting to embedded SurrealDB so no external
/// graph server is required. Returns the runtime unchanged when the environment is
/// incomplete or the connection fails.
pub async fn attach(runtime: AgentRuntime) -> AgentRuntime {
    let Some(config) = build_config() else {
        tracing::debug!(
            "knowledge: embedding endpoint env unset/invalid; knowledge (RAG) disabled"
        );
        return runtime;
    };
    let Some(driver) = build_driver(config.embedding_dim).await else {
        // build_driver logs the specific reason (unknown kind / connection error).
        return runtime;
    };

    match KnowledgeChronicle::from_config(&config, driver).await {
        Ok(knowledge) => {
            tracing::info!(
                "knowledge: Chronicle document-RAG attached (provider-neutral embeddings)"
            );
            runtime.with_knowledge(Arc::new(KnowledgeBridge(knowledge)))
        }
        Err(err) => {
            tracing::warn!(error = %err, "knowledge: Chronicle connect failed; knowledge disabled");
            runtime
        }
    }
}

/// Build the knowledge embedding config from the environment. The embedding
/// endpoint (base URL + key + model) is mandatory — its absence is how an operator
/// opts out — so a missing value returns `None` and leaves knowledge disabled.
fn build_config() -> Option<KnowledgeConfig> {
    let base_url = std::env::var(ENV_EMBED_BASE_URL).ok()?;
    let api_key = std::env::var(ENV_EMBED_API_KEY).ok()?;
    let model = std::env::var(ENV_EMBED_MODEL).ok()?;
    let embedding_dim = std::env::var(ENV_EMBED_DIM)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_EMBEDDING_DIM);

    let mut config = KnowledgeConfig::new(embedding_dim);
    config.openai_base_url = Some(base_url);
    config.openai_api_key = Some(api_key);
    config.embedding_model = Some(model);
    Some(config)
}

/// Resolve and connect the graph-store driver from the environment. Defaults to
/// embedded SurrealDB. Returns `None` — with a logged reason — when the selector
/// value is unknown, the backend isn't compiled into this build, or the
/// connection fails. `embedding_dim` sizes the vector index.
async fn build_driver(embedding_dim: usize) -> Option<Arc<dyn GraphDriver>> {
    let kind = std::env::var(ENV_BACKEND).unwrap_or_else(|_| DEFAULT_BACKEND.to_string());
    match kind.as_str() {
        "surreal-embedded" => {
            let path = std::env::var(ENV_SURREAL_PATH)
                .unwrap_or_else(|_| DEFAULT_SURREAL_PATH.to_string());
            match SurrealDriver::connect_embedded(&path, embedding_dim).await {
                Ok(driver) => Some(Arc::new(driver)),
                Err(err) => {
                    tracing::warn!(error = %err, path = %path, "knowledge: embedded SurrealDB connect failed; knowledge disabled");
                    None
                }
            }
        }
        "surreal-memory" => {
            tracing::warn!(
                "knowledge: backend=surreal-memory is EPHEMERAL — the ingested corpus is lost on \
                 restart and must be re-ingested"
            );
            match SurrealDriver::connect_memory(embedding_dim).await {
                Ok(driver) => Some(Arc::new(driver)),
                Err(err) => {
                    tracing::warn!(error = %err, "knowledge: in-memory SurrealDB connect failed; knowledge disabled");
                    None
                }
            }
        }
        other => {
            tracing::warn!(
                backend = %other,
                "knowledge: GREENTIC_KNOWLEDGE_BACKEND='{other}' is unknown or not compiled into \
                 this build (supported: surreal-embedded|surreal-memory); knowledge disabled",
            );
            None
        }
    }
}

/// Adapts the provider-stack [`KnowledgeChronicle`] (which implements the
/// `greentic-dw-knowledge` trait over its own DTOs) to the runtime's local
/// [`AwKnowledge`] seam. The two DTO families mirror each other field-for-field;
/// this bridge does the (trivial) conversion so the runtime crate stays free of
/// the provider/catalog dependency weight.
struct KnowledgeBridge(KnowledgeChronicle);

#[async_trait::async_trait]
impl AwKnowledge for KnowledgeBridge {
    async fn ingest(&self, tenant: &TenantCtx, chunks: Vec<AwChunk>) -> AwResult<AwIngestOutcome> {
        let dw_chunks: Vec<DwChunk> = chunks.into_iter().map(to_dw_chunk).collect();
        let DwIngestOutcome { chunk_ids } =
            self.0.ingest(tenant, dw_chunks).await.map_err(to_aw_err)?;
        Ok(AwIngestOutcome { chunk_ids })
    }

    async fn search(&self, tenant: &TenantCtx, query: AwQuery) -> AwResult<Vec<AwRetrievedChunk>> {
        let hits = self
            .0
            .search(
                tenant,
                DwQuery {
                    query: query.query,
                    limit: query.limit,
                },
            )
            .await
            .map_err(to_aw_err)?;
        Ok(hits
            .into_iter()
            .map(
                |DwRetrievedChunk {
                     text,
                     score,
                     doc_id,
                     chunk_index,
                     metadata,
                 }| AwRetrievedChunk {
                    text,
                    score,
                    doc_id,
                    chunk_index,
                    metadata,
                },
            )
            .collect())
    }
}

/// Map a provider-stack knowledge error onto the runtime's local error. Tenant
/// validation and "not configured" both surface as backend errors here because
/// the runtime seam only needs the message for its fail-degrade logging.
fn to_aw_err(err: greentic_dw_knowledge::KnowledgeError) -> AwKnowledgeError {
    AwKnowledgeError::Backend(err.to_string())
}

/// Convert a runtime knowledge chunk to the provider-stack DTO (field-for-field).
fn to_dw_chunk(c: AwChunk) -> DwChunk {
    DwChunk {
        doc_id: c.doc_id,
        chunk_index: c.chunk_index,
        text: c.text,
        metadata: c.metadata,
    }
}

/// First-boot ingest of a pack-baked knowledge corpus (W4 4c). Opens a temporary
/// knowledge connection, ingests `chunks` for `tenant`, then drops it — releasing
/// the embedded-store lock before the serving mount ([`attach`]) opens its own
/// connection (embedded SurrealDB allows only one handle per store directory, so
/// the two must not overlap). Idempotent: Chronicle keys chunks by content hash,
/// so re-ingesting an unchanged corpus on every boot is a no-op. Fail-open: any
/// error is logged and skipped, never blocking startup.
pub(crate) async fn ingest_corpus(tenant: &TenantCtx, chunks: Vec<AwChunk>) {
    if chunks.is_empty() {
        return;
    }
    let Some(config) = build_config() else {
        tracing::debug!("knowledge: embedding env unset; skipping baked-corpus ingest");
        return;
    };
    // surreal-memory's ingest connection is a SEPARATE in-memory DB from the
    // serving mount's, so ingested chunks would be invisible at query time. Skip
    // with a clear note — durable RAG needs a persistent backend.
    if std::env::var(ENV_BACKEND).as_deref() == Ok("surreal-memory") {
        tracing::warn!(
            "knowledge: backend=surreal-memory does not retain a baked corpus across the \
             ingest/serve connection boundary; skipping corpus ingest (use surreal-embedded)"
        );
        return;
    }
    let Some(driver) = build_driver(config.embedding_dim).await else {
        return;
    };
    let knowledge = match KnowledgeChronicle::from_config(&config, driver).await {
        Ok(knowledge) => knowledge,
        Err(err) => {
            tracing::warn!(error = %err, "knowledge: baked-corpus ingest connect failed; skipping");
            return;
        }
    };
    let count = chunks.len();
    let dw_chunks: Vec<DwChunk> = chunks.into_iter().map(to_dw_chunk).collect();
    match knowledge.ingest(tenant, dw_chunks).await {
        Ok(outcome) => tracing::info!(
            chunks = count,
            stored = outcome.chunk_ids.len(),
            "knowledge: baked corpus ingested"
        ),
        Err(err) => tracing::warn!(error = %err, "knowledge: baked-corpus ingest failed"),
    }
    // `knowledge` (and its embedded-store handle) drops here, before the serving
    // mount opens its own connection.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `std::env::set_var`/`remove_var` are `unsafe` under edition 2024; confined to
    // tests, which run under `#[serial]` so no other thread observes the env mid-mutation.
    #[allow(unsafe_code)]
    fn set(key: &str, val: &str) {
        // SAFETY: env-touching tests are serialized via `#[serial]`.
        unsafe { std::env::set_var(key, val) };
    }

    #[allow(unsafe_code)]
    fn unset(key: &str) {
        // SAFETY: env-touching tests are serialized via `#[serial]`.
        unsafe { std::env::remove_var(key) };
    }

    fn clear_embed_env() {
        for key in [
            ENV_EMBED_BASE_URL,
            ENV_EMBED_API_KEY,
            ENV_EMBED_MODEL,
            ENV_EMBED_DIM,
        ] {
            unset(key);
        }
    }

    fn set_required_embed_env() {
        set(ENV_EMBED_BASE_URL, "https://api.example/v1");
        set(ENV_EMBED_API_KEY, "sk-test");
        set(ENV_EMBED_MODEL, "text-embedding-3-small");
    }

    #[test]
    #[serial]
    fn build_config_reads_embedding_env() {
        clear_embed_env();
        set_required_embed_env();
        set(ENV_EMBED_DIM, "1536");
        let config = build_config().expect("complete embedding env yields a config");
        assert_eq!(
            config.openai_base_url.as_deref(),
            Some("https://api.example/v1")
        );
        assert_eq!(config.openai_api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            config.embedding_model.as_deref(),
            Some("text-embedding-3-small")
        );
        assert_eq!(config.embedding_dim, 1536);
        clear_embed_env();
    }

    #[test]
    #[serial]
    fn build_config_defaults_dim_when_unset_or_invalid() {
        clear_embed_env();
        set_required_embed_env();
        // Unset dim -> default.
        assert_eq!(
            build_config().expect("config").embedding_dim,
            DEFAULT_EMBEDDING_DIM
        );
        // Non-numeric dim -> default (never panics).
        set(ENV_EMBED_DIM, "not-a-number");
        assert_eq!(
            build_config().expect("config").embedding_dim,
            DEFAULT_EMBEDDING_DIM
        );
        clear_embed_env();
    }

    #[test]
    #[serial]
    fn build_config_disabled_when_embedding_env_incomplete() {
        clear_embed_env();
        // No embedding env at all -> opt-out.
        assert!(build_config().is_none());
        // Base URL + key but no model -> still incomplete.
        set(ENV_EMBED_BASE_URL, "https://api.example/v1");
        set(ENV_EMBED_API_KEY, "sk-test");
        assert!(build_config().is_none());
        clear_embed_env();
    }
}
