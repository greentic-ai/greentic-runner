use greentic_types::telemetry::{attr_keys, set_current_tenant_ctx};
use greentic_types::{EnvId, TenantCtx, TenantId};
use rand::{RngExt, rng};
use std::str::FromStr;
use tracing::Span;

pub const PROVIDER_ID: &str = "greentic-runner";

#[derive(Debug, Clone)]
pub struct FlowSpanAttributes<'a> {
    pub tenant: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub action: Option<&'a str>,
}

pub fn annotate_span(span: &Span, attrs: &FlowSpanAttributes<'_>) {
    span.record("tenant", attrs.tenant);
    span.record("flow_id", attrs.flow_id);
    if let Some(node) = attrs.node_id {
        span.record("node_id", node);
    }
    if let Some(tool) = attrs.tool {
        span.record("tool", tool);
    }
    if let Some(action) = attrs.action {
        span.record("action", action);
    }
}

pub fn tenant_context(
    env: &str,
    tenant: &str,
    flow_id: Option<&str>,
    node_id: Option<&str>,
    provider_id: Option<&str>,
    session_id: Option<&str>,
) -> TenantCtx {
    let env_id = EnvId::from_str(env).expect("invalid env id");
    let tenant_id = TenantId::from_str(tenant).expect("invalid tenant id");
    let mut ctx = TenantCtx::new(env_id, tenant_id);
    let provider = provider_id.unwrap_or(PROVIDER_ID);
    ctx = ctx.with_provider(provider.to_string());
    if let Some(flow) = flow_id {
        ctx = ctx.with_flow(flow.to_string());
    }
    if let Some(node) = node_id {
        ctx = ctx.with_node(node.to_string());
    }
    if let Some(session) = session_id {
        ctx = ctx.with_session(session.to_string());
    }
    ctx
}

pub fn set_flow_context(
    env: &str,
    tenant: &str,
    flow_id: &str,
    node_id: Option<&str>,
    provider_id: Option<&str>,
    session_id: Option<&str>,
) {
    let ctx = tenant_context(env, tenant, Some(flow_id), node_id, provider_id, session_id);
    set_current_tenant_ctx(&ctx);
}

/// Deploy-spec rollout identifiers stamped onto the per-invocation
/// [`TenantCtx`] for telemetry attribution (B11). All optional — the producer
/// (the revision dispatcher resolving a deployment/revision) is Phase D, so
/// today these are `None` and [`stamp_rollout_ids`] is a no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolloutIds {
    pub customer_id: Option<String>,
    pub deployment_id: Option<String>,
    pub bundle_id: Option<String>,
    pub revision_id: Option<String>,
}

impl RolloutIds {
    /// True when no identifier is set (the common case until Phase D wires the
    /// dispatcher producer).
    pub fn is_empty(&self) -> bool {
        self.customer_id.is_none()
            && self.deployment_id.is_none()
            && self.bundle_id.is_none()
            && self.revision_id.is_none()
    }
}

/// Stamp the rollout IDs onto `ctx.attributes` under the canonical
/// [`attr_keys`](greentic_types::telemetry::attr_keys), so the telemetry bridge
/// (`set_current_tenant_ctx`) copies them into `TelemetryCtx` for spans/logs.
///
/// Authoritative over these four keys: a present ID is written, an absent one
/// is cleared. Stamping is therefore safe to re-run on a reused `TenantCtx`
/// (e.g. a session that migrates between revisions) — an ID dropped on a
/// re-stamp won't linger as a stale attribute from an earlier stamp.
pub fn stamp_rollout_ids(ctx: &mut TenantCtx, ids: &RolloutIds) {
    set_or_clear(ctx, attr_keys::CUSTOMER_ID, ids.customer_id.as_deref());
    set_or_clear(ctx, attr_keys::DEPLOYMENT_ID, ids.deployment_id.as_deref());
    set_or_clear(ctx, attr_keys::BUNDLE_ID, ids.bundle_id.as_deref());
    set_or_clear(ctx, attr_keys::REVISION_ID, ids.revision_id.as_deref());
}

fn set_or_clear(ctx: &mut TenantCtx, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            ctx.attributes.insert(key.to_string(), v.to_string());
        }
        None => {
            ctx.attributes.remove(key);
        }
    }
}

pub fn backoff_delay_ms(base: u64, attempt: u32) -> u64 {
    let multiplier = 1_u64 << attempt.min(10);
    let exp = base.saturating_mul(multiplier);
    let mut rng = rng();
    let jitter = rng.random_range(0..=exp.min(1000));
    exp + jitter
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TenantCtx {
        tenant_context("prod-eu", "acme", None, None, None, None)
    }

    #[test]
    fn stamp_sets_present_ids_under_canonical_keys() {
        let mut c = ctx();
        let ids = RolloutIds {
            customer_id: Some("cust-acme".into()),
            deployment_id: Some("01JTKS".into()),
            bundle_id: Some("customer.support".into()),
            revision_id: Some("01JTKR".into()),
        };
        stamp_rollout_ids(&mut c, &ids);
        assert_eq!(
            c.attributes.get(attr_keys::CUSTOMER_ID).map(String::as_str),
            Some("cust-acme")
        );
        assert_eq!(
            c.attributes
                .get(attr_keys::DEPLOYMENT_ID)
                .map(String::as_str),
            Some("01JTKS")
        );
        assert_eq!(
            c.attributes.get(attr_keys::BUNDLE_ID).map(String::as_str),
            Some("customer.support")
        );
        assert_eq!(
            c.attributes.get(attr_keys::REVISION_ID).map(String::as_str),
            Some("01JTKR")
        );
    }

    #[test]
    fn stamp_empty_is_noop() {
        let mut c = ctx();
        let before = c.attributes.len();
        stamp_rollout_ids(&mut c, &RolloutIds::default());
        assert_eq!(c.attributes.len(), before);
        assert!(RolloutIds::default().is_empty());
    }

    #[test]
    fn stamp_only_sets_present_subset() {
        let mut c = ctx();
        stamp_rollout_ids(
            &mut c,
            &RolloutIds {
                deployment_id: Some("01JTKS".into()),
                ..Default::default()
            },
        );
        assert!(c.attributes.contains_key(attr_keys::DEPLOYMENT_ID));
        assert!(!c.attributes.contains_key(attr_keys::CUSTOMER_ID));
    }

    #[test]
    fn stamp_clears_stale_ids_on_restamp() {
        let mut c = ctx();
        stamp_rollout_ids(
            &mut c,
            &RolloutIds {
                customer_id: Some("cust-acme".into()),
                deployment_id: Some("01JTKS".into()),
                bundle_id: Some("customer.support".into()),
                revision_id: Some("01JTKR".into()),
            },
        );
        // Re-stamp with only a new revision (e.g. a session migrating revisions):
        // the other three IDs must be cleared, not left stale from the first stamp.
        stamp_rollout_ids(
            &mut c,
            &RolloutIds {
                revision_id: Some("01JTKZ".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            c.attributes.get(attr_keys::REVISION_ID).map(String::as_str),
            Some("01JTKZ")
        );
        assert!(!c.attributes.contains_key(attr_keys::CUSTOMER_ID));
        assert!(!c.attributes.contains_key(attr_keys::DEPLOYMENT_ID));
        assert!(!c.attributes.contains_key(attr_keys::BUNDLE_ID));
    }
}
