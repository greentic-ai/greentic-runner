//! Component loading functions from various sources.

use super::helpers::{
    compute_sha256_digest_for, read_zip_entry, verify_component_digest, verify_wasm_sha256,
};
use super::resolution::{
    ComponentArtifactLocation, ComponentResolution, ComponentSourceInfo, ComponentSpec,
    component_path_for_spec,
};
use crate::cache::{ArtifactKey, CacheManager};
use crate::fault;
use crate::runtime_wasmtime::{Component, Engine};
use anyhow::{Context, Result, anyhow, bail};
use greentic_distributor_client::dist::{DistClient, DistError, DistOptions};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;
use zip::ZipArchive;

/// Compiled pack component.
pub(crate) struct PackComponent {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    pub component: Arc<Component>,
}

/// Build an artifact key for caching.
fn build_artifact_key(cache: &CacheManager, digest: Option<&str>, bytes: &[u8]) -> ArtifactKey {
    let wasm_digest = digest
        .map(super::helpers::normalize_digest)
        .unwrap_or_else(|| compute_sha256_digest_for(bytes));
    ArtifactKey::new(cache.engine_profile_id().to_string(), wasm_digest)
}

/// Compile a component with caching.
pub(crate) async fn compile_component_with_cache(
    cache: &CacheManager,
    engine: &Engine,
    digest: Option<&str>,
    bytes: Vec<u8>,
) -> Result<Arc<Component>> {
    let key = build_artifact_key(cache, digest, &bytes);
    cache.get_component(engine, &key, || Ok(bytes)).await
}

/// Load components from explicit overrides.
pub(crate) async fn load_components_from_overrides(
    cache: &CacheManager,
    engine: &Engine,
    overrides: &HashMap<String, PathBuf>,
    specs: &[ComponentSpec],
    missing: &mut HashSet<String>,
    into: &mut HashMap<String, PackComponent>,
) -> Result<()> {
    for spec in specs {
        if !missing.contains(&spec.id) {
            continue;
        }
        let Some(path) = overrides.get(&spec.id) else {
            continue;
        };
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read override component {}", path.display()))?;
        let component = compile_component_with_cache(cache, engine, None, bytes)
            .await
            .with_context(|| {
                format!(
                    "failed to compile component {} from override {}",
                    spec.id,
                    path.display()
                )
            })?;
        into.insert(
            spec.id.clone(),
            PackComponent {
                name: spec.id.clone(),
                version: spec.version.clone(),
                component,
            },
        );
        missing.remove(&spec.id);
    }
    Ok(())
}

/// Load components from a materialized directory.
pub(crate) async fn load_components_from_dir(
    cache: &CacheManager,
    engine: &Engine,
    root: &Path,
    specs: &[ComponentSpec],
    missing: &mut HashSet<String>,
    into: &mut HashMap<String, PackComponent>,
) -> Result<()> {
    for spec in specs {
        if !missing.contains(&spec.id) {
            continue;
        }
        let path = component_path_for_spec(root, spec);
        if !path.exists() {
            tracing::debug!(
                component = %spec.id,
                path = %path.display(),
                "materialized component missing; will try other sources"
            );
            continue;
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read component {}", path.display()))?;
        let component = compile_component_with_cache(cache, engine, None, bytes)
            .await
            .with_context(|| {
                format!(
                    "failed to compile component {} from {}",
                    spec.id,
                    path.display()
                )
            })?;
        into.insert(
            spec.id.clone(),
            PackComponent {
                name: spec.id.clone(),
                version: spec.version.clone(),
                component,
            },
        );
        missing.remove(&spec.id);
    }
    Ok(())
}

/// Load components from a pack archive.
pub(crate) async fn load_components_from_archive(
    cache: &CacheManager,
    engine: &Engine,
    path: &Path,
    specs: &[ComponentSpec],
    missing: &mut HashSet<String>,
    into: &mut HashMap<String, PackComponent>,
) -> Result<()> {
    let mut archive = ZipArchive::new(File::open(path)?)
        .with_context(|| format!("{} is not a valid gtpack", path.display()))?;
    for spec in specs {
        if !missing.contains(&spec.id) {
            continue;
        }
        let file_name = spec
            .legacy_path
            .clone()
            .unwrap_or_else(|| format!("components/{}.wasm", spec.id));
        let bytes = match read_zip_entry(&mut archive, &file_name) {
            Ok(bytes) => bytes,
            Err(err) => {
                warn!(
                    component = %spec.id,
                    pack = %path.display(),
                    error = %err,
                    "component entry missing in pack archive"
                );
                continue;
            }
        };
        let component = compile_component_with_cache(cache, engine, None, bytes)
            .await
            .with_context(|| format!("failed to compile component {}", spec.id))?;
        into.insert(
            spec.id.clone(),
            PackComponent {
                name: spec.id.clone(),
                version: spec.version.clone(),
                component,
            },
        );
        missing.remove(&spec.id);
    }
    Ok(())
}

fn dist_options_from(component_resolution: &ComponentResolution) -> DistOptions {
    let mut opts = DistOptions {
        allow_tags: true,
        ..DistOptions::default()
    };
    if let Some(cache_dir) = component_resolution.dist_cache_dir.clone() {
        opts.cache_dir = cache_dir;
    }
    if component_resolution.dist_offline {
        opts.offline = true;
    }
    opts
}

/// Load components from component sources table.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_components_from_sources(
    cache: &CacheManager,
    engine: &Engine,
    component_sources: &HashMap<String, ComponentSourceInfo>,
    component_resolution: &ComponentResolution,
    specs: &[ComponentSpec],
    missing: &mut HashSet<String>,
    into: &mut HashMap<String, PackComponent>,
    materialized_root: Option<&Path>,
    archive_hint: Option<&Path>,
) -> Result<()> {
    let mut archive = if let Some(path) = archive_hint {
        Some(
            ZipArchive::new(File::open(path)?)
                .with_context(|| format!("{} is not a valid gtpack", path.display()))?,
        )
    } else {
        None
    };
    let mut dist_client: Option<DistClient> = None;

    for spec in specs {
        if !missing.contains(&spec.id) {
            continue;
        }
        let Some(source) = component_sources.get(&spec.id) else {
            continue;
        };

        let bytes = match &source.artifact {
            ComponentArtifactLocation::Inline { wasm_path } => {
                load_inline_artifact(&spec.id, wasm_path, materialized_root, archive.as_mut())?
            }
            ComponentArtifactLocation::Remote => {
                load_remote_artifact(&spec.id, source, component_resolution, &mut dist_client)
                    .await?
            }
        };

        // Verify digests
        if let Some(expected) = source.expected_wasm_sha256.as_deref() {
            verify_wasm_sha256(&spec.id, expected, &bytes)?;
        } else if source.skip_digest_verification {
            let actual = compute_sha256_digest_for(&bytes);
            warn!(
                component_id = %spec.id,
                digest = %actual,
                "bundled component missing wasm_sha256; allowing due to flag"
            );
        } else {
            let expected = source.digest.as_deref().ok_or_else(|| {
                anyhow!(
                    "component {} missing expected digest for verification",
                    spec.id
                )
            })?;
            verify_component_digest(&spec.id, expected, &bytes)?;
        }

        let component =
            compile_component_with_cache(cache, engine, source.digest.as_deref(), bytes)
                .await
                .with_context(|| format!("failed to compile component {}", spec.id))?;
        into.insert(
            spec.id.clone(),
            PackComponent {
                name: spec.id.clone(),
                version: spec.version.clone(),
                component,
            },
        );
        missing.remove(&spec.id);
    }

    Ok(())
}

