// placeholder — filled in subsequent tasks

use crate::error::ConfigError;
use crate::config::AgentConfig;
use crate::tenant::TenantContext;

/// Loads [`AgentConfig`] for a given tenant + agent pair (Task 1.6).
///
/// The blanket `dyn` compatibility rule requires that async methods use
/// `Pin<Box<dyn Future>>` return types instead of `async fn` in the trait
/// definition when stored behind `Arc<dyn ConfigProvider>`.
pub trait ConfigProvider: Send + Sync {
    fn load(
        &self,
        tenant: &TenantContext,
        agent_id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AgentConfig, ConfigError>> + Send + '_>>;
}
