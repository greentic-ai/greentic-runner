//! Bridges an `operala.call` flow node into the in-process deep-worker
//! runtime (`greentic-dw-operala-invoker`), mirroring `agent_node`'s
//! [`AgentNodeHandler`](crate::runner::agent_node::AgentNodeHandler) seam for
//! `dw.agent`.
//!
//! The trait itself is unconditional (like `AgentNodeHandler`) so the engine
//! can hold `Option<Arc<dyn OperalaNodeHandler>>` regardless of build
//! features; the concrete [`RuntimeOperalaNodeHandler`] impl (wrapping
//! `DeepWorkerInvoker`) is feature-gated behind `desktop-agent-ephemeral` —
//! the same feature the designer's offline Test-chat sidecar already builds
//! with — so `operala.call` nodes run with NO NATS in that build. Server
//! builds without that feature keep the existing NATS
//! `RemoteDispatchHandler` fallback (`execute_remote_dispatch`) untouched.

use anyhow::Result;
use serde_json::Value;

/// Bridges an `operala.call` flow node into an in-process deep-worker
/// runtime. The engine holds this as a trait object so `engine.rs` stays
/// free of deep-worker construction details, exactly like
/// [`AgentNodeHandler`](crate::runner::agent_node::AgentNodeHandler).
#[async_trait::async_trait]
pub trait OperalaNodeHandler: Send + Sync {
    /// Execute one deep-worker dispatch. `target` is the node's routing
    /// target; `operation` and `input` come from the node's rendered payload
    /// (the same `{await, operation, input}` contract
    /// [`FlowEngine::execute_remote_dispatch`](super::engine) parses for the
    /// NATS path — `operation` must be `""` or `"run"`). Returns the node
    /// output JSON.
    async fn execute(
        &self,
        tenant: &str,
        env: &str,
        target: &str,
        operation: &str,
        session_id: &str,
        input: &Value,
    ) -> Result<Value>;
}

// ---------------------------------------------------------------------------
// desktop-agent-ephemeral feature: DeepWorkerInvoker-backed handler
// ---------------------------------------------------------------------------

#[cfg(feature = "desktop-agent-ephemeral")]
mod dw {
    use anyhow::{Context, Result};
    use async_trait::async_trait;
    use greentic_dw_operala_bridge::OperalaDispatchInvoker;
    use greentic_dw_operala_invoker::DeepWorkerInvoker;
    use serde_json::Value;

    use super::OperalaNodeHandler;

    /// Production [`OperalaNodeHandler`] wrapping [`DeepWorkerInvoker`]: runs
    /// the deep-worker `DeepLoopCoordinator` in-process (the invoker itself
    /// runs it on `tokio::task::spawn_blocking`) — no NATS transport, no
    /// `greentic-dw-operala-bridge` wire hop involved.
    pub struct RuntimeOperalaNodeHandler {
        invoker: DeepWorkerInvoker,
    }

    impl RuntimeOperalaNodeHandler {
        pub fn new(invoker: DeepWorkerInvoker) -> Self {
            Self { invoker }
        }
    }

    #[async_trait]
    impl OperalaNodeHandler for RuntimeOperalaNodeHandler {
        async fn execute(
            &self,
            tenant: &str,
            env: &str,
            target: &str,
            operation: &str,
            session_id: &str,
            input: &Value,
        ) -> Result<Value> {
            let idempotency_key = (!session_id.trim().is_empty()).then_some(session_id);
            let outcome = self
                .invoker
                .invoke(
                    tenant,
                    env,
                    target,
                    operation,
                    input.clone(),
                    idempotency_key,
                )
                .await
                .with_context(|| format!("in-process operala dispatch to '{target}' failed"))?;
            // Mirror the NATS `operala.call` response shape closely enough for
            // flow templates to read `{{node.reply}}`/`{{node.output}}`
            // regardless of dispatch mode: `reply` is the deep-worker's
            // `output.reply` when present, else the raw `output`.
            let reply = outcome
                .output
                .get("reply")
                .cloned()
                .unwrap_or_else(|| outcome.output.clone());
            Ok(serde_json::json!({
                "ok": outcome.ok,
                "reply": reply,
                "output": outcome.output,
            }))
        }
    }
}

#[cfg(feature = "desktop-agent-ephemeral")]
pub use dw::RuntimeOperalaNodeHandler;
