use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use greentic_types::provider::ProviderRuntimeRef;

use crate::pack::PackRuntime;

#[derive(Clone, Debug)]
pub struct OperatorBinding {
    pub provider_id: Option<String>,
    pub provider_type: String,
    pub op_id: String,
    pub runtime: ProviderRuntimeRef,
    pub pack_ref: String,
    pub pack_digest: Option<String>,
    pub config_schema_ref: Option<String>,
    pub state_schema_ref: Option<String>,
    pub docs_ref: Option<String>,
    pub capabilities: Vec<String>,
    pub pack_priority: usize,
}

#[derive(Debug)]
pub enum OperatorResolveError {
    ProviderNotFound,
    OpNotFound,
}

pub struct OperatorRegistry {
    per_provider_id: HashMap<String, HashMap<String, OperatorBinding>>,
    per_provider_type: HashMap<String, HashMap<String, OperatorBinding>>,
}

impl OperatorRegistry {
    pub fn build(packs: &[(Arc<PackRuntime>, Option<String>)]) -> Result<OperatorRegistry> {
        let mut per_provider_id: HashMap<String, HashMap<String, OperatorBinding>> = HashMap::new();
        let mut per_provider_type: HashMap<String, HashMap<String, OperatorBinding>> =
            HashMap::new();

        for (pack_priority, (pack, digest)) in packs.iter().enumerate() {
            let pack_meta = pack.metadata();
            let computed_ref = format!("{}@{}", pack_meta.pack_id, pack_meta.version);
            let registry = match pack.provider_registry_optional() {
                Ok(Some(registry)) => registry,
                Ok(None) => continue,
                Err(err) => {
                    return Err(err.context(format!(
                        "failed to build provider registry for pack {}",
                        pack_meta.pack_id
                    )));
                }
            };
            for provider in registry.operator_metadata() {
                let pack_ref = provider
                    .pack_ref
                    .clone()
                    .unwrap_or_else(|| computed_ref.clone());
                for op_id in &provider.ops {
                    let binding = OperatorBinding {
                        provider_id: provider.provider_id.clone(),
                        provider_type: provider.provider_type.clone(),
                        op_id: op_id.clone(),
                        runtime: provider.runtime.clone(),
                        pack_ref: pack_ref.clone(),
                        pack_digest: digest.clone(),
                        config_schema_ref: provider.config_schema_ref.clone(),
                        state_schema_ref: provider.state_schema_ref.clone(),
                        docs_ref: provider.docs_ref.clone(),
                        capabilities: provider.capabilities.clone(),
                        pack_priority,
                    };
                    if let Some(provider_id) = binding.provider_id.clone() {
                        per_provider_id
                            .entry(provider_id)
                            .or_default()
                            .insert(op_id.clone(), binding.clone());
                    }
                    per_provider_type
                        .entry(binding.provider_type.clone())
                        .or_default()
                        .insert(op_id.clone(), binding);
                }
            }
        }

        Ok(OperatorRegistry {
            per_provider_id,
            per_provider_type,
        })
    }

    pub fn resolve(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
        op_id: &str,
    ) -> Result<&OperatorBinding, OperatorResolveError> {
        // Strict identity semantics: a caller supplying `provider_id` opts
        // into id-keyed lookup. If the id is unknown to this registry, fail
        // loudly rather than silently rebinding to the type index (which
        // would route a stale/typo'd instance id to a generic type-level
        // binding with different config). State-store-backed instances —
        // which never appear in `per_provider_id` because the registry is
        // built from inline pack decls only — are resolved by the
        // invoke_operator probe (operator.rs), which retries with
        // (None, Some(derived_type)) once it learns the type from the
        // instance file.
        if let Some(id) = provider_id {
            let ops = self
                .per_provider_id
                .get(id)
                .ok_or(OperatorResolveError::ProviderNotFound)?;
            return ops.get(op_id).ok_or(OperatorResolveError::OpNotFound);
        }
        if let Some(ty) = provider_type {
            let ops = self
                .per_provider_type
                .get(ty)
                .ok_or(OperatorResolveError::ProviderNotFound)?;
            return ops.get(op_id).ok_or(OperatorResolveError::OpNotFound);
        }
        Err(OperatorResolveError::ProviderNotFound)
    }
}
