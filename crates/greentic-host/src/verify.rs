use std::path::Path;

use anyhow::{Context, Result};
use tokio::fs;

pub async fn verify_pack(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .await
        .with_context(|| format!("pack file {:?} does not exist", path))?;
    if !metadata.is_file() {
        anyhow::bail!("pack path {:?} is not a file", path);
    }
    // TODO: perform signature verification and manifest hash validation
    Ok(())
}
