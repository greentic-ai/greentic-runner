use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

thread_local! {
    static CANONICAL_ROOT_CACHE: RefCell<HashMap<PathBuf, PathBuf>> = RefCell::new(HashMap::new());
}

/// Normalize a user-supplied path and ensure it stays within an allowed root.
/// Rejects absolute candidates and any that escape the root via `..`.
pub fn normalize_under_root(root: &Path, candidate: &Path) -> Result<PathBuf> {
    if candidate.is_absolute() {
        anyhow::bail!("absolute paths are not allowed: {}", candidate.display());
    }

    let root = canonicalize_cached(root)?;
    let joined = root.join(candidate);
    let canon = joined
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", joined.display()))?;

    if !canon.starts_with(&root) {
        anyhow::bail!(
            "path escapes root ({}): {}",
            root.display(),
            canon.display()
        );
    }

    Ok(canon)
}

fn canonicalize_cached(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", root.display()));
    }

    if let Some(cached) = CANONICAL_ROOT_CACHE.with(|cache| cache.borrow().get(root).cloned()) {
        return Ok(cached);
    }

    let canonical = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    if canonical == root {
        CANONICAL_ROOT_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert(root.to_path_buf(), canonical.clone());
        });
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::{CANONICAL_ROOT_CACHE, normalize_under_root};
    use anyhow::Result;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn normalizes_relative_roots_before_comparing() -> Result<()> {
        let current_dir = std::env::current_dir()?;
        let temp = tempfile::tempdir_in(&current_dir)?;
        let relative_root = temp.path().strip_prefix(&current_dir)?;
        let file_path = temp.path().join("pack.gtpack");
        fs::write(&file_path, b"pack")?;

        let normalized = normalize_under_root(relative_root, std::path::Path::new("pack.gtpack"))?;

        assert_eq!(normalized, file_path.canonicalize()?);
        Ok(())
    }

    #[test]
    fn rejects_parent_escape() -> Result<()> {
        let temp = TempDir::new()?;
        let sibling = temp
            .path()
            .parent()
            .expect("tempdir parent")
            .join("escape.gtpack");
        fs::write(&sibling, b"escape")?;

        let err = normalize_under_root(temp.path(), std::path::Path::new("../escape.gtpack"))
            .expect_err("parent traversal should be rejected");

        assert!(err.to_string().contains("path escapes root"));
        Ok(())
    }

    #[test]
    fn caches_root_canonicalization_per_thread() -> Result<()> {
        let temp = TempDir::new()?;
        // Use the canonical path as root so that `canonical == root` holds on
        // platforms where the temp directory contains symlinks (e.g. macOS
        // `/var` -> `/private/var`).
        let canonical_root = temp.path().canonicalize()?;
        let file_path = canonical_root.join("pack.gtpack");
        fs::write(&file_path, b"pack")?;

        CANONICAL_ROOT_CACHE.with(|cache| cache.borrow_mut().clear());
        normalize_under_root(&canonical_root, std::path::Path::new("pack.gtpack"))?;

        let cached =
            CANONICAL_ROOT_CACHE.with(|cache| cache.borrow().get(&canonical_root).cloned());
        assert_eq!(cached, Some(canonical_root));
        Ok(())
    }

    #[test]
    fn does_not_cache_relative_roots() -> Result<()> {
        let current_dir = std::env::current_dir()?;
        let temp = tempfile::tempdir_in(&current_dir)?;
        let relative_root = temp.path().strip_prefix(&current_dir)?;
        let file_path = temp.path().join("pack.gtpack");
        fs::write(&file_path, b"pack")?;

        CANONICAL_ROOT_CACHE.with(|cache| cache.borrow_mut().clear());
        normalize_under_root(relative_root, std::path::Path::new("pack.gtpack"))?;

        let cached = CANONICAL_ROOT_CACHE.with(|cache| cache.borrow().get(relative_root).cloned());
        assert!(cached.is_none());
        Ok(())
    }
}
