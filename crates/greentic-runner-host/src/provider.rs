use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use greentic_types::{
    EnvId, PackManifest, ProviderRuntimeRef, StateKey as StoreStateKey, TenantCtx, TenantId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::storage::DynStateStore;
use crate::storage::state::STATE_PREFIX;

#[derive(Clone, Debug, Serialize)]
pub struct ProviderBinding {
    pub provider_id: Option<String>,
    pub provider_type: String,
    pub component_ref: String,
    pub export: String,
    pub world: String,
    pub config_json: Option<String>,
    pub pack_ref: Option<String>,
}

#[derive(Clone, Debug)]
pub struct OperatorProviderMetadata {
    pub provider_id: Option<String>,
    pub provider_type: String,
    pub capabilities: Vec<String>,
    pub ops: Vec<String>,
    pub config_schema_ref: Option<String>,
    pub state_schema_ref: Option<String>,
    pub runtime: ProviderRuntimeRef,
    pub docs_ref: Option<String>,
    pub pack_ref: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderInstance {
    provider_id: String,
    provider_type: String,
    pack_ref: Option<String>,
    component_ref: String,
    export: String,
    world: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    config: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderExtRuntime {
    component_ref: String,
    export: String,
    world: String,
}

#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
struct ProviderExtDecl {
    #[serde(default)]
    provider_id: Option<String>,
    provider_type: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    ops: Vec<String>,
    #[serde(default)]
    config_schema_ref: Option<String>,
    #[serde(default)]
    state_schema_ref: Option<String>,
    runtime: ProviderExtRuntime,
    #[serde(default)]
    docs_ref: Option<String>,
}

#[derive(Clone)]
pub struct ProviderRegistry {
    pack_ref: Option<String>,
    inline: Vec<ProviderExtDecl>,
    state_store: Option<DynStateStore>,
    tenant: TenantCtx,
}

impl ProviderRegistry {
    pub fn new(
        manifest: &PackManifest,
        state_store: Option<DynStateStore>,
        tenant: &str,
        env: &str,
    ) -> Result<Self> {
        let inline = extract_inline_providers(manifest)?;
        let tenant_ctx = TenantCtx::new(
            EnvId::from_str(env).unwrap_or_else(|_| EnvId::from_str("local").expect("local env")),
            TenantId::from_str(tenant).with_context(|| format!("invalid tenant id `{tenant}`"))?,
        );
        let pack_ref = Some(format!(
            "{}@{}",
            manifest.pack_id.as_str(),
            manifest.version
        ));
        Ok(Self {
            pack_ref,
            inline,
            state_store,
            tenant: tenant_ctx,
        })
    }

    pub fn operator_metadata(&self) -> Vec<OperatorProviderMetadata> {
        self.inline
            .iter()
            .map(|decl| OperatorProviderMetadata {
                provider_id: decl.provider_id.clone(),
                provider_type: decl.provider_type.clone(),
                capabilities: decl.capabilities.clone(),
                ops: decl.ops.clone(),
                config_schema_ref: decl.config_schema_ref.clone(),
                state_schema_ref: decl.state_schema_ref.clone(),
                runtime: ProviderRuntimeRef {
                    component_ref: decl.runtime.component_ref.clone(),
                    export: decl.runtime.export.clone(),
                    world: decl.runtime.world.clone(),
                },
                docs_ref: decl.docs_ref.clone(),
                pack_ref: self.pack_ref.clone(),
            })
            .collect()
    }

    pub fn resolve(
        &self,
        provider_id: Option<&str>,
        provider_type: Option<&str>,
    ) -> Result<ProviderBinding> {
        if provider_id.is_none() && provider_type.is_none() {
            bail!("provider.invoke requires provider_id or provider_type");
        }

        if let Some(id) = provider_id {
            let binding = if let Some(binding) = self.load_instance(id)? {
                binding
            } else if let Some(ext) = self
                .inline
                .iter()
                .find(|decl| decl.provider_id.as_deref() == Some(id))
            {
                binding_from_decl(ext, self.pack_ref.clone(), None)
            } else {
                bail!("provider_id `{id}` not found");
            };

            // Defense-in-depth: when caller supplies both, the resolved
            // binding's provider_type must match the requested provider_type.
            // Catches drift between an instance file's provider_type and the
            // caller's expectation (e.g. an instance retyped from Teams→Slack
            // while flows still target it by id).
            if let Some(ty) = provider_type
                && binding.provider_type != ty
            {
                bail!(
                    "provider_id `{id}` resolved to provider_type `{}`, but caller requested `{ty}`",
                    binding.provider_type
                );
            }
            return Ok(binding);
        }

        let provider_type = provider_type.unwrap();
        let matches: Vec<_> = self
            .inline
            .iter()
            .filter(|decl| decl.provider_type == provider_type)
            .collect();
        match matches.as_slice() {
            [] => bail!("no provider runtime found for type `{provider_type}`"),
            [decl] => Ok(binding_from_decl(
                decl,
                self.pack_ref.clone(),
                Some(provider_type.to_string()),
            )),
            _ => bail!("multiple providers found for type `{provider_type}`, specify provider_id"),
        }
    }

    fn load_instance(&self, provider_id: &str) -> Result<Option<ProviderBinding>> {
        let store = match &self.state_store {
            Some(store) => Arc::clone(store),
            None => return Ok(None),
        };
        let key = StoreStateKey::from(format!("providers/instances/{provider_id}.json"));
        let value = store
            .get_json(&self.tenant, STATE_PREFIX, &key, None)
            .map_err(|err| anyhow!(err.to_string()))
            .with_context(|| format!("failed to load provider instance `{provider_id}`"))?;
        let Some(doc) = value else {
            return Ok(None);
        };
        let instance: ProviderInstance = serde_json::from_value(doc)
            .with_context(|| format!("invalid provider instance `{provider_id}`"))?;
        if !instance.enabled {
            bail!("provider `{provider_id}` is disabled");
        }
        Ok(Some(binding_from_instance(instance)))
    }
}

fn extract_inline_providers(manifest: &PackManifest) -> Result<Vec<ProviderExtDecl>> {
    let Some(inline) = manifest.provider_extension_inline() else {
        return Ok(Vec::new());
    };

    // Trust boundary: validate the wire payload before building any runtime
    // registry from it. validate_basic enforces non-empty/unique provider_type,
    // unique non-empty provider_id, no cross-namespace collisions, and
    // populated runtime fields. Without this gate, OperatorRegistry::build
    // (last-wins HashMap) and ProviderRegistry::resolve (first-wins Vec scan)
    // can diverge on a pack with duplicate provider_ids, routing op metadata
    // to one decl and the actual runtime invocation to another.
    inline
        .validate_basic()
        .context("provider extension inline failed validation")?;

    let providers = inline
        .providers
        .iter()
        .map(|provider| ProviderExtDecl {
            // M1.1b hard-cutover: preserve the wire `provider_id` verbatim
            // (Option<String>). Previously synthesized as `Some(provider_type)`,
            // which conflated identity with type and made cross-namespace
            // collisions ambiguous.
            provider_id: provider.provider_id.clone(),
            provider_type: provider.provider_type.clone(),
            capabilities: provider.capabilities.clone(),
            ops: provider.ops.clone(),
            config_schema_ref: Some(provider.config_schema_ref.clone()),
            state_schema_ref: provider.state_schema_ref.clone(),
            runtime: ProviderExtRuntime {
                component_ref: provider.runtime.component_ref.clone(),
                export: provider.runtime.export.clone(),
                world: provider.runtime.world.clone(),
            },
            docs_ref: provider.docs_ref.clone(),
        })
        .collect();

    Ok(providers)
}

fn binding_from_decl(
    decl: &ProviderExtDecl,
    pack_ref: Option<String>,
    default_provider_id: Option<String>,
) -> ProviderBinding {
    ProviderBinding {
        provider_id: decl.provider_id.clone().or(default_provider_id),
        provider_type: decl.provider_type.clone(),
        component_ref: decl.runtime.component_ref.clone(),
        export: decl.runtime.export.clone(),
        world: decl.runtime.world.clone(),
        config_json: None,
        pack_ref,
    }
}

fn binding_from_instance(instance: ProviderInstance) -> ProviderBinding {
    ProviderBinding {
        config_json: if instance.config.is_null() {
            None
        } else {
            Some(instance.config.to_string())
        },
        provider_id: Some(instance.provider_id),
        provider_type: instance.provider_type,
        component_ref: instance.component_ref,
        export: instance.export,
        world: instance.world,
        pack_ref: instance.pack_ref,
    }
}
