//! Pack runtime - the main entry point for loading and executing packs.

use super::component_state::{ComponentState, add_component_control_to_linker, register_all};
use super::flows::{FlowDescriptor, ManifestLoad, PackFlows, flow_kind_to_str, load_manifest_and_flows, load_manifest_and_flows_from_dir};
use super::helpers::{HTTP_CLIENT, deserialize_json_bytes, locate_pack_assets, normalize_pack_path, normalize_schema_ref, path_is_gtpack, run_on_wasi_thread};
use super::host_state::HostState;
use super::i18n::I18nCatalog;
use super::loaders::{PackComponent, compile_component_with_cache, load_components_from_archive, load_components_from_dir, load_components_from_overrides, load_components_from_sources};
use super::metadata::PackMetadata;
use super::resolution::{ComponentResolution, component_sources_table, component_sources_table_from_pack_lock, component_specs, find_pack_lock_roots, load_pack_lock};
use crate::cache::{CacheConfig, CacheManager, CpuPolicy, EngineProfile};
use crate::component_api::node::ExecCtx as ComponentExecCtx;
use crate::config::HostConfig;
use crate::oauth::OAuthBrokerConfig;
use crate::provider::{ProviderBinding, ProviderRegistry};
use crate::provider_core::{
    schema_core::SchemaCorePre as LegacySchemaCorePre,
    schema_core_schema::SchemaCorePre as SchemaSchemaCorePre,
};
use crate::runner::mocks::MockLayer;
use crate::runtime_wasmtime::{Engine, Linker};
use crate::secrets::{DynSecretsManager, read_secret_blocking};
use crate::storage::{DynSessionStore, DynStateStore};
use crate::verify;
use crate::wasi::{PreopenSpec, RunnerWasiPolicy};
use anyhow::{Context, Result, anyhow, bail};
use futures::executor::block_on;
use greentic_pack::builder as legacy_pack;
use greentic_types::{ComponentManifest, Flow, TenantCtx as TypesTenantCtx};
use parking_lot::{Mutex, RwLock};
use reqwest::blocking::Client as BlockingClient;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::fs;
use tracing::warn;
use zip::ZipArchive;

#[cfg(feature = "fault-injection")]
use crate::testing::fault_injection::{FaultContext, FaultPoint, maybe_fail};

/// Runtime for executing packs and their components.
#[allow(dead_code)]
pub struct PackRuntime {
    path: PathBuf,
    archive_path: Option<PathBuf>,
    pub(crate) config: Arc<HostConfig>,
    pub(crate) engine: Engine,
    metadata: PackMetadata,
    manifest: Option<greentic_types::PackManifest>,
    legacy_manifest: Option<Box<legacy_pack::PackManifest>>,
    component_manifests: HashMap<String, ComponentManifest>,
    pub(crate) mocks: Option<Arc<MockLayer>>,
    flows: Option<PackFlows>,
    pub(crate) components: HashMap<String, PackComponent>,
    pub(crate) http_client: Arc<BlockingClient>,
    pre_cache: Mutex<HashMap<String, wasmtime::component::InstancePre<ComponentState>>>,
    pub(crate) session_store: Option<DynSessionStore>,
    pub(crate) state_store: Option<DynStateStore>,
    pub(crate) wasi_policy: Arc<RunnerWasiPolicy>,
    assets_tempdir: Option<TempDir>,
    provider_registry: RwLock<Option<ProviderRegistry>>,
    pub(crate) secrets: DynSecretsManager,
    pub(crate) oauth_config: Option<OAuthBrokerConfig>,
    cache: CacheManager,
}

impl PackRuntime {
    /// Check if state store is allowed for a component.
    pub(crate) fn allows_state_store(&self, component_ref: &str) -> bool {
        if self.state_store.is_none() {
            return false;
        }
        if !self.config.state_store_policy.allow {
            return false;
        }
        let Some(manifest) = self.component_manifests.get(component_ref) else {
            return true;
        };
        manifest
            .capabilities
            .host
            .state
            .as_ref()
            .map(|caps| caps.read || caps.write)
            .unwrap_or(true)
    }

    /// Check if pack contains a component.
    pub fn contains_component(&self, component_ref: &str) -> bool {
        self.components.contains_key(component_ref)
    }

