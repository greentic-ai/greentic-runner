//! Component resolution and pack lock handling.

use super::helpers::normalize_sha256;
use anyhow::{Context, Result, anyhow, bail};
use greentic_pack::builder as legacy_pack;
use greentic_types::{ArtifactLocationV1, ComponentId, ComponentSourceRef, ComponentSourcesV1};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Configuration for component resolution.
#[derive(Debug, Default, Clone)]
pub struct ComponentResolution {
    /// Root of a materialized pack directory.
    pub materialized_root: Option<PathBuf>,
    /// Explicit overrides mapping component id -> wasm path.
    pub overrides: HashMap<String, PathBuf>,
    /// If true, do not fetch remote components; require cached artifacts.
    pub dist_offline: bool,
    /// Optional cache directory for resolved remote components.
    pub dist_cache_dir: Option<PathBuf>,
    /// Allow bundled components without wasm_sha256 (dev-only escape hatch).
    pub allow_missing_hash: bool,
}

/// Component specification from manifest.
#[derive(Clone, Debug)]
pub(crate) struct ComponentSpec {
    pub id: String,
    pub version: String,
    pub legacy_path: Option<String>,
}

/// Information about a component source.
#[derive(Clone, Debug)]
pub(crate) struct ComponentSourceInfo {
    pub digest: Option<String>,
    pub source: ComponentSourceRef,
    pub artifact: ComponentArtifactLocation,
    pub expected_wasm_sha256: Option<String>,
    pub skip_digest_verification: bool,
}

/// Location of a component artifact.
#[derive(Clone, Debug)]
pub(crate) enum ComponentArtifactLocation {
    Inline { wasm_path: String },
    Remote,
}

/// Pack lock file schema v1.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PackLockV1 {
    pub schema_version: u32,
    pub components: Vec<PackLockComponent>,
}

/// Component entry in pack lock.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct PackLockComponent {
    pub name: String,
    #[serde(default, rename = "source_ref")]
    pub source_ref: Option<String>,
    #[serde(default, rename = "ref")]
    pub legacy_ref: Option<String>,
    #[serde(default)]
    pub component_id: Option<ComponentId>,
    #[serde(default)]
    pub bundled: Option<bool>,
    #[serde(default, rename = "bundled_path")]
    pub bundled_path: Option<String>,
    #[serde(default, rename = "path")]
    pub legacy_path: Option<String>,
    #[serde(default)]
    pub wasm_sha256: Option<String>,
    #[serde(default, rename = "sha256")]
    pub legacy_sha256: Option<String>,
    #[serde(default)]
    pub resolved_digest: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
}

/// Build component specs from various manifest sources.
pub(crate) fn component_specs(
    manifest: Option<&greentic_types::PackManifest>,
    legacy_manifest: Option<&legacy_pack::PackManifest>,
    component_sources: Option<&ComponentSourcesV1>,
    pack_lock: Option<&PackLockV1>,
) -> Vec<ComponentSpec> {
    if let Some(manifest) = manifest {
        if !manifest.components.is_empty() {
            return manifest
                .components
                .iter()
                .map(|entry| ComponentSpec {
                    id: entry.id.as_str().to_string(),
                    version: entry.version.to_string(),
                    legacy_path: None,
                })
                .collect();
        }
        if let Some(lock) = pack_lock {
            let mut seen = HashSet::new();
            let mut specs = Vec::new();
            for entry in &lock.components {
                let id = entry
                    .component_id
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or(entry.name.as_str());
                if seen.insert(id.to_string()) {
                    specs.push(ComponentSpec {
                        id: id.to_string(),
                        version: "0.0.0".to_string(),
                        legacy_path: None,
                    });
                }
            }
            return specs;
        }
        if let Some(sources) = component_sources {
            let mut seen = HashSet::new();
            let mut specs = Vec::new();
            for entry in &sources.components {
                let id = entry
                    .component_id
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or(entry.name.as_str());
                if seen.insert(id.to_string()) {
                    specs.push(ComponentSpec {
                        id: id.to_string(),
                        version: "0.0.0".to_string(),
                        legacy_path: None,
                    });
                }
            }
            return specs;
        }
    }
    if let Some(legacy_manifest) = legacy_manifest {
        return legacy_manifest
            .components
            .iter()
            .map(|entry| ComponentSpec {
                id: entry.name.clone(),
                version: entry.version.to_string(),
                legacy_path: Some(entry.file_wasm.clone()),
            })
            .collect();
    }
    Vec::new()
}