fn load_inline_artifact(
    spec_id: &str,
    wasm_path: &str,
    materialized_root: Option<&Path>,
    archive: Option<&mut ZipArchive<File>>,
) -> Result<Vec<u8>> {
    if let Some(root) = materialized_root {
        let path = root.join(wasm_path);
        if path.exists() {
            return std::fs::read(&path).with_context(|| {
                format!(
                    "failed to read inline component {} from {}",
                    spec_id,
                    path.display()
                )
            });
        } else if archive.is_none() {
            bail!("inline component {} missing at {}", spec_id, path.display());
        }
    }

    if let Some(archive) = archive {
        read_zip_entry(archive, wasm_path).with_context(|| {
            format!(
                "inline component {} missing at {} in pack archive",
                spec_id, wasm_path
            )
        })
    } else {
        bail!(
            "inline component {} missing and no pack source available",
            spec_id
        )
    }
}

async fn load_remote_artifact(
    spec_id: &str,
    source: &ComponentSourceInfo,
    component_resolution: &ComponentResolution,
    dist_client: &mut Option<DistClient>,
) -> Result<Vec<u8>> {
    if source.source.is_tag() {
        bail!(
            "component {} uses tag ref {} but is not bundled; rebuild the pack",
            spec_id,
            source.source
        );
    }

    let client =
        dist_client.get_or_insert_with(|| DistClient::new(dist_options_from(component_resolution)));
    let reference = source.source.to_string();

    fault::maybe_fail_asset(&reference)
        .await
        .with_context(|| format!("fault injection blocked asset {reference}"))?;

    let digest = source.digest.as_deref().ok_or_else(|| {
        anyhow!(
            "component {} missing expected digest for remote component",
            spec_id
        )
    })?;

    let cache_path = if component_resolution.dist_offline {
        client
            .fetch_digest(digest)
            .await
            .map_err(|err| dist_error_for_component(err, spec_id, &reference))?
    } else {
        let resolved = client
            .resolve_ref(&reference)
            .await
            .map_err(|err| dist_error_for_component(err, spec_id, &reference))?;
        let expected = super::helpers::normalize_digest(digest);
        let actual = super::helpers::normalize_digest(&resolved.digest);
        if expected != actual {
            bail!(
                "component {} digest mismatch after fetch: expected {}, got {}",
                spec_id,
                expected,
                actual
            );
        }
        resolved.cache_path.ok_or_else(|| {
            anyhow!(
                "component {} resolved from {} but cache path is missing",
                spec_id,
                reference
            )
        })?
    };

    std::fs::read(&cache_path).with_context(|| {
        format!(
            "failed to read cached component {} from {}",
            spec_id,
            cache_path.display()
        )
    })
}

fn dist_error_for_component(err: DistError, component_id: &str, reference: &str) -> anyhow::Error {
    match err {
        DistError::NotFound { reference: missing } => anyhow!(
            "remote component {} is not cached for {}. Run `greentic-dist pull --lock <pack.lock>` or `greentic-dist pull {}`",
            component_id,
            missing,
            reference
        ),
        DistError::Offline { reference: blocked } => anyhow!(
            "offline mode blocked fetching component {} from {}; run `greentic-dist pull --lock <pack.lock>` or `greentic-dist pull {}`",
            component_id,
            blocked,
            reference
        ),
        DistError::Unauthorized { target } => anyhow!(
            "component {} requires authenticated source {}; run `greentic-dist pull --lock <pack.lock>` or `greentic-dist pull {}`",
            component_id,
            target,
            reference
        ),
        other => anyhow!(
            "failed to resolve component {} from {}: {}",
            component_id,
            reference,
            other
        ),
    }
}
