use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use tempfile::TempPath;

mod azblob;
mod fs;
mod gcs;
mod http;
mod oci;
mod s3;

pub use azblob::AzBlobResolver;
pub use fs::FsResolver;
pub use gcs::GcsResolver;
pub use http::HttpResolver;
pub use oci::OciResolver;
pub use s3::S3Resolver;

/// Response from a resolver indicating where the artifact was stored.
pub struct FetchResponse {
    location: FetchLocation,
}

impl FetchResponse {
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            location: FetchLocation::Permanent(path),
        }
    }

    pub fn from_temp(path: TempPath) -> Self {
        Self {
            location: FetchLocation::Temporary(path),
        }
    }

    pub fn path(&self) -> &Path {
        match &self.location {
            FetchLocation::Permanent(path) => path,
            FetchLocation::Temporary(path) => path.as_ref(),
        }
    }
}

enum FetchLocation {
    Permanent(PathBuf),
    Temporary(TempPath),
}

pub trait PackResolver: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn fetch(&self, locator: &str) -> Result<FetchResponse>;
}

#[derive(Default)]
pub struct ResolverRegistry {
    resolvers: HashMap<String, Arc<dyn PackResolver>>,
}

impl ResolverRegistry {
    pub fn register(&mut self, resolver: impl PackResolver + 'static) {
        self.resolvers
            .insert(resolver.scheme().to_string(), Arc::new(resolver));
    }

    pub fn register_builtin(&mut self) -> Result<()> {
        self.register(FsResolver::new());
        self.register(HttpResolver::new("http")?);
        self.register(HttpResolver::new("https")?);
        self.register(OciResolver::new()?);
        self.register(S3Resolver::new()?);
        self.register(GcsResolver::new()?);
        self.register(AzBlobResolver::new()?);
        Ok(())
    }

    pub fn fetch(&self, reference: &str) -> Result<FetchResponse> {
        let parsed = ParsedReference::parse(reference)?;
        let resolver = self
            .resolvers
            .get(&parsed.scheme)
            .ok_or_else(|| anyhow!("no resolver registered for scheme `{}`", parsed.scheme))?;
        resolver.fetch(&parsed.locator)
    }
}

struct ParsedReference {
    scheme: String,
    locator: String,
}

impl ParsedReference {
    fn parse(input: &str) -> Result<Self> {
        let (scheme_part, rest) = input
            .split_once("://")
            .ok_or_else(|| anyhow!("reference `{input}` is missing a URI scheme"))?;
        if rest.is_empty() {
            bail!("reference `{input}` is missing a locator");
        }
        if let Some((logical, actual)) = scheme_part.split_once('+') {
            let locator = format!("{actual}://{rest}");
            return Ok(Self {
                scheme: logical.to_ascii_lowercase(),
                locator,
            });
        }
        Ok(Self {
            scheme: scheme_part.to_ascii_lowercase(),
            locator: input.to_string(),
        })
    }
}