/// Build component sources table from manifest extension.
pub(crate) fn component_sources_table(
    sources: Option<&ComponentSourcesV1>,
) -> Result<Option<HashMap<String, ComponentSourceInfo>>> {
    let Some(sources) = sources else {
        return Ok(None);
    };
    let mut table = HashMap::new();
    for entry in &sources.components {
        let artifact = match &entry.artifact {
            ArtifactLocationV1::Inline { wasm_path, .. } => ComponentArtifactLocation::Inline {
                wasm_path: wasm_path.clone(),
            },
            ArtifactLocationV1::Remote => ComponentArtifactLocation::Remote,
        };
        let info = ComponentSourceInfo {
            digest: Some(entry.resolved.digest.clone()),
            source: entry.source.clone(),
            artifact,
            expected_wasm_sha256: None,
            skip_digest_verification: false,
        };
        if let Some(component_id) = entry.component_id.as_ref() {
            table.insert(component_id.as_str().to_string(), info.clone());
        }
        table.insert(entry.name.clone(), info);
    }
    Ok(Some(table))
}

/// Load pack lock from a directory.
pub(crate) fn load_pack_lock(path: &Path) -> Result<Option<PackLockV1>> {
    let lock_path = if path.is_dir() {
        let candidate = path.join("pack.lock");
        if candidate.exists() {
            Some(candidate)
        } else {
            let candidate = path.join("pack.lock.json");
            candidate.exists().then_some(candidate)
        }
    } else {
        None
    };
    let Some(lock_path) = lock_path else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
    let lock: PackLockV1 = serde_json::from_str(&raw).context("failed to parse pack.lock")?;
    if lock.schema_version != 1 {
        bail!("pack.lock schema_version must be 1");
    }
    Ok(Some(lock))
}

/// Find potential pack lock root directories.
pub(crate) fn find_pack_lock_roots(
    pack_path: &Path,
    is_dir: bool,
    archive_hint: Option<&Path>,
) -> Vec<PathBuf> {
    if is_dir {
        return vec![pack_path.to_path_buf()];
    }
    let mut roots = Vec::new();
    if let Some(archive_path) = archive_hint {
        if let Some(parent) = archive_path.parent() {
            roots.push(parent.to_path_buf());
            if let Some(grandparent) = parent.parent() {
                roots.push(grandparent.to_path_buf());
            }
        }
    } else if let Some(parent) = pack_path.parent() {
        roots.push(parent.to_path_buf());
        if let Some(grandparent) = parent.parent() {
            roots.push(grandparent.to_path_buf());
        }
    }
    roots
}

