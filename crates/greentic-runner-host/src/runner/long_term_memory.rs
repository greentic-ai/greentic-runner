//! Operator-configured long-term (episodic) memory wiring for the agentic
//! worker. Compiled only with the `long-term-chronicle` feature.
//!
//! A single native Chronicle backend (graphiti over Neo4j) is built once from
//! operator environment variables and shared across every agent on the runtime;
//! per-agent/tenant data isolation is handled inside the runtime by scoping each
//! call to the agent's tenant (Chronicle `group_id`).
//!
//! Chronicle is driven **entirely through the provider-neutral DW LLM and
//! embedding families** — both wired to whatever OpenAI-compatible endpoint the
//! operator points at (`GREENTIC_CHRONICLE_{LLM,EMBED}_BASE_URL`). There is no
//! hard dependency on a single provider. When the required environment is unset
//! — or a provider config / Neo4j connection fails — the long-term tier simply
//! stays disabled; a missing backend never fails construction.

use std::sync::Arc;

use greentic_aw_runtime::{AgentRuntime, LongTermMemory};
use greentic_dw_embedding::EmbeddingProvider;
use greentic_dw_embedding_openai_compatible::{
    OpenAiCompatibleEmbeddingConfig, OpenAiCompatibleEmbeddingProvider,
};
use greentic_dw_llm::LlmProvider;
use greentic_dw_llm_openai_compatible::{OpenAiCompatibleConfig, OpenAiCompatibleProvider};
use greentic_dw_memory_chronicle::{ChronicleLongTermMemory, ChronicleMemoryConfig};
use greentic_types::{EnvId, TenantCtx, TenantId};

// Neo4j connection.
const ENV_NEO4J_URI: &str = "GREENTIC_CHRONICLE_NEO4J_URI";
const ENV_NEO4J_USER: &str = "GREENTIC_CHRONICLE_NEO4J_USER";
const ENV_NEO4J_PASSWORD: &str = "GREENTIC_CHRONICLE_NEO4J_PASSWORD";
const ENV_NEO4J_DATABASE: &str = "GREENTIC_CHRONICLE_NEO4J_DATABASE";
// LLM (entity/edge extraction) — any OpenAI-compatible endpoint.
const ENV_LLM_BASE_URL: &str = "GREENTIC_CHRONICLE_LLM_BASE_URL";
const ENV_LLM_API_KEY: &str = "GREENTIC_CHRONICLE_LLM_API_KEY";
const ENV_LLM_MODEL: &str = "GREENTIC_CHRONICLE_LLM_MODEL";
// Embeddings (vector recall) — any OpenAI-compatible endpoint.
const ENV_EMBED_BASE_URL: &str = "GREENTIC_CHRONICLE_EMBED_BASE_URL";
const ENV_EMBED_API_KEY: &str = "GREENTIC_CHRONICLE_EMBED_API_KEY";
const ENV_EMBED_MODEL: &str = "GREENTIC_CHRONICLE_EMBED_MODEL";
const ENV_EMBED_DIM: &str = "GREENTIC_CHRONICLE_EMBED_DIM";

const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_EMBEDDING_DIM: usize = 1024;

/// Attach an operator-configured, provider-neutral Chronicle long-term backend
/// to `runtime` when Neo4j + the LLM and embedding endpoints are all present and
/// the connection succeeds. Returns the runtime unchanged otherwise.
pub async fn attach(runtime: AgentRuntime) -> AgentRuntime {
    let Some(mut config) = neo4j_config() else {
        tracing::debug!("long-term memory: Neo4j env unset; long-term disabled");
        return runtime;
    };
    let Some(llm) = build_llm() else {
        tracing::debug!("long-term memory: LLM endpoint env unset/invalid; long-term disabled");
        return runtime;
    };
    let Some((embedder, embedding_dim)) = build_embedder() else {
        tracing::debug!(
            "long-term memory: embedding endpoint env unset/invalid; long-term disabled"
        );
        return runtime;
    };
    // Chronicle's graph vector index dimension must match the embedder's output.
    config.embedding_dim = Some(embedding_dim);

    match ChronicleLongTermMemory::connect_with_dw_providers(
        config,
        llm,
        embedder,
        operator_tenant(),
    )
    .await
    {
        Ok(memory) => {
            tracing::info!(
                "long-term memory: Chronicle attached (provider-neutral DW LLM + embeddings)"
            );
            runtime.with_long_term_memory(Arc::new(memory) as Arc<dyn LongTermMemory>)
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "long-term memory: Chronicle connect failed; long-term disabled"
            );
            runtime
        }
    }
}

