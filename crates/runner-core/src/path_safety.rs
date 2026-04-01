use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Normalize a user-supplied path and ensure it stays within an allowed root.
/// Rejects absolute candidates and any that escape the root via `..`.
pub fn normalize_under_root(root: &Path, candidate: &Path) -> Result<PathBuf> {
    if candidate.is_absolute() {
        anyhow::bail!("absolute paths are not allowed: {}", candidate.display());
    }

    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
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

#[cfg(test)]
mod tests {
    use super::normalize_under_root;
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
}