/// Build component sources table from pack lock.
pub(crate) fn component_sources_table_from_pack_lock(
    lock: &PackLockV1,
    allow_missing_hash: bool,
) -> Result<HashMap<String, ComponentSourceInfo>> {
    let mut table = HashMap::new();
    let mut names = HashSet::new();
    for entry in &lock.components {
        if !names.insert(entry.name.clone()) {
            bail!(
                "pack.lock contains duplicate component name `{}`",
                entry.name
            );
        }
        let source_ref = match (&entry.source_ref, &entry.legacy_ref) {
            (Some(primary), Some(legacy)) => {
                if primary != legacy {
                    bail!(
                        "pack.lock component {} has conflicting refs: {} vs {}",
                        entry.name,
                        primary,
                        legacy
                    );
                }
                primary.as_str()
            }
            (Some(primary), None) => primary.as_str(),
            (None, Some(legacy)) => legacy.as_str(),
            (None, None) => {
                bail!("pack.lock component {} missing source_ref", entry.name);
            }
        };
        let source: ComponentSourceRef = source_ref
            .parse()
            .with_context(|| format!("invalid component ref `{}`", source_ref))?;
        let bundled_path = match (&entry.bundled_path, &entry.legacy_path) {
            (Some(primary), Some(legacy)) => {
                if primary != legacy {
                    bail!(
                        "pack.lock component {} has conflicting bundled paths: {} vs {}",
                        entry.name,
                        primary,
                        legacy
                    );
                }
                Some(primary.clone())
            }
            (Some(primary), None) => Some(primary.clone()),
            (None, Some(legacy)) => Some(legacy.clone()),
            (None, None) => None,
        };
        let bundled = entry.bundled.unwrap_or(false) || bundled_path.is_some();
        let (artifact, digest, expected_wasm_sha256, skip_digest_verification) = if bundled {
            let wasm_path = bundled_path.ok_or_else(|| {
                anyhow!(
                    "pack.lock component {} marked bundled but bundled_path is missing",
                    entry.name
                )
            })?;
            let expected_raw = match (&entry.wasm_sha256, &entry.legacy_sha256) {
                (Some(primary), Some(legacy)) => {
                    if primary != legacy {
                        bail!(
                            "pack.lock component {} has conflicting wasm_sha256 values: {} vs {}",
                            entry.name,
                            primary,
                            legacy
                        );
                    }
                    Some(primary.as_str())
                }
                (Some(primary), None) => Some(primary.as_str()),
                (None, Some(legacy)) => Some(legacy.as_str()),
                (None, None) => None,
            };
            let expected = match expected_raw {
                Some(value) => Some(normalize_sha256(value)?),
                None => None,
            };
            if expected.is_none() && !allow_missing_hash {
                bail!(
                    "pack.lock component {} missing wasm_sha256 for bundled component",
                    entry.name
                );
            }
            (
                ComponentArtifactLocation::Inline { wasm_path },
                expected.clone(),
                expected,
                allow_missing_hash && expected_raw.is_none(),
            )
        } else {
            if source.is_tag() {
                bail!(
                    "component {} uses tag ref {} but is not bundled; rebuild the pack",
                    entry.name,
                    source
                );
            }
            let expected = entry
                .resolved_digest
                .as_deref()
                .or(entry.digest.as_deref())
                .ok_or_else(|| {
                    anyhow!(
                        "pack.lock component {} missing resolved_digest for remote component",
                        entry.name
                    )
                })?;
            (
                ComponentArtifactLocation::Remote,
                Some(super::helpers::normalize_digest(expected)),
                None,
                false,
            )
        };
        let info = ComponentSourceInfo {
            digest,
            source,
            artifact,
            expected_wasm_sha256,
            skip_digest_verification,
        };
        if let Some(component_id) = entry.component_id.as_ref() {
            let key = component_id.as_str().to_string();
            if table.contains_key(&key) {
                bail!(
                    "pack.lock contains duplicate component id `{}`",
                    component_id.as_str()
                );
            }
            table.insert(key, info.clone());
        }
        if entry.name
            != entry
                .component_id
                .as_ref()
                .map(|id| id.as_str())
                .unwrap_or("")
        {
            table.insert(entry.name.clone(), info);
        }
    }
    Ok(table)
}

/// Get the component path for a spec.
pub(crate) fn component_path_for_spec(root: &Path, spec: &ComponentSpec) -> PathBuf {
    if let Some(path) = &spec.legacy_path {
        return root.join(path);
    }
    root.join("components").join(format!("{}.wasm", spec.id))
}