    /// Load a pack from path.
    #[allow(clippy::too_many_arguments)]
    pub async fn load(
        path: impl AsRef<Path>,
        config: Arc<HostConfig>,
        mocks: Option<Arc<MockLayer>>,
        archive_source: Option<&Path>,
        session_store: Option<DynSessionStore>,
        state_store: Option<DynStateStore>,
        wasi_policy: Arc<RunnerWasiPolicy>,
        secrets: DynSecretsManager,
        oauth_config: Option<OAuthBrokerConfig>,
        verify_archive: bool,
        component_resolution: ComponentResolution,
    ) -> Result<Self> {
        let path = path.as_ref();
        let (_pack_root, safe_path) = normalize_pack_path(path)?;
        let path_meta = std::fs::metadata(&safe_path).ok();
        let is_dir = path_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let is_component = !is_dir && safe_path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("wasm")).unwrap_or(false);

        let archive_hint_path = if let Some(source) = archive_source {
            let (_, normalized) = normalize_pack_path(source)?;
            Some(normalized)
        } else if is_component || is_dir {
            None
        } else {
            Some(safe_path.clone())
        };
        let archive_hint = archive_hint_path.as_deref();

        if verify_archive {
            if let Some(verify_target) = archive_hint.and_then(|p| {
                std::fs::metadata(p).ok().filter(|m| m.is_file()).map(|_| p)
            }) {
                verify::verify_pack(verify_target).await?;
                tracing::info!(pack_path = %verify_target.display(), "pack verification complete");
            }
        }

        let engine = Engine::default();
        let engine_profile = EngineProfile::from_engine(&engine, CpuPolicy::Native, "default".to_string());
        let cache = CacheManager::new(CacheConfig::default(), engine_profile);

        let mut metadata = PackMetadata::fallback(&safe_path);
        let mut manifest = None;
        let mut legacy_manifest: Option<Box<legacy_pack::PackManifest>> = None;
        let mut flows = None;

        let materialized_root = component_resolution.materialized_root.clone()
            .or_else(|| is_dir.then(|| safe_path.clone()));

        let (pack_assets_dir, assets_tempdir) = locate_pack_assets(materialized_root.as_deref(), archive_hint)?;

        // Load manifest from materialized root or archive
        if let Some(root) = materialized_root.as_ref() {
            match load_manifest_and_flows_from_dir(root) {
                Ok(ManifestLoad::New { manifest: m, flows: cache }) => {
                    metadata = cache.metadata.clone();
                    manifest = Some(*m);
                    flows = Some(cache);
                }
                Ok(ManifestLoad::Legacy { manifest: m, flows: cache }) => {
                    metadata = cache.metadata.clone();
                    legacy_manifest = Some(m);
                    flows = Some(cache);
                }
                Err(err) => {
                    warn!(error = %err, pack = %root.display(), "failed to parse materialized pack manifest");
                }
            }
        }

        if manifest.is_none() && legacy_manifest.is_none() {
            if let Some(archive_path) = archive_hint {
                match load_manifest_and_flows(archive_path) {
                    Ok(ManifestLoad::New { manifest: m, flows: cache }) => {
                        metadata = cache.metadata.clone();
                        manifest = Some(*m);
                        flows = Some(cache);
                    }
                    Ok(ManifestLoad::Legacy { manifest: m, flows: cache }) => {
                        metadata = cache.metadata.clone();
                        legacy_manifest = Some(m);
                        flows = Some(cache);
                    }
                    Err(err) => {
                        return Err(err).with_context(|| format!(
                            "failed to load manifest.cbor from {}", archive_path.display()
                        ));
                    }
                }
            }
        }

        #[cfg(feature = "fault-injection")]
        {
            let fault_ctx = FaultContext { pack_id: metadata.pack_id.as_str(), flow_id: "unknown", node_id: None, attempt: 1 };
            maybe_fail(FaultPoint::PackResolve, fault_ctx).map_err(|err| anyhow!(err.to_string()))?;
        }

        // Load pack lock and component sources
        let mut pack_lock = None;
        for root in find_pack_lock_roots(&safe_path, is_dir, archive_hint) {
            pack_lock = load_pack_lock(&root)?;
            if pack_lock.is_some() { break; }
        }

