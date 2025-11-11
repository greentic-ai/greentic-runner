use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use url::Url;

/// Environment-driven configuration for pack management.
#[derive(Debug, Clone)]
pub struct PackConfig {
    pub source: PackSource,
    pub index_location: IndexLocation,
    pub cache_dir: PathBuf,
    pub public_key: Option<String>,
}

impl PackConfig {
    /// Build a [`PackConfig`] by reading the documented PACK_* variables.
    pub fn from_env() -> Result<Self> {
        let source = env::var("PACK_SOURCE")
            .ok()
            .map(|value| PackSource::from_str(&value))
            .transpose()?
            .unwrap_or(PackSource::Fs);

        let index_raw = env::var("PACK_INDEX_URL")
            .context("PACK_INDEX_URL is required to locate the pack index")?;
        let index_location = IndexLocation::from_value(&index_raw)?;

        let cache_dir = env::var("PACK_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".packs"));

        let public_key = env::var("PACK_PUBLIC_KEY").ok();

        Ok(Self {
            source,
            index_location,
            cache_dir,
            public_key,
        })
    }
}

/// Location of the pack index document (supports file paths and HTTP/S URLs).
#[derive(Debug, Clone)]
pub enum IndexLocation {
    File(PathBuf),
    Remote(Url),
}

impl IndexLocation {
    pub fn from_value(value: &str) -> Result<Self> {
        if value.starts_with("http://") || value.starts_with("https://") {
            let url = Url::parse(value).context("PACK_INDEX_URL is not a valid URL")?;
            return Ok(Self::Remote(url));
        }
        if value.starts_with("file://") {
            let url = Url::parse(value).context("PACK_INDEX_URL is not a valid file:// URL")?;
            let path = url
                .to_file_path()
                .map_err(|_| anyhow!("PACK_INDEX_URL points to an invalid file URI"))?;
            return Ok(Self::File(path));
        }
        Ok(Self::File(PathBuf::from(value)))
    }

    pub fn display(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::Remote(url) => url.to_string(),
        }
    }
}

/// Supported default sources for packs when the index omits the URI scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackSource {
    Fs,
    Http,
    Oci,
    S3,
    Gcs,
    AzBlob,
}

impl PackSource {
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Http => "http",
            Self::Oci => "oci",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
            Self::AzBlob => "azblob",
        }
    }
}

impl FromStr for PackSource {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "fs" => Ok(Self::Fs),
            "http" | "https" => Ok(Self::Http),
            "oci" => Ok(Self::Oci),
            "s3" => Ok(Self::S3),
            "gcs" => Ok(Self::Gcs),
            "azblob" | "azure" | "azureblob" => Ok(Self::AzBlob),
            other => bail!("unsupported PACK_SOURCE `{other}`"),
        }
    }
}
