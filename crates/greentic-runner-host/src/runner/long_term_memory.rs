//! Operator-configured long-term (episodic) memory wiring for the agentic
//! worker. Compiled only with the `long-term-chronicle` feature.
//!
//! A single native Chronicle backend (graphiti over Neo4j) is built once from
//! operator environment variables and shared across every agent on the runtime;
//! per-agent/tenant isolation is handled inside the runtime by scoping each call
//! to the agent's tenant (Chronicle `group_id`). When the required Neo4j
//! connection environment is unset — or the connection fails — the long-term
//! tier simply stays disabled; a missing backend never fails construction.

use std::sync::Arc;

use greentic_aw_runtime::{AgentRuntime, LongTermMemory};
use greentic_dw_memory_chronicle::{ChronicleLongTermMemory, ChronicleMemoryConfig};

const ENV_NEO4J_URI: &str = "GREENTIC_CHRONICLE_NEO4J_URI";
const ENV_NEO4J_USER: &str = "GREENTIC_CHRONICLE_NEO4J_USER";
const ENV_NEO4J_PASSWORD: &str = "GREENTIC_CHRONICLE_NEO4J_PASSWORD";
const ENV_NEO4J_DATABASE: &str = "GREENTIC_CHRONICLE_NEO4J_DATABASE";

/// Attach an operator-configured Chronicle long-term backend to `runtime` when
/// the Neo4j connection environment is present and the connection succeeds.
/// Returns the runtime unchanged otherwise — a missing or unreachable backend
/// degrades to "no long-term memory", never a hard failure at construction.
pub async fn attach(runtime: AgentRuntime) -> AgentRuntime {
    let (uri, user, password) = match (
        std::env::var(ENV_NEO4J_URI),
        std::env::var(ENV_NEO4J_USER),
        std::env::var(ENV_NEO4J_PASSWORD),
    ) {
        (Ok(uri), Ok(user), Ok(password)) => (uri, user, password),
        _ => {
            tracing::debug!(
                "long-term memory: {ENV_NEO4J_URI}/USER/PASSWORD unset; long-term disabled"
            );
            return runtime;
        }
    };

    let mut config = ChronicleMemoryConfig::new(uri, user, password);
    if let Ok(database) = std::env::var(ENV_NEO4J_DATABASE) {
        config.neo4j_database = database;
    }
    // `openai_api_key` is left `None`: the Chronicle client reads `OPENAI_API_KEY`
    // from the environment itself, so the operator configures it the same way.

    match ChronicleLongTermMemory::connect(config).await {
        Ok(memory) => {
            tracing::info!("long-term memory: Chronicle (Neo4j) backend attached");
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
