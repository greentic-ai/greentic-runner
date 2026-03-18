//! Utility functions for pack operations.

use anyhow::{Context, Result, anyhow, bail};
use once_cell::sync::Lazy;
use reqwest::blocking::Client as BlockingClient;
use runner_core::normalize_under_root;
use serde_json::Value;
use sha2::Digest;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use zip::ZipArchive;

/// Global HTTP client shared across pack operations.
pub static HTTP_CLIENT: Lazy<Arc<BlockingClient>> = Lazy::new(|| Arc::new(build_blocking_client()));

fn build_blocking_client() -> BlockingClient {
    std::thread::spawn(|| {
        BlockingClient::builder()
            .no_proxy()
            .build()
            .expect("blocking client")
    })
    .join()
    .expect("client build thread panicked")
}

/// Run a task on a dedicated thread for WASM operations.
pub fn run_on_wasi_thread<F, T>(task_name: &'static str, task: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let builder = std::thread::Builder::new().name(format!("greentic-wasmtime-{task_name}"));
    let handle = builder
        .spawn(move || {
            let pid = std::process::id();
            let thread_id = std::thread::current().id();
            let tokio_handle_present = tokio::runtime::Handle::try_current().is_ok();
            tracing::info!(
                event = "wasmtime.thread.start",
                task = task_name,
                pid,
                thread_id = ?thread_id,
                tokio_handle_present,
                "starting Wasmtime thread"
            );
            task()
        })
        .context("failed to spawn Wasmtime thread")?;
    handle
        .join()
        .map_err(|err| {
            let reason = if let Some(msg) = err.downcast_ref::<&str>() {
                msg.to_string()
            } else if let Some(msg) = err.downcast_ref::<String>() {
                msg.clone()
            } else {
                "unknown panic".to_string()
            };
            anyhow!("Wasmtime thread panicked: {reason}")
        })
        .and_then(|res| res)
}

/// Normalize a pack path to absolute form.
pub fn normalize_pack_path(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let (root, candidate) = if path.is_absolute() {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("pack path {} has no parent", path.display()))?;
        let root = parent
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", parent.display()))?;
        let file = path
            .file_name()
            .ok_or_else(|| anyhow!("pack path {} has no file name", path.display()))?;
        (root, PathBuf::from(file))
    } else {
        let cwd = std::env::current_dir().context("failed to resolve current directory")?;
        let base = if let Some(parent) = path.parent() {
            cwd.join(parent)
        } else {
            cwd
        };
        let root = base
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", base.display()))?;
        let file = path
            .file_name()
            .ok_or_else(|| anyhow!("pack path {} has no file name", path.display()))?;
        (root, PathBuf::from(file))
    };
    let safe = normalize_under_root(&root, &candidate)?;
    Ok((root, safe))
}

/// Check if path has .gtpack extension.
pub fn path_is_gtpack(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("gtpack"))
        .unwrap_or(false)
}

/// Normalize a schema reference path.
pub fn normalize_schema_ref(schema_ref: &str) -> Result<String> {
    let candidate = schema_ref.trim();
    if candidate.is_empty() {
        bail!("schema ref cannot be empty");
    }
    let path = Path::new(candidate);
    if path.is_absolute() {
        bail!("schema ref must be relative: {}", schema_ref);
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::CurDir => {}
            _ => bail!("schema ref must not contain traversal: {}", schema_ref),
        }
    }
    let normalized = normalized
        .to_str()
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("schema ref must be valid UTF-8"))?;
    if normalized.is_empty() {
        bail!("schema ref cannot normalize to empty path");
    }
    Ok(normalized)
}

/// Read an entry from a ZIP archive.
pub fn read_zip_entry(archive: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut file = archive
        .by_name(name)
        .with_context(|| format!("entry {name} missing from archive"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Deserialize JSON bytes to a Value.
pub fn deserialize_json_bytes(bytes: Vec<u8>) -> Result<Value> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).or_else(|_| {
        String::from_utf8(bytes)
            .map(Value::String)
            .map_err(|err| anyhow!(err))
    })
}

/// Normalize a digest to include the algorithm prefix.
pub fn normalize_digest(digest: &str) -> String {
    if digest.starts_with("sha256:") || digest.starts_with("blake3:") {
        digest.to_string()
    } else {
        format!("sha256:{digest}")
    }
}

/// Normalize a SHA256 digest.
pub fn normalize_sha256(digest: &str) -> Result<String> {
    let trimmed = digest.trim();
    if trimmed.is_empty() {
        bail!("sha256 digest cannot be empty");
    }
    if let Some(stripped) = trimmed.strip_prefix("sha256:") {
        if stripped.is_empty() {
            bail!("sha256 digest must include hex bytes after sha256:");
        }
        return Ok(trimmed.to_string());
    }
    if trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(format!("sha256:{trimmed}"));
    }
    bail!("sha256 digest must be hex or sha256:<hex>");
}

