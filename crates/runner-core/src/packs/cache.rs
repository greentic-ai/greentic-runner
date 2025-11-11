use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use super::{PackDigest, PackEntry, PackRef, PackVersion};

pub struct PackCache {
    root: PathBuf,
}

impl PackCache {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self, entry: &PackEntry, source: &Path, digest: &PackDigest) -> Result<PathBuf> {
        let dest_dir = self.dir_for(&entry.reference);
        fs::create_dir_all(&dest_dir)
            .with_context(|| format!("failed to create cache dir {}", dest_dir.display()))?;
        let dest_path = dest_dir.join("pack.gtpack");
        if dest_path.exists() {
            if digest.matches_file(&dest_path)? {
                return Ok(dest_path);
            }
            fs::remove_file(&dest_path)
                .with_context(|| format!("failed to remove stale cache {}", dest_path.display()))?;
        }

        if same_path(source, &dest_path) {
            return Ok(dest_path);
        }

        copy_atomic(source, &dest_path)?;
        Ok(dest_path)
    }

    fn dir_for(&self, reference: &PackRef) -> PathBuf {
        match &reference.version {
            PackVersion::Semver(version) => self
                .root
                .join(super::sanitize_segment(&reference.name))
                .join(version.to_string()),
            PackVersion::Digest(digest) => self
                .root
                .join(super::sanitize_segment(&reference.name))
                .join(digest.cache_label()),
        }
    }
}

fn copy_atomic(source: &Path, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension(format!(
        "tmp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|dur| dur.as_nanos())
            .unwrap_or(0)
    ));
    fs::copy(source, &tmp)
        .with_context(|| format!("failed to copy {} -> {}", source.display(), dest.display()))?;
    fs::rename(&tmp, dest)
        .with_context(|| format!("failed to place cached pack at {}", dest.display()))?;
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        a == b
    } else {
        a == b
    }
}
