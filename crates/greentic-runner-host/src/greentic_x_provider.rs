use std::sync::Arc;

use crate::component_api::node::{ExecCtx as ComponentExecCtx, TenantCtx as ComponentTenantCtx};
use crate::pack::PackRuntime;
use greentic_x_runtime::{
    ComponentInvocationEnvelope, ComponentInvocationResultEnvelope, ComponentProvider, RuntimeError,
};
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};

/// Greentic-X component provider backed by a materialized Greentic runner pack.
///
/// The provider intentionally invokes components that are already present in the
/// loaded pack runtime. OCI/component resolution remains the responsibility of
/// pack materialization; the Greentic-X envelope supplies the stable invocation
/// contract used by descriptor-driven solutions such as Telco-X.
pub struct RunnerPackComponentProvider {
    pack: Arc<PackRuntime>,
    runtime: Runtime,
    default_operation: String,
    tenant: String,
    flow_id: String,
}

impl RunnerPackComponentProvider {
    pub fn new(pack: Arc<PackRuntime>) -> Result<Self, RuntimeError> {
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| RuntimeError::ComponentInvocationFailed {
                component_id: "runner-pack-provider".to_owned(),
                message: format!("failed to create component invocation runtime: {err}"),
            })?;
        Ok(Self {
            pack,
            runtime,
            default_operation: "invoke".to_owned(),
            tenant: "default".to_owned(),
            flow_id: "greentic-x.component-invocation".to_owned(),
        })
    }

    pub fn with_default_operation(mut self, operation: impl Into<String>) -> Self {
        self.default_operation = operation.into();
        self
    }

    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    pub fn with_flow_id(mut self, flow_id: impl Into<String>) -> Self {
        self.flow_id = flow_id.into();
        self
    }

    fn component_ref(envelope: &ComponentInvocationEnvelope) -> String {
        metadata_string(envelope, "component_ref").unwrap_or_else(|| envelope.component_id.clone())
    }

    fn operation(&self, envelope: &ComponentInvocationEnvelope) -> String {
        metadata_string(envelope, "operation").unwrap_or_else(|| self.default_operation.clone())
    }

    fn exec_ctx(&self, envelope: &ComponentInvocationEnvelope) -> ComponentExecCtx {
        ComponentExecCtx {
            tenant: ComponentTenantCtx {
                tenant: self.tenant.clone(),
                team: None,
                user: Some(envelope.provenance.actor.actor_id.to_string()),
                trace_id: envelope.provenance.trace_id.clone(),
                i18n_id: None,
                correlation_id: envelope
                    .provenance
                    .correlation_id
                    .clone()
                    .or_else(|| envelope.run_id.clone()),
                deadline_unix_ms: None,
                attempt: 1,
                idempotency_key: Some(envelope.invocation_id.clone()),
            },
            i18n_id: None,
            flow_id: envelope
                .run_id
                .clone()
                .unwrap_or_else(|| self.flow_id.clone()),
            node_id: Some(envelope.component_id.clone()),
        }
    }
}

impl ComponentProvider for RunnerPackComponentProvider {
    fn invoke_component(
        &self,
        envelope: ComponentInvocationEnvelope,
    ) -> Result<ComponentInvocationResultEnvelope, RuntimeError> {
        let component_ref = Self::component_ref(&envelope);
        let operation = self.operation(&envelope);
        let ctx = self.exec_ctx(&envelope);
        let input_json = serde_json::to_string(&envelope.input).map_err(|err| {
            RuntimeError::ComponentInvocationFailed {
                component_id: envelope.component_id.clone(),
                message: format!("failed to serialize component input: {err}"),
            }
        })?;
        let invocation_id = envelope.invocation_id.clone();
        let component_id = envelope.component_id.clone();
        let pack = Arc::clone(&self.pack);

        let output = self
            .runtime
            .block_on(async move {
                pack.invoke_component(&component_ref, ctx, &operation, None, input_json)
                    .await
            })
            .map_err(|err| RuntimeError::ComponentInvocationFailed {
                component_id: component_id.clone(),
                message: err.to_string(),
            })?;

        Ok(ComponentInvocationResultEnvelope::success(
            invocation_id,
            component_id,
            output,
        ))
    }
}

fn metadata_string(envelope: &ComponentInvocationEnvelope, key: &str) -> Option<String> {
    envelope
        .metadata
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use greentic_x_runtime::{ComponentDescriptor, ComponentRuntimeKind};
    use greentic_x_types::{ActorRef, Provenance};

    use super::*;

    #[test]
    fn component_ref_prefers_metadata_mapping() {
        let mut descriptor = ComponentDescriptor::new(
            "zain.analyser.rca",
            "analyser",
            ComponentRuntimeKind::WasmWasi,
            "oci://example/zain-analyser:latest",
        );
        descriptor.metadata.insert(
            "component_ref".to_owned(),
            Value::String("zain-analyser".to_owned()),
        );
        let envelope = ComponentInvocationEnvelope::new(
            "invoke-1",
            &descriptor,
            Value::Null,
            Provenance::new(ActorRef::service("test").expect("actor")),
        );

        assert_eq!(
            RunnerPackComponentProvider::component_ref(&envelope),
            "zain-analyser"
        );
    }
}