/// Compute digest for bytes using the algorithm indicated by the expected digest.
pub fn compute_digest_for(bytes: &[u8], digest: &str) -> Result<String> {
    if digest.starts_with("blake3:") {
        let hash = blake3::hash(bytes);
        return Ok(format!("blake3:{}", hash.to_hex()));
    }
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

/// Compute SHA256 digest for bytes.
pub fn compute_sha256_digest_for(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

/// Verify component digest matches expected.
pub fn verify_component_digest(component_id: &str, expected: &str, bytes: &[u8]) -> Result<()> {
    let normalized_expected = normalize_digest(expected);
    let actual = compute_digest_for(bytes, &normalized_expected)?;
    if normalize_digest(&actual) != normalized_expected {
        bail!(
            "component {component_id} digest mismatch: expected {normalized_expected}, got {actual}"
        );
    }
    Ok(())
}

/// Verify WASM SHA256 matches expected.
pub fn verify_wasm_sha256(component_id: &str, expected: &str, bytes: &[u8]) -> Result<()> {
    let normalized_expected = normalize_sha256(expected)?;
    let actual = compute_sha256_digest_for(bytes);
    if actual != normalized_expected {
        bail!(
            "component {component_id} bundled digest mismatch: expected {normalized_expected}, got {actual}"
        );
    }
    Ok(())
}

/// Locate pack assets directory from materialized root or archive.
pub fn locate_pack_assets(
    materialized_root: Option<&Path>,
    archive_hint: Option<&Path>,
) -> Result<(Option<PathBuf>, Option<TempDir>)> {
    if let Some(root) = materialized_root {
        let assets = root.join("assets");
        if assets.is_dir() {
            return Ok((Some(assets), None));
        }
    }
    if let Some(path) = archive_hint
        && let Some((tempdir, assets)) = extract_assets_from_archive(path)?
    {
        return Ok((Some(assets), Some(tempdir)));
    }
    Ok((None, None))
}

/// Extract assets from a pack archive to a temp directory.
pub fn extract_assets_from_archive(path: &Path) -> Result<Option<(TempDir, PathBuf)>> {
    let file =
        File::open(path).with_context(|| format!("failed to open pack {}", path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("failed to read pack {}", path.display()))?;
    let temp = TempDir::new().context("failed to create temporary assets directory")?;
    let mut found = false;
    for idx in 0..archive.len() {
        let mut entry = archive.by_index(idx)?;
        let name = entry.name();
        if !name.starts_with("assets/") {
            continue;
        }
        let dest = temp.path().join(name);
        if name.ends_with('/') {
            std::fs::create_dir_all(&dest)?;
            found = true;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outfile = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut outfile)?;
        found = true;
    }
    if found {
        let assets_path = temp.path().join("assets");
        Ok(Some((temp, assets_path)))
    } else {
        Ok(None)
    }
}

/// Canonicalize a WASM secret key to lowercase snake_case.
pub fn canonicalize_wasm_secret_key(raw: &str) -> String {
    raw.trim()
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            match ch {
                'a'..='z' | '0'..='9' | '_' => ch,
                _ => '_',
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_snake_to_lower_snake() {
        assert_eq!(
            canonicalize_wasm_secret_key("TELEGRAM_BOT_TOKEN"),
            "telegram_bot_token"
        );
    }

    #[test]
    fn trim_and_replace_non_alphanumeric() {
        assert_eq!(
            canonicalize_wasm_secret_key("  webex-bot-token  "),
            "webex_bot_token"
        );
    }

    #[test]
    fn preserve_existing_lower_snake_with_extra_underscores() {
        assert_eq!(canonicalize_wasm_secret_key("MiXeD__Case"), "mixed__case");
    }
}
