// placeholder — filled in subsequent tasks

use crate::error::StateError;
use crate::tenant::TenantContext;

/// A single message in the conversation history (Task 1.5).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage;

/// Full conversation state persisted per session (Task 1.5).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConversationState;

/// RAII guard that holds a distributed session lock (Task 1.5).
pub struct SessionLock;

/// Persistent store for conversation state with optimistic locking (Task 1.5).
///
/// Uses `Pin<Box<dyn Future>>` returns for dyn-safety behind `Arc<dyn AgentStateStore>`.
pub trait AgentStateStore: Send + Sync {
    fn load(
        &self,
        tenant: &TenantContext,
        session_id: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<ConversationState>, StateError>>
                + Send
                + '_,
        >,
    >;

    fn save(
        &self,
        tenant: &TenantContext,
        session_id: &str,
        state: &ConversationState,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StateError>> + Send + '_>>;

    fn lock(
        &self,
        tenant: &TenantContext,
        session_id: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SessionLock, StateError>> + Send + '_>,
    >;
}
