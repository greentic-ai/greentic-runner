//! Redis-backed `AgentStateStore` impl. Filled in Phase 2 — Phase 1
//! ships only the type alias so `pub use` in `lib.rs` compiles.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::StateError;
use crate::state::{AgentStateStore, ConversationState, SessionLock};
use crate::tenant::TenantContext;

/// Production state store backed by the workspace `greentic-state`
/// Redis client. Phase 2 implements `load`, `save`, `acquire_lock`.
pub struct RedisAgentStateStore {
    // Phase 2 holds an Arc<greentic_state::RedisPool> here.
    _placeholder: (),
}

impl RedisAgentStateStore {
    /// Phase 2 replaces this with `new(pool: Arc<RedisPool>) -> Self`.
    pub fn placeholder() -> Self {
        Self { _placeholder: () }
    }
}

impl AgentStateStore for RedisAgentStateStore {
    fn load<'a>(
        &'a self,
        _tenant: &'a TenantContext,
        _session_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ConversationState, StateError>> + Send + 'a>> {
        Box::pin(async {
            Err(StateError::Redis(
                "RedisAgentStateStore not yet implemented (Phase 2)".into(),
            ))
        })
    }

    fn save<'a>(
        &'a self,
        _t: &'a TenantContext,
        _s: &'a str,
        _state: &'a ConversationState,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
        Box::pin(async {
            Err(StateError::Redis(
                "RedisAgentStateStore not yet implemented (Phase 2)".into(),
            ))
        })
    }

    fn acquire_lock<'a>(
        &'a self,
        _t: &'a TenantContext,
        _s: &'a str,
        _wait: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<SessionLock, StateError>> + Send + 'a>> {
        Box::pin(async {
            Err(StateError::Redis(
                "RedisAgentStateStore not yet implemented (Phase 2)".into(),
            ))
        })
    }
}
