use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use url::Url;

use super::{FetchResponse, PackResolver};

#[derive(Debug, Default)]
pub struct FsResolver;

impl FsResolver {
    pub fn new() -> Self {
        Self
    }

    fn parse_path(&self, locator: &str) -> Result<PathBuf> {
        if let Some(stripped) = locator.strip_prefix("fs://") {
            if stripped.starts_with('/')
                || stripped.starts_with("./")
                || stripped.starts_with("../")
            {
                return Ok(PathBuf::from(stripped));
            }
            if cfg!(windows) && stripped.chars().nth(1) == Some(':') {
                return Ok(PathBuf::from(stripped));
            }
            let file_url = format!("file://{stripped}");
            let url = Url::parse(&file_url).context("failed to parse fs:// locator as file URL")?;
            return url
                .to_file_path()
                .map_err(|_| anyhow!("fs locator {locator} cannot be represented as a path"));
        }
        Ok(PathBuf::from(locator))
    }
}

impl PackResolver for FsResolver {
    fn scheme(&self) -> &'static str {
        "fs"
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        let path = self.parse_path(locator)?;
        if !path.exists() {
            anyhow::bail!("fs resolver: {} does not exist", path.display());
        }
        Ok(FetchResponse::from_path(path))
    }
}
