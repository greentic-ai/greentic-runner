//! Multi-tenant context. Mandatory on every public AgentRuntime method
//! so cross-tenant access is a compile error, not a runtime check.

use serde::{Deserialize, Serialize};

/// Identifies the (tenant, environment) pair an agent step runs under.
/// Pass-by-value (cheap clone — two `String`s and one optional).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantContext {
    pub tenant_id: String,
    pub env_id: String,
    /// Optional session user for per-user LLM resolution (host-LLM seam).
    /// `None` for autonomous workers; the tenant-level provider is used then.
    #[serde(default)]
    pub user_email: Option<String>,
    /// Identity of the deployed unit this step's spend belongs to — emitted as
    /// the `project_id` billing dimension.
    ///
    /// The host fills this from the revision's `bundle_id`, which is exactly
    /// what greentic-designer records as `pack_name` in its `published_workers`
    /// join table; matching the two is what lets a product's authoring spend
    /// and runtime spend join.
    ///
    /// Deliberately NOT the agent id: an agent id is the key inside
    /// `manifest.agents` (`"greeter"`, `"assistant"`), which is not unique
    /// across packs — two packs each shipping an `assistant` would collapse
    /// into one bogus project row with summed credits.
    ///
    /// `None` when the identity is genuinely unknown (the tenant-only legacy
    /// pack path, the process-level NATS serve path, graph/tool call sites with
    /// no pack context). Consumers MUST omit the dimension rather than
    /// substitute a fallback or placeholder — cloud-commerce already groups a
    /// missing key under `"unknown"`.
    #[serde(default)]
    pub project_id: Option<String>,
}

impl TenantContext {
    pub fn new(tenant_id: impl Into<String>, env_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            env_id: env_id.into(),
            user_email: None,
            project_id: None,
        }
    }

    /// Set an optional user email for per-user LLM resolution.
    #[must_use]
    pub fn with_user_email(mut self, email: Option<String>) -> Self {
        self.user_email = email;
        self
    }

    /// Set the deployed unit's identity (the revision `bundle_id`) that billing
    /// reports as `project_id`. Pass `None` when it is genuinely unknown — see
    /// [`TenantContext::project_id`] for why there is no fallback.
    #[must_use]
    pub fn with_project_id(mut self, project_id: Option<String>) -> Self {
        self.project_id = project_id;
        self
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

    #[test]
    fn user_email_defaults_none_and_builder_sets_it() {
        let a = TenantContext::new("acme", "prod");
        assert_eq!(a.user_email, None);
        let b = TenantContext::new("acme", "prod").with_user_email(Some("u@x.com".into()));
        assert_eq!(b.user_email.as_deref(), Some("u@x.com"));
        // key_prefix is unaffected by user_email
        assert_eq!(b.key_prefix(), "aw:acme:prod");
    }

    #[test]
    fn project_id_defaults_none_and_builder_sets_it() {
        let a = TenantContext::new("acme", "prod");
        assert_eq!(
            a.project_id, None,
            "unknown pack identity must stay None, never a placeholder"
        );
        let b = TenantContext::new("acme", "prod").with_project_id(Some("customer.support".into()));
        assert_eq!(b.project_id.as_deref(), Some("customer.support"));
        // key_prefix is unaffected by project_id — Redis keys must not move.
        assert_eq!(b.key_prefix(), "aw:acme:prod");
    }
}
