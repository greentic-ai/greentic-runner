//! Greentic Agentic Worker Runtime — library crate.
//!
//! See `docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md`
//! for the full design spec. This crate exposes the [`AgentRuntime`] entry
//! point and the trait surface (`AgentStateStore`, `ConfigProvider`,
//! `LlmBackend`, `Telemetry`) that the production runner-host and the
//! designer playground both consume.
//!
//! The [`graph`] module provides durable multi-agent graph execution
//! (`GraphExecutor`, `GraphConfig`, `CheckpointStore`); see
//! `docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md`.

#![deny(unsafe_code)]
#![warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod config;
pub mod config_provider;
pub mod cost;
pub mod error;
pub mod graph;
pub mod guardrail;
pub mod http_provider;
pub mod layered_provider;
pub mod llm;
pub mod llm_extension;
pub mod llm_openai;
pub mod r#loop;
pub mod manifest_provider;
pub mod manifest_tools;
pub mod mcp_source;
pub mod state;
pub mod state_redis;
pub mod telemetry;
pub mod tenant;
pub mod tools;

#[cfg(feature = "test-mock")]
pub mod mock;

pub use config::{AgentConfig, AgentLimits, LlmProviderRef, ToolRef};
pub use config_provider::{CachingConfigProvider, ConfigProvider, InMemoryConfigProvider};
#[cfg(feature = "test-mock")]
pub use cost::MockTokenMeter;
pub use cost::{RedisTokenMeter, TokenMeter};
pub use error::{AgentError, ConfigError, LlmError, StateError, TerminationReason};
pub use graph::http_provider::{CachingGraphProvider, HttpGraphProvider};
pub use http_provider::HttpConfigProvider;
pub use layered_provider::LayeredConfigProvider;
pub use llm::{LlmBackend, LlmRequest, LlmResponse, RetryingLlmBackend};
pub use llm_extension::{
    BridgeCredential, ExtensionLlmBackend, LlmExtensionInvoker, RuntimeInvoker,
};
pub use llm_openai::OpenAiLlmBackend;
pub use manifest_provider::ManifestToolOverlayProvider;
pub use mcp_source::{McpRoute, McpToolCatalog, McpToolEntry, McpToolSource, dispatch_route};
pub use state::{AgentStateStore, ChatMessage, ConversationState, SessionLock};
pub use state_redis::RedisAgentStateStore;
pub use telemetry::{OtelTelemetry, StepTelemetryCtx, Telemetry};
pub use tenant::TenantContext;
pub use tools::{RedisToolLedger, ToolLedger};

use std::sync::Arc;

/// Observer for incremental step progress: token deltas as the LLM streams
/// its reply, and tool-call activity as the loop dispatches tools.
///
/// All methods have no-op default bodies, so callers implement only the
/// hooks they consume and the non-streaming [`AgentRuntime::step`] path
/// (which uses [`NoopStepObserver`]) costs nothing.
///
/// **Extending this trait:** add new capabilities as NEW defaulted methods
/// (e.g. `fn on_iteration_started(&self, _iter: u32) {}`) rather than
/// changing an existing signature. The current `on_token_delta(&self,
/// chunk: &str)` is deliberately minimal; per-iteration context must arrive
/// through an additional hook so existing implementors keep compiling.
pub trait StepObserver: Send + Sync {
    /// Whether this observer wants token-level streaming. Defaults to
    /// `false` so the non-streaming [`AgentRuntime::step`] path calls
    /// [`LlmBackend::complete`] and preserves the exact request wire shape
    /// (no `stream: true`) every existing caller relied on before streaming
    /// existed. A streaming consumer overrides this to `true`, which makes
    /// [`r#loop::run_step`] use [`LlmBackend::complete_streaming`] and drive
    /// [`StepObserver::on_token_delta`].
    fn wants_streaming(&self) -> bool {
        false
    }
    /// Called with each incremental text chunk of the assistant reply.
    /// Only invoked when [`StepObserver::wants_streaming`] returns `true`.
    fn on_token_delta(&self, _chunk: &str) {}
    /// Called just before a tool is dispatched.
    fn on_tool_call(&self, _name: &str, _call_id: &str) {}
    /// Called after a tool dispatch succeeds, with the tool's result.
    fn on_tool_result(&self, _name: &str, _call_id: &str, _result: &serde_json::Value) {}
}

