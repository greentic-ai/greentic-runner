//! Artifact resolution utilities that locate components and compute their digests.

use std::fs;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::ResolveError;
use crate::store::{self, ToolInfo, ToolStore};

#[derive(Clone, Debug)]
pub struct ResolvedArtifact {
    #[allow(dead_code)]
    pub info: ToolInfo,
    pub bytes: Arc<[u8]>,
    pub digest: String,
}

pub fn resolve(component: &str, store_ref: &ToolStore) -> Result<ResolvedArtifact, ResolveError> {
    let info = match store_ref.fetch(component) {
        Ok(info) => info,
        Err(err) if store::is_not_found(&err) => return Err(ResolveError::NotFound),
        Err(err) => return Err(ResolveError::Store(err)),
    };

    let bytes = fs::read(&info.path).map_err(ResolveError::Io)?;
    let digest = info
        .sha256
        .clone()
        .unwrap_or_else(|| compute_digest(&bytes));

    Ok(ResolvedArtifact {
        info,
        bytes: Arc::from(bytes),
        digest,
    })
}

fn compute_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn resolves_exact_component_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wasm_path = tmp.path().join("tool.wasm");
        std::fs::write(&wasm_path, b"payload").expect("write wasm");

        let store = ToolStore::LocalDir(PathBuf::from(tmp.path()));
        let artifact = resolve("tool", &store).expect("resolve");

        assert_eq!(artifact.info.name, "tool");
        assert_eq!(artifact.info.path, wasm_path);
        assert_eq!(artifact.digest, compute_digest(b"payload"));
    }

    #[test]
    fn fails_when_component_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = ToolStore::LocalDir(PathBuf::from(tmp.path()));

        let err = resolve("missing", &store).expect_err("should fail");
        assert!(matches!(err, ResolveError::NotFound));
    }
}
