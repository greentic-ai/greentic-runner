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
}