/// Read the Neo4j connection from the environment. Returns `None` when any of
/// the three required vars is absent.
fn neo4j_config() -> Option<ChronicleMemoryConfig> {
    let uri = std::env::var(ENV_NEO4J_URI).ok()?;
    let user = std::env::var(ENV_NEO4J_USER).ok()?;
    let password = std::env::var(ENV_NEO4J_PASSWORD).ok()?;
    let mut config = ChronicleMemoryConfig::new(uri, user, password);
    if let Ok(database) = std::env::var(ENV_NEO4J_DATABASE) {
        config.neo4j_database = database;
    }
    Some(config)
}

/// Build the provider-neutral DW LLM backend (any OpenAI-compatible endpoint).
/// Returns `None` when the endpoint env is incomplete or the config is invalid.
fn build_llm() -> Option<Arc<dyn LlmProvider>> {
    let base_url = std::env::var(ENV_LLM_BASE_URL).ok()?;
    let api_key = std::env::var(ENV_LLM_API_KEY).ok()?;
    let model = std::env::var(ENV_LLM_MODEL).ok()?;
    let mut cfg = OpenAiCompatibleConfig::new(base_url, model, DEFAULT_TIMEOUT_MS);
    cfg.api_key_secret = Some(api_key);
    match OpenAiCompatibleProvider::new(cfg) {
        Ok(provider) => Some(Arc::new(provider)),
        Err(err) => {
            tracing::warn!(error = %err, "long-term memory: LLM provider config invalid");
            None
        }
    }
}

/// Build the provider-neutral DW embedding backend (any OpenAI-compatible
/// endpoint), returning it alongside the embedding dimension Chronicle indexes
/// at. Returns `None` when the endpoint env is incomplete or invalid.
fn build_embedder() -> Option<(Arc<dyn EmbeddingProvider>, usize)> {
    let base_url = std::env::var(ENV_EMBED_BASE_URL).ok()?;
    let api_key = std::env::var(ENV_EMBED_API_KEY).ok()?;
    let model = std::env::var(ENV_EMBED_MODEL).ok()?;
    let embedding_dim = std::env::var(ENV_EMBED_DIM)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_EMBEDDING_DIM);
    let mut cfg =
        OpenAiCompatibleEmbeddingConfig::new(api_key, base_url, model, DEFAULT_TIMEOUT_MS);
    cfg.embedding_dim = embedding_dim;
    match OpenAiCompatibleEmbeddingProvider::new(cfg) {
        Ok(provider) => Some((Arc::new(provider), embedding_dim)),
        Err(err) => {
            tracing::warn!(error = %err, "long-term memory: embedding provider config invalid");
            None
        }
    }
}

/// Operator/system tenant the Chronicle LLM + embedding calls run as. The
/// OpenAI-compatible providers ignore the tenant (the bridge merely carries it),
/// and end-tenant data isolation is via the per-call tenant -> Chronicle
/// `group_id`, so a fixed operator identity is correct here.
fn operator_tenant() -> TenantCtx {
    let env = EnvId::try_from("operator").unwrap_or_else(|_| {
        EnvId::try_from("dev").expect("the literal env id \"dev\" is always valid")
    });
    let tenant = TenantId::try_from("chronicle")
        .expect("the literal tenant id \"chronicle\" is always valid");
    TenantCtx::new(env, tenant)
}