/// No-op observer used by the non-streaming [`AgentRuntime::step`].
pub struct NoopStepObserver;
impl StepObserver for NoopStepObserver {}

/// The main entry point for executing a single agentic step.
///
/// Construct via [`AgentRuntime::new`] with the trait objects (config,
/// state, LLM, telemetry, token_meter, ledger) plus a shared
/// `Arc<ExtensionRuntime>` for tool dispatch. Call [`AgentRuntime::step`]
/// per inbound user message.
pub struct AgentRuntime {
    pub(crate) config_provider: Arc<dyn ConfigProvider>,
    pub(crate) state_store: Arc<dyn AgentStateStore>,
    pub(crate) ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
    pub(crate) llm: Arc<dyn LlmBackend>,
    pub(crate) telemetry: Arc<dyn Telemetry>,
    pub(crate) token_meter: Arc<dyn TokenMeter>,
    pub(crate) ledger: Arc<dyn ToolLedger>,
    /// Per-tenant agentic-worker MCP tool source. `None` disables MCP tools
    /// entirely (`mcp:`-prefixed tool refs then resolve to nothing). The real
    /// per-operator wiring lives in the runner host; tests and non-MCP callers
    /// pass `None`.
    pub(crate) mcp: Option<Arc<crate::mcp_source::McpToolSource>>,
}

impl AgentRuntime {
    // Each argument is a distinct injected dependency (config, state, ext,
    // llm, telemetry, token-meter, ledger, mcp); a builder would add ceremony
    // without removing the coupling, so the wide constructor is intentional.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_provider: Arc<dyn ConfigProvider>,
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
        mcp: Option<Arc<crate::mcp_source::McpToolSource>>,
    ) -> Self {
        Self {
            config_provider,
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
            mcp,
        }
    }

    /// Execute one agentic step against the given session.
    /// Implementation lives in [`r#loop::run_step`].
    pub async fn step(
        &self,
        tenant: TenantContext,
        session_id: &str,
        agent_id: &str,
        message: AgentInput,
    ) -> Result<AgentOutput, AgentError> {
        self.step_with_observer(
            tenant,
            session_id,
            agent_id,
            message,
            Arc::new(NoopStepObserver),
        )
        .await
    }

    /// Execute one agentic step while reporting incremental progress to
    /// `observer` (streamed token deltas + tool-call activity).
    /// [`AgentRuntime::step`] delegates here with a [`NoopStepObserver`].
    pub async fn step_with_observer(
        &self,
        tenant: TenantContext,
        session_id: &str,
        agent_id: &str,
        message: AgentInput,
        observer: Arc<dyn StepObserver>,
    ) -> Result<AgentOutput, AgentError> {
        r#loop::run_step(self, tenant, session_id, agent_id, message, observer).await
    }
}

/// Inbound user message handed to [`AgentRuntime::step`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentInput {
    pub text: String,
}

/// Outbound reply produced by [`AgentRuntime::step`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentOutput {
    pub reply: String,
    pub trail: Vec<AgentStep>,
    pub terminated_by: TerminationReason,
}

/// One iteration of the Plan-Act-Observe loop, surfaced in the audit
/// trail (`AgentOutput.trail`). Caller decides whether to persist or
/// display.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStep {
    ToolCall {
        name: String,
        call_id: String,
        result: serde_json::Value,
    },
    ToolCallReused {
        name: String,
        call_id: String,
    },
    ToolCallBlocked {
        name: String,
        reason: String,
    },
    Reply {
        text: String,
    },
}
