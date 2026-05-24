//! Multi-tenant context. Mandatory on every public AgentRuntime method
//! so cross-tenant access is a compile error, not a runtime check.

use serde::{Deserialize, Serialize};

/// Identifies the (tenant, environment) pair an agent step runs under.
/// Pass-by-value (cheap clone — two `String`s).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantContext {
    pub tenant_id: String,
    pub env_id: String,
}

impl TenantContext {
    pub fn new(tenant_id: impl Into<String>, env_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            env_id: env_id.into(),
        }
    }

    /// Prefix used by Redis key builders. Returns `aw:{tenant}:{env}`.
    pub fn key_prefix(&self) -> String {
        format!("aw:{}:{}", self.tenant_id, self.env_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefix_formats_as_expected() {
        let ctx = TenantContext::new("acme", "prod");
        assert_eq!(ctx.key_prefix(), "aw:acme:prod");
    }

    #[test]
    fn tenant_context_is_eq_and_hashable() {
        let a = TenantContext::new("acme", "prod");
        let b = TenantContext::new("acme", "prod");
        assert_eq!(a, b);
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
