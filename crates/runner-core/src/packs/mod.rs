use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use greentic_pack::builder::PackManifest;
use greentic_pack::reader::{PackLoad, SigningPolicy, VerifyReport, open_pack};
use semver::Version;

use crate::env::PackConfig;

pub use cache::PackCache;
pub use index::{Index, PackEntry, TenantRecord};
pub use resolver::{FetchResponse, ResolverRegistry};
pub use verify::PackVerifier;

mod cache;
mod index;
pub mod resolver;
mod verify;

/// Reference to a pack as defined in the index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackRef {
    pub name: String,
    pub version: PackVersion,
}

impl PackRef {
    pub fn cache_key(&self) -> String {
        format!(
            "{}-{}",
            sanitize_segment(&self.name),
            self.version.cache_label()
        )
    }
}

/// Version metadata for a pack.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackVersion {
    Semver(Version),
    Digest(PackDigest),
}

impl PackVersion {
    pub fn cache_label(&self) -> Cow<'_, str> {
        match self {
            Self::Semver(v) => Cow::Owned(v.to_string()),
            Self::Digest(digest) => Cow::Owned(digest.cache_label()),
        }
    }

    pub fn as_digest(&self) -> Option<&PackDigest> {
        match self {
            Self::Digest(digest) => Some(digest),
            _ => None,
        }
    }
}

/// Digest (algorithm:value) to assert pack integrity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PackDigest {
    raw: String,
    algorithm: String,
    value: String,
}

impl PackDigest {
    pub fn parse(raw: impl Into<String>) -> Result<Self> {
        let raw_string = raw.into();
        let (algorithm, value) = {
            let (algorithm_raw, value_raw) = raw_string
                .split_once(':')
                .ok_or_else(|| anyhow!("invalid digest `{raw_string}`; expected algo:value"))?;
            if algorithm_raw.is_empty() || value_raw.is_empty() {
                bail!("invalid digest format `{raw_string}`");
            }
            (algorithm_raw.to_ascii_lowercase(), value_raw.to_string())
        };
        Ok(Self {
            raw: raw_string,
            algorithm,
            value,
        })
    }

    pub fn sha256_from_bytes(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        Self {
            raw: format!("sha256:{digest:0x}"),
            algorithm: "sha256".into(),
            value: format!("{digest:0x}"),
        }
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn raw_string(&self) -> String {
        self.raw.clone()
    }

    pub fn cache_label(&self) -> String {
        self.raw.replace(':', "_")
    }

    pub fn matches_file(&self, path: &Path) -> Result<bool> {
        let computed = compute_digest(path)?;
        Ok(computed.raw.eq_ignore_ascii_case(&self.raw))
    }
}

/// Cached and verified pack metadata.
#[derive(Debug, Clone)]
pub struct ResolvedPack {
    pub reference: PackRef,
    pub locator: String,
    pub path: PathBuf,
    pub manifest: PackManifest,
    pub digest: PackDigest,
    pub report: VerifyReport,
}

/// Per-tenant resolved packs.
#[derive(Debug, Clone)]
pub struct TenantPacks {
    pub main: ResolvedPack,
    pub overlays: Vec<ResolvedPack>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSet {
    pub tenants: BTreeMap<String, TenantPacks>,
}

impl ResolvedSet {
    pub fn tenants(&self) -> &BTreeMap<String, TenantPacks> {
        &self.tenants
    }
}

/// Coordinates resolvers, cache, and verification.
pub struct PackManager {
    cfg: PackConfig,
    cache: PackCache,
    registry: ResolverRegistry,
    verifier: Option<PackVerifier>,
}

impl PackManager {
    pub fn new(cfg: PackConfig) -> Result<Self> {
        let verifier = cfg
            .public_key
            .as_deref()
            .map(PackVerifier::from_env_value)
            .transpose()?;
        let mut registry = ResolverRegistry::default();
        registry.register_builtin()?;
        Ok(Self {
            cache: PackCache::new(cfg.cache_dir.clone()),
            cfg,
            registry,
            verifier,
        })
    }

    /// Resolve all packs referenced in the provided index.
    pub fn resolve_all_for_index(&self, index: &Index) -> Result<ResolvedSet> {
        let mut tenants = BTreeMap::new();
        for (tenant, record) in index.tenants() {
            let main = self.resolve_entry(&record.main_pack)?;
            let mut overlays = Vec::new();
            for overlay in &record.overlays {
                overlays.push(self.resolve_entry(overlay)?);
            }
            tenants.insert(tenant.clone(), TenantPacks { main, overlays });
        }
        Ok(ResolvedSet { tenants })
    }

    fn resolve_entry(&self, entry: &PackEntry) -> Result<ResolvedPack> {
        let locator = entry
            .locator
            .with_fallback(self.cfg.source)
            .context("pack locator missing scheme")?;
        let response = self
            .registry
            .fetch(&locator)
            .with_context(|| format!("resolver failed for {}", locator))?;

        let fetched_digest = compute_digest(response.path())?;
        if entry
            .content_digest
            .as_ref()
            .or_else(|| entry.reference.version.as_digest())
            .map(|expected| {
                expected
                    .as_str()
                    .eq_ignore_ascii_case(fetched_digest.as_str())
            })
            == Some(false)
        {
            let expected = entry
                .content_digest
                .as_ref()
                .or_else(|| entry.reference.version.as_digest())
                .map(|value| value.as_str())
                .unwrap_or("<unknown>");
            bail!(
                "digest mismatch for {}: expected {}, found {}",
                entry.reference.name,
                expected,
                fetched_digest.as_str()
            );
        }

        if let Some(verifier) = &self.verifier {
            let signature = entry.signature.as_deref().ok_or_else(|| {
                anyhow!(
                    "signature missing for pack {} but PACK_PUBLIC_KEY is configured",
                    entry.reference.name
                )
            })?;
            verifier.verify(fetched_digest.as_str().as_bytes(), signature)?;
        }

        let cached = self.cache.store(entry, response.path(), &fetched_digest)?;
        let PackLoad {
            manifest, report, ..
        } = open_pack(&cached, SigningPolicy::DevOk).map_err(|err| {
            anyhow!(
                "failed to open cached pack {}: {}",
                cached.display(),
                err.message
            )
        })?;

        Ok(ResolvedPack {
            reference: entry.reference.clone(),
            locator,
            path: cached,
            manifest,
            digest: fetched_digest,
            report,
        })
    }
}

fn compute_digest(path: &Path) -> Result<PackDigest> {
    use sha2::{Digest, Sha256};
    const BUF_SIZE: usize = 64 * 1024;
    let file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; BUF_SIZE];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    PackDigest::parse(format!("sha256:{digest:0x}"))
}

fn sanitize_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '_',
            other => other,
        })
        .collect()
}
