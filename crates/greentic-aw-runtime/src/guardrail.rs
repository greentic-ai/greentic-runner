use greentic_ext_runtime::capability::CapabilityRegistry;
use greentic_extension_sdk_contract::{CapabilityId, CapabilityRef as ExtCapabilityRef};

use crate::config::GuardrailRef;
use crate::tenant::TenantContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardrailDirection {
    Inbound,
    Outbound,
}

impl GuardrailDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardrailDirection::Inbound => "inbound",
            GuardrailDirection::Outbound => "outbound",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GuardrailDenyInfo {
    pub code: String,
    pub message: String,
    pub details: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GuardrailVerdict {
    Accept,
    Update(String),
    Deny(GuardrailDenyInfo),
}

#[derive(Clone, Debug)]
pub struct GuardrailInput {
    pub direction: GuardrailDirection,
    pub content: String,
    pub agent_id: String,
    pub session_id: String,
    pub tenant_id: String,
    pub env_id: String,
    pub context: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedGuardrail {
    pub extension_id: String,
    pub cap_id: String,
    pub mandatory: bool,
    pub config: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct GuardrailInvokeError(pub String);

pub trait GuardrailEvaluator: Send + Sync {
    fn evaluate(
        &self,
        extension_id: &str,
        input: &GuardrailInput,
    ) -> Result<GuardrailVerdict, GuardrailInvokeError>;
}

pub trait GuardrailPolicy: Send + Sync {
    fn mandatory_guardrails(&self, tenant: &TenantContext) -> Vec<GuardrailRef>;
}

pub struct NoMandatoryGuardrails;

impl GuardrailPolicy for NoMandatoryGuardrails {
    fn mandatory_guardrails(&self, _tenant: &TenantContext) -> Vec<GuardrailRef> {
        Vec::new()
    }
}

pub struct StaticGuardrailPolicy(pub Vec<GuardrailRef>);

impl GuardrailPolicy for StaticGuardrailPolicy {
    fn mandatory_guardrails(&self, _tenant: &TenantContext) -> Vec<GuardrailRef> {
        self.0.clone()
    }
}

/// Resolve a single `GuardrailRef` through the capability registry.
///
/// Returns `None` and logs a warning if the cap_id cannot be parsed or has no
/// matching offering registered.
fn resolve_one(
    registry: &CapabilityRegistry,
    guardrail_ref: &GuardrailRef,
    is_mandatory: bool,
) -> Option<ResolvedGuardrail> {
    let cap_id: CapabilityId = match guardrail_ref.cap_id.parse() {
        Ok(id) => id,
        Err(_) => {
            tracing::warn!(
                cap_id = %guardrail_ref.cap_id,
                "guardrail cap_id is not a valid capability ID (expected namespace:path); skipping"
            );
            return None;
        }
    };

    let required = [ExtCapabilityRef {
        id: cap_id.clone(),
        version: "*".to_string(),
        deprecated: None,
    }];
    let plan = registry.resolve("agentic-worker", &required);

    match plan.resolved.get(&cap_id) {
        Some(binding) => Some(ResolvedGuardrail {
            extension_id: binding.extension_id.clone(),
            cap_id: guardrail_ref.cap_id.clone(),
            mandatory: is_mandatory,
            config: guardrail_ref.config.clone(),
        }),
        None => {
            tracing::warn!(
                cap_id = %guardrail_ref.cap_id,
                "guardrail capability unresolved; skipping"
            );
            None
        }
    }
}

/// Assemble an ordered guardrail chain from mandatory and agent-level refs.
///
/// Mandatory guardrails appear first (in list order), followed by agent-level
/// guardrails (in list order). Refs that cannot be resolved via the capability
/// registry are silently skipped (a warning is logged).
pub fn assemble_chain(
    registry: &CapabilityRegistry,
    mandatory: &[GuardrailRef],
    agent: &[GuardrailRef],
) -> Vec<ResolvedGuardrail> {
    let mut chain = Vec::new();
    for guardrail_ref in mandatory {
        if let Some(resolved) = resolve_one(registry, guardrail_ref, true) {
            chain.push(resolved);
        }
    }
    for guardrail_ref in agent {
        if let Some(resolved) = resolve_one(registry, guardrail_ref, false) {
            chain.push(resolved);
        }
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mandatory_policy_is_empty() {
        let policy = NoMandatoryGuardrails;
        let t = TenantContext::new("t1", "dev");
        assert!(policy.mandatory_guardrails(&t).is_empty());
    }

    #[test]
    fn static_policy_returns_its_list() {
        use crate::config::GuardrailRef;
        let refs = vec![GuardrailRef {
            cap_id: "greentic.cap.guardrail.v1".into(),
            offer_id: None,
            config: serde_json::Value::Null,
        }];
        let policy = StaticGuardrailPolicy(refs.clone());
        let t = TenantContext::new("t1", "dev");
        assert_eq!(policy.mandatory_guardrails(&t), refs);
    }

    // cap IDs must be "namespace:path" format to satisfy CapabilityId::from_str.
    // Using "greentic:guardrail/pii" and "greentic:guardrail/toxicity" as
    // spec-compliant stand-ins for the brief's dot-separated IDs.
    #[test]
    fn assemble_chain_orders_mandatory_first_then_agent() {
        use greentic_ext_runtime::capability::{CapabilityRegistry, OfferedBinding};
        use greentic_extension_sdk_contract::ExtensionKind;

        let mut registry = CapabilityRegistry::new();
        registry.add_offering(OfferedBinding {
            extension_id: "ext.pii".into(),
            cap_id: "greentic:guardrail/pii".parse().unwrap(),
            version: semver::Version::parse("1.0.0").unwrap(),
            kind: ExtensionKind::Design,
            export_path: String::new(),
        });
        registry.add_offering(OfferedBinding {
            extension_id: "ext.toxicity".into(),
            cap_id: "greentic:guardrail/toxicity".parse().unwrap(),
            version: semver::Version::parse("1.0.0").unwrap(),
            kind: ExtensionKind::Design,
            export_path: String::new(),
        });

        let mandatory = vec![GuardrailRef {
            cap_id: "greentic:guardrail/toxicity".into(),
            offer_id: None,
            config: serde_json::Value::Null,
        }];
        let agent = vec![GuardrailRef {
            cap_id: "greentic:guardrail/pii".into(),
            offer_id: None,
            config: serde_json::Value::Null,
        }];

        let chain = assemble_chain(&registry, &mandatory, &agent);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].extension_id, "ext.toxicity");
        assert!(chain[0].mandatory);
        assert_eq!(chain[1].extension_id, "ext.pii");
        assert!(!chain[1].mandatory);
    }

    #[test]
    fn assemble_chain_skips_unresolved() {
        use greentic_ext_runtime::capability::CapabilityRegistry;

        let registry = CapabilityRegistry::new();
        let agent = vec![GuardrailRef {
            cap_id: "greentic:guardrail/missing".into(),
            offer_id: None,
            config: serde_json::Value::Null,
        }];
        let chain = assemble_chain(&registry, &[], &agent);
        assert!(chain.is_empty());
    }
}
