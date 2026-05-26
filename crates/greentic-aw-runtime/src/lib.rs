//! Greentic Agentic Worker Runtime — library crate.
//!
//! See `docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md`
//! for the full design spec. This crate exposes the [`AgentRuntime`] entry
//! point and the trait surface (`AgentStateStore`, `ConfigProvider`,
//! `LlmBackend`, `Telemetry`) that the production runner-host and the
//! designer playground both consume.

#![deny(unsafe_code)]
#![warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod config;
pub mod config_provider;
pub mod cost;
pub mod error;
pub mod llm;
pub mod llm_openai;
pub mod r#loop;
pub mod manifest_tools;
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
pub use llm::{LlmBackend, LlmRequest, LlmResponse, RetryingLlmBackend};
pub use llm_openai::OpenAiLlmBackend;
pub use state::{AgentStateStore, ChatMessage, ConversationState, SessionLock};
pub use state_redis::RedisAgentStateStore;
pub use telemetry::{OtelTelemetry, StepTelemetryCtx, Telemetry};
pub use tenant::TenantContext;
pub use tools::{RedisToolLedger, ToolLedger};

use std::sync::Arc;

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
}

impl AgentRuntime {
    pub fn new(
        config_provider: Arc<dyn ConfigProvider>,
        state_store: Arc<dyn AgentStateStore>,
        ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
        llm: Arc<dyn LlmBackend>,
        telemetry: Arc<dyn Telemetry>,
        token_meter: Arc<dyn TokenMeter>,
        ledger: Arc<dyn ToolLedger>,
    ) -> Self {
        Self {
            config_provider,
            state_store,
            ext_runtime,
            llm,
            telemetry,
            token_meter,
            ledger,
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
        r#loop::run_step(self, tenant, session_id, agent_id, message).await
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
