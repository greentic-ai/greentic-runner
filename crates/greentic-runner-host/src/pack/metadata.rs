//! Pack metadata extraction and handling.

use serde::{Deserialize, Serialize};
use std::path::Path;
use wasmparser::{Parser, Payload};

/// Metadata about a loaded pack.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PackMetadata {
    pub pack_id: String,
    pub version: String,
    #[serde(default)]
    pub entry_flows: Vec<String>,
    #[serde(default)]
    pub secret_requirements: Vec<greentic_types::SecretRequirement>,
}

impl PackMetadata {
    /// Extract metadata from WASM bytes (custom section or data section).
    pub fn from_wasm(bytes: &[u8]) -> Option<Self> {
        let parser = Parser::new(0);
        for payload in parser.parse_all(bytes) {
            let payload = payload.ok()?;
            match payload {
                Payload::CustomSection(section) => {
                    if section.name() == "greentic.manifest"
                        && let Ok(meta) = Self::from_bytes(section.data())
                    {
                        return Some(meta);
                    }
                }
                Payload::DataSection(reader) => {
                    for segment in reader.into_iter().flatten() {
                        if let Ok(meta) = Self::from_bytes(segment.data) {
                            return Some(meta);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Parse metadata from CBOR bytes.
    fn from_bytes(bytes: &[u8]) -> Result<Self, serde_cbor::Error> {
        #[derive(Deserialize)]
        struct RawManifest {
            pack_id: String,
            version: String,
            #[serde(default)]
            entry_flows: Vec<String>,
            #[serde(default)]
            flows: Vec<RawFlow>,
            #[serde(default)]
            secret_requirements: Vec<greentic_types::SecretRequirement>,
        }

        #[derive(Deserialize)]
        struct RawFlow {
            id: String,
        }

        let manifest: RawManifest = serde_cbor::from_slice(bytes)?;
        let mut entry_flows = if manifest.entry_flows.is_empty() {
            manifest.flows.iter().map(|f| f.id.clone()).collect()
        } else {
            manifest.entry_flows.clone()
        };
        entry_flows.retain(|id| !id.is_empty());
        Ok(Self {
            pack_id: manifest.pack_id,
            version: manifest.version,
            entry_flows,
            secret_requirements: manifest.secret_requirements,
        })
    }

    /// Create fallback metadata from path.
    pub fn fallback(path: &Path) -> Self {
        let pack_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown-pack".to_string());
        Self {
            pack_id,
            version: "0.0.0".to_string(),
            entry_flows: Vec::new(),
            secret_requirements: Vec::new(),
        }
    }

    /// Create metadata from a pack manifest.
    pub fn from_manifest(manifest: &greentic_types::PackManifest) -> Self {
        let entry_flows = manifest
            .flows
            .iter()
            .map(|flow| flow.id.as_str().to_string())
            .collect::<Vec<_>>();
        Self {
            pack_id: manifest.pack_id.as_str().to_string(),
            version: manifest.version.to_string(),
            entry_flows,
            secret_requirements: manifest.secret_requirements.clone(),
        }
    }
}