        let component_sources_payload = if pack_lock.is_none() {
            manifest.as_ref().and_then(|m| m.get_component_sources_v1().ok()).flatten()
        } else { None };

        let component_sources = if let Some(lock) = pack_lock.as_ref() {
            Some(component_sources_table_from_pack_lock(lock, component_resolution.allow_missing_hash)?)
        } else {
            component_sources_table(component_sources_payload.as_ref())?
        };

        // Load components
        let components = if is_component {
            let wasm_bytes = fs::read(&safe_path).await?;
            metadata = PackMetadata::from_wasm(&wasm_bytes).unwrap_or_else(|| PackMetadata::fallback(&safe_path));
            let name = safe_path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "component".to_string());
            let component = compile_component_with_cache(&cache, &engine, None, wasm_bytes).await?;
            let mut map = HashMap::new();
            map.insert(name.clone(), PackComponent { name, version: metadata.version.clone(), component });
            map
        } else {
            let specs = component_specs(manifest.as_ref(), legacy_manifest.as_deref(), component_sources_payload.as_ref(), pack_lock.as_ref());
            if specs.is_empty() {
                HashMap::new()
            } else {
                let mut loaded = HashMap::new();
                let mut missing: HashSet<String> = specs.iter().map(|s| s.id.clone()).collect();

                if !component_resolution.overrides.is_empty() {
                    load_components_from_overrides(&cache, &engine, &component_resolution.overrides, &specs, &mut missing, &mut loaded).await?;
                }
                if let Some(cs) = component_sources.as_ref() {
                    load_components_from_sources(&cache, &engine, cs, &component_resolution, &specs, &mut missing, &mut loaded, materialized_root.as_deref(), archive_hint).await?;
                }
                if let Some(root) = materialized_root.as_ref() {
                    load_components_from_dir(&cache, &engine, root, &specs, &mut missing, &mut loaded).await?;
                }
                if let Some(archive_path) = archive_hint {
                    load_components_from_archive(&cache, &engine, archive_path, &specs, &mut missing, &mut loaded).await?;
                }

                if !missing.is_empty() {
                    let missing_list = missing.into_iter().collect::<Vec<_>>().join(", ");
                    bail!("components missing: {}", missing_list);
                }
                loaded
            }
        };

        let mut component_manifests = HashMap::new();
        if let Some(m) = manifest.as_ref() {
            for c in &m.components {
                component_manifests.insert(c.id.as_str().to_string(), c.clone());
            }
        }

        let mut pack_policy = (*wasi_policy).clone();
        if let Some(dir) = pack_assets_dir {
            pack_policy = pack_policy.with_preopen(PreopenSpec::new(dir, "/assets").read_only(true));
        }

        Ok(Self {
            path: safe_path,
            archive_path: archive_hint.map(Path::to_path_buf),
            config,
            engine,
            metadata,
            manifest,
            legacy_manifest,
            component_manifests,
            mocks,
            flows,
            components,
            http_client: Arc::clone(&HTTP_CLIENT),
            pre_cache: Mutex::new(HashMap::new()),
            session_store,
            state_store,
            wasi_policy: Arc::new(pack_policy),
            assets_tempdir,
            provider_registry: RwLock::new(None),
            secrets,
            oauth_config,
            cache,
        })
    }

    /// List all flows in the pack.
    pub async fn list_flows(&self) -> Result<Vec<FlowDescriptor>> {
        if let Some(cache) = &self.flows {
            return Ok(cache.descriptors.clone());
        }
        if let Some(manifest) = &self.manifest {
            return Ok(manifest.flows.iter().map(|f| FlowDescriptor {
                id: f.id.as_str().to_string(),
                flow_type: flow_kind_to_str(f.kind).to_string(),
                pack_id: manifest.pack_id.as_str().to_string(),
                profile: manifest.pack_id.as_str().to_string(),
                version: manifest.version.to_string(),
                description: None,
            }).collect());
        }
        Ok(Vec::new())
    }

    /// Invoke a component.
    pub async fn invoke_component(
        &self,
        component_ref: &str,
        ctx: ComponentExecCtx,
        operation: &str,
        _config_json: Option<String>,
        input_json: String,
    ) -> Result<Value> {
        let pack_component = self.components.get(component_ref)
            .with_context(|| format!("component '{component_ref}' not found in pack"))?;
        let engine = self.engine.clone();
        let config = Arc::clone(&self.config);
        let http_client = Arc::clone(&self.http_client);
        let mocks = self.mocks.clone();
        let session_store = self.session_store.clone();
        let state_store = self.state_store.clone();
        let secrets = Arc::clone(&self.secrets);
        let oauth_config = self.oauth_config.clone();
        let wasi_policy = Arc::clone(&self.wasi_policy);
        let pack_id = self.metadata.pack_id.clone();
        let allow_state_store = self.allows_state_store(component_ref);
        let component = pack_component.component.clone();
        let component_ref_owned = component_ref.to_string();
        let operation_owned = operation.to_string();

        run_on_wasi_thread("component.invoke", move || {
            let mut linker = Linker::new(&engine);
            register_all(&mut linker, allow_state_store)?;
            add_component_control_to_linker(&mut linker)?;

            let host_state = HostState::new(pack_id, config, http_client, mocks, session_store, state_store, secrets, oauth_config, Some(ctx.clone()), Some(component_ref_owned), false)?;
            let store_state = ComponentState::new(host_state, wasi_policy)?;
            let mut store = wasmtime::Store::new(&engine, store_state);

            let result = HostState::instantiate_component_result(&mut linker, &mut store, &component, &ctx, &operation_owned, &input_json)?;
            HostState::convert_invoke_result(result)
        })
    }

    /// Load a flow by ID.
    pub fn load_flow(&self, flow_id: &str) -> Result<Flow> {
        if let Some(cache) = &self.flows {
            return cache.flows.get(flow_id).cloned()
                .ok_or_else(|| anyhow!("flow '{flow_id}' not found in pack"));
        }
        if let Some(manifest) = &self.manifest {
            let entry = manifest.flows.iter().find(|f| f.id.as_str() == flow_id)
                .ok_or_else(|| anyhow!("flow '{flow_id}' not found in manifest"))?;
            return Ok(entry.flow.clone());
        }
        bail!("flow '{flow_id}' not available (pack exports disabled)")
    }

    /// Get pack metadata.
    pub fn metadata(&self) -> &PackMetadata { &self.metadata }

    /// Read an asset file.
    pub fn read_asset(&self, asset_path: &str) -> Result<Vec<u8>> {
        let normalized = asset_path.trim_start_matches("assets/").trim_start_matches("/assets/");
        if let Some(tempdir) = &self.assets_tempdir {
            let full = tempdir.path().join("assets").join(normalized);
            if full.exists() {
                return std::fs::read(&full).with_context(|| format!("read asset {}", full.display()));
            }
        }
        let full = self.path.join("assets").join(normalized);
        if full.exists() {
            return std::fs::read(&full).with_context(|| format!("read asset {}", full.display()));
        }
        bail!("asset not found: {}", asset_path)
    }

    /// Load an i18n catalog.
    pub fn load_i18n_catalog(&self, locale: &str) -> Result<Option<I18nCatalog>> {
        let path = format!("i18n/{}.json", locale);
        if let Ok(bytes) = self.read_asset(&path) {
            let json: Value = serde_json::from_slice(&bytes).with_context(|| format!("parse i18n bundle: {}", path))?;
            return Ok(Some(I18nCatalog::from_json(locale, &json).map_err(|e| anyhow!("invalid i18n bundle {}: {}", path, e))?));
        }
        let lang = locale.split('-').next().unwrap_or(locale);
        if lang != locale {
            let lang_path = format!("i18n/{}.json", lang);
            if let Ok(bytes) = self.read_asset(&lang_path) {
                let json: Value = serde_json::from_slice(&bytes).with_context(|| format!("parse i18n bundle: {}", lang_path))?;
                return Ok(Some(I18nCatalog::from_json(lang, &json).map_err(|e| anyhow!("invalid i18n bundle {}: {}", lang_path, e))?));
            }
        }
        Ok(None)
    }

    /// Get required secrets.
    pub fn required_secrets(&self) -> &[greentic_types::SecretRequirement] {
        &self.metadata.secret_requirements
    }

    /// Get missing secrets for a tenant context.
    pub fn missing_secrets(&self, tenant_ctx: &TypesTenantCtx) -> Vec<greentic_types::SecretRequirement> {
        let env = tenant_ctx.env.as_str().to_string();
        let tenant = tenant_ctx.tenant.as_str().to_string();
        let team = tenant_ctx.team.as_ref().map(|t| t.as_str().to_string());
        self.required_secrets().iter().filter(|req| {
            if let Some(scope) = &req.scope {
                if scope.env != env || scope.tenant != tenant { return false; }
                if let Some(ref team_req) = scope.team { if team.as_ref() != Some(team_req) { return false; } }
            }
            let ctx = self.config.tenant_ctx();
            read_secret_blocking(&self.secrets, &ctx, &self.metadata.pack_id, req.key.as_str()).is_err()
        }).cloned().collect()
    }

    /// Resolve a provider binding.
    pub fn resolve_provider(&self, provider_id: Option<&str>, provider_type: Option<&str>) -> Result<ProviderBinding> {
        let registry = self.provider_registry()?;
        registry.resolve(provider_id, provider_type)
    }

    pub(crate) fn provider_registry(&self) -> Result<ProviderRegistry> {
        if let Some(registry) = self.provider_registry.read().clone() { return Ok(registry); }
        let manifest = self.manifest.as_ref().context("pack manifest required for provider resolution")?;
        let env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
        let registry = ProviderRegistry::new(manifest, self.state_store.clone(), &self.config.tenant, &env)?;
        *self.provider_registry.write() = Some(registry.clone());
        Ok(registry)
    }

    pub(crate) fn provider_registry_optional(&self) -> Result<Option<ProviderRegistry>> {
        if self.manifest.is_none() { return Ok(None); }
        Ok(Some(self.provider_registry()?))
    }

    /// Invoke a provider.
    pub async fn invoke_provider(&self, binding: &ProviderBinding, ctx: ComponentExecCtx, op: &str, input_json: Vec<u8>) -> Result<Value> {
        let component_ref_owned = binding.component_ref.clone();
        let pack_component = self.components.get(&component_ref_owned)
            .with_context(|| format!("provider component '{component_ref_owned}' not found in pack"))?;
        let component = pack_component.component.clone();
        let engine = self.engine.clone();
        let config = Arc::clone(&self.config);
        let http_client = Arc::clone(&self.http_client);
        let mocks = self.mocks.clone();
        let session_store = self.session_store.clone();
        let state_store = self.state_store.clone();
        let secrets = Arc::clone(&self.secrets);
        let oauth_config = self.oauth_config.clone();
        let wasi_policy = Arc::clone(&self.wasi_policy);
        let pack_id = self.metadata.pack_id.clone();
        let allow_state_store = self.allows_state_store(&component_ref_owned);
        let op_owned = op.to_string();
        let world = binding.world.clone();

        run_on_wasi_thread("provider.invoke", move || {
            let mut linker = Linker::new(&engine);
            register_all(&mut linker, allow_state_store)?;
            add_component_control_to_linker(&mut linker)?;
            let mut pre_instance = Some(linker.instantiate_pre(component.as_ref())?);
            let host_state = HostState::new(pack_id, config, http_client, mocks, session_store, state_store, secrets, oauth_config, Some(ctx), Some(component_ref_owned), true)?;
            let store_state = ComponentState::new(host_state, wasi_policy)?;
            let mut store = wasmtime::Store::new(&engine, store_state);
            let use_schema_core = world.contains("provider-schema-core") || world.contains("provider/schema-core");
            let result = if use_schema_core {
                let pre_instance = pre_instance.take().ok_or_else(|| anyhow!("provider pre_instance already consumed"))?;
                let pre: SchemaSchemaCorePre<ComponentState> = SchemaSchemaCorePre::new(pre_instance)?;
                let bindings = block_on(async { pre.instantiate_async(&mut store).await })?;
                bindings.greentic_provider_schema_core_schema_core_api().call_invoke(&mut store, &op_owned, &input_json)?
            } else {
                let pre_instance = pre_instance.take().ok_or_else(|| anyhow!("provider pre_instance already consumed"))?;
                let pre: LegacySchemaCorePre<ComponentState> = LegacySchemaCorePre::new(pre_instance)?;
                let bindings = block_on(async { pre.instantiate_async(&mut store).await })?;
                bindings.greentic_provider_core_schema_core_api().call_invoke(&mut store, &op_owned, &input_json)?
            };
            deserialize_json_bytes(result)
        })
    }

    /// Get component manifest.
    pub fn component_manifest(&self, component_ref: &str) -> Option<&ComponentManifest> {
        self.component_manifests.get(component_ref)
    }

    /// Describe component contract v0.6.
    pub fn describe_component_contract_v0_6(&self, component_ref: &str) -> Result<Option<Value>> {
        let pack_component = self.components.get(component_ref)
            .with_context(|| format!("component '{component_ref}' not found in pack"))?;
        let engine = self.engine.clone();
        let config = Arc::clone(&self.config);
        let http_client = Arc::clone(&self.http_client);
        let mocks = self.mocks.clone();
        let session_store = self.session_store.clone();
        let state_store = self.state_store.clone();
        let secrets = Arc::clone(&self.secrets);
        let oauth_config = self.oauth_config.clone();
        let wasi_policy = Arc::clone(&self.wasi_policy);
        let pack_id = self.metadata.pack_id.clone();
        let allow_state_store = self.allows_state_store(component_ref);
        let component = pack_component.component.clone();
        let component_ref_owned = component_ref.to_string();

        run_on_wasi_thread("component.describe", move || {
            let mut linker = Linker::new(&engine);
            register_all(&mut linker, allow_state_store)?;
            add_component_control_to_linker(&mut linker)?;
            let host_state = HostState::new(pack_id, config, http_client, mocks, session_store, state_store, secrets, oauth_config, None, Some(component_ref_owned), false)?;
            let store_state = ComponentState::new(host_state, wasi_policy)?;
            let mut store = wasmtime::Store::new(&engine, store_state);
            let pre_instance = linker.instantiate_pre(&component)?;
            let pre = match crate::component_api::v0_6_descriptor::ComponentV0V6V0Pre::new(pre_instance) {
                Ok(pre) => pre,
                Err(_) => return Ok(None),
            };
            let bytes = block_on(async {
                let bindings = pre.instantiate_async(&mut store).await?;
                let descriptor = bindings.greentic_component_component_descriptor();
                descriptor.call_describe(&mut store)
            })?;
            if bytes.is_empty() { return Ok(Some(Value::Null)); }
            if let Ok(value) = serde_cbor::from_slice::<Value>(&bytes) { return Ok(Some(value)); }
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) { return Ok(Some(value)); }
            if let Ok(text) = String::from_utf8(bytes) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) { return Ok(Some(value)); }
                return Ok(Some(Value::String(text)));
            }
            Ok(Some(Value::Null))
        })
    }

    /// Load a schema JSON file from the pack.
    pub fn load_schema_json(&self, schema_ref: &str) -> Result<Option<Value>> {
        let rel = normalize_schema_ref(schema_ref)?;
        if self.path.is_dir() {
            let candidate = self.path.join(&rel);
            if candidate.exists() {
                let bytes = std::fs::read(&candidate)
                    .with_context(|| format!("failed to read schema file {}", candidate.display()))?;
                let value = serde_json::from_slice::<Value>(&bytes)
                    .with_context(|| format!("invalid schema JSON in {}", candidate.display()))?;
                return Ok(Some(value));
            }
        }
        if let Some(archive_path) = self.archive_path.as_ref().or_else(|| path_is_gtpack(&self.path).then_some(&self.path)) {
            let file = File::open(archive_path)
                .with_context(|| format!("failed to open {}", archive_path.display()))?;
            let mut archive = ZipArchive::new(file)
                .with_context(|| format!("failed to read pack {}", archive_path.display()))?;
            match archive.by_name(&rel) {
                Ok(mut entry) => {
                    let mut bytes = Vec::new();
                    entry.read_to_end(&mut bytes)?;
                    let value = serde_json::from_slice::<Value>(&bytes)
                        .with_context(|| format!("invalid schema JSON in {}:{}", archive_path.display(), rel))?;
                    Ok(Some(value))
                }
                Err(zip::result::ZipError::FileNotFound) => Ok(None),
                Err(err) => Err(anyhow!(err))
                    .with_context(|| format!("failed to read schema `{}` from {}", rel, archive_path.display())),
            }
        } else {
            Ok(None)
        }
    }
}
