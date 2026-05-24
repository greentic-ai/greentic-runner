//! Test doubles. Compiled only when `--features test-mock`. The runner-
//! host and designer integration tests use these to avoid hitting Redis
//! or real LLM providers in CI.

#[allow(clippy::expect_used)]
mod inner {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::config::AgentConfig;
    use crate::config_provider::ConfigProvider;
    use crate::error::{ConfigError, LlmError, StateError, TerminationReason};
    use crate::llm::{LlmBackend, LlmRequest, LlmResponse};
    use crate::state::{AgentStateStore, ConversationState, SessionLock, SessionLockInner};
    use crate::telemetry::{StepTelemetryCtx, Telemetry};
    use crate::tenant::TenantContext;

    /// LLM mock with a scripted response queue.
    pub struct MockLlmBackend {
        pub responses: Mutex<Vec<Result<LlmResponse, LlmError>>>,
    }

    impl MockLlmBackend {
        pub fn new(responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl LlmBackend for MockLlmBackend {
        fn complete<'a>(
            &'a self,
            _req: LlmRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
            // Eagerly extract the next scripted response so the future is Send + 'a.
            let next = {
                let mut queue = self.responses.lock().expect("mock LLM mutex poisoned");
                if queue.is_empty() {
                    Err(LlmError::Transport("mock queue exhausted".into()))
                } else {
                    queue.remove(0)
                }
            };
            Box::pin(async move { next })
        }
    }

    /// In-memory state store; lock is a no-op semaphore.
    pub struct MockAgentStateStore {
        entries: Mutex<HashMap<String, ConversationState>>,
    }

    impl MockAgentStateStore {
        pub fn new() -> Self {
            Self {
                entries: Mutex::new(HashMap::new()),
            }
        }

        fn build_key(tenant: &TenantContext, session_id: &str) -> String {
            format!("{}:{}", tenant.key_prefix(), session_id)
        }
    }

    impl Default for MockAgentStateStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl AgentStateStore for MockAgentStateStore {
        fn load<'a>(
            &'a self,
            tenant: &'a TenantContext,
            session_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<ConversationState, StateError>> + Send + 'a>>
        {
            let key = Self::build_key(tenant, session_id);
            let state = self
                .entries
                .lock()
                .expect("mock state mutex poisoned")
                .get(&key)
                .cloned()
                .unwrap_or_else(|| ConversationState::empty(tenant, session_id));
            Box::pin(async move { Ok(state) })
        }

        fn save<'a>(
            &'a self,
            tenant: &'a TenantContext,
            session_id: &'a str,
            state: &'a ConversationState,
        ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
            let key = Self::build_key(tenant, session_id);
            let cloned = state.clone();
            Box::pin(async move {
                self.entries
                    .lock()
                    .expect("mock state mutex poisoned")
                    .insert(key, cloned);
                Ok(())
            })
        }

        fn acquire_lock<'a>(
            &'a self,
            _tenant: &'a TenantContext,
            _session_id: &'a str,
            _wait: Duration,
        ) -> Pin<Box<dyn Future<Output = Result<SessionLock, StateError>> + Send + 'a>> {
            Box::pin(async move { Ok(SessionLock::new(Box::new(NoopLockInner))) })
        }
    }

    struct NoopLockInner;

    impl SessionLockInner for NoopLockInner {
        fn refresh<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn release(&self) {}
    }

    pub struct MockTelemetry {
        pub recorded: Mutex<Vec<StepTelemetryCtx>>,
    }

    impl MockTelemetry {
        pub fn new() -> Self {
            Self {
                recorded: Mutex::new(Vec::new()),
            }
        }
    }

    impl Default for MockTelemetry {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Telemetry for MockTelemetry {
        fn record_step(&self, ctx: &StepTelemetryCtx) {
            self.recorded
                .lock()
                .expect("mock telemetry mutex poisoned")
                .push(ctx.clone());
        }
    }

    pub struct MockConfigProvider {
        pub configs: Mutex<HashMap<String, AgentConfig>>,
    }

    impl MockConfigProvider {
        pub fn new() -> Self {
            Self {
                configs: Mutex::new(HashMap::new()),
            }
        }

        pub fn insert(&self, tenant: &TenantContext, agent_id: &str, cfg: AgentConfig) {
            self.configs
                .lock()
                .expect("mock config mutex poisoned")
                .insert(format!("{}:{agent_id}", tenant.key_prefix()), cfg);
        }
    }

    impl Default for MockConfigProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ConfigProvider for MockConfigProvider {
        fn agent_config<'a>(
            &'a self,
            tenant: &'a TenantContext,
            agent_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<AgentConfig, ConfigError>> + Send + 'a>> {
            let key = format!("{}:{agent_id}", tenant.key_prefix());
            let agent_id_owned = agent_id.to_string();
            let entry = self
                .configs
                .lock()
                .expect("mock config mutex poisoned")
                .get(&key)
                .cloned();
            Box::pin(async move { entry.ok_or(ConfigError::AgentNotFound(agent_id_owned)) })
        }
    }

    /// Convenience: assert a [`TerminationReason`] matches the expected value.
    pub fn assert_terminated_by(actual: &TerminationReason, expected: &TerminationReason) {
        assert_eq!(actual, expected, "expected {expected:?}, got {actual:?}");
    }
}

pub use inner::{
    MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry, assert_terminated_by,
};
