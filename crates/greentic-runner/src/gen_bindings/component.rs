use anyhow::{Context, Result};
use wasmparser::{Parser, Payload};

#[derive(Debug, Default, Clone)]
pub struct ComponentFeatures {
    pub http: bool,
    pub secrets: bool,
    pub filesystem: bool,
}

pub fn analyze_component(path: &std::path::Path) -> Result<ComponentFeatures> {
    let wasm = std::fs::read(path)
        .with_context(|| format!("failed to read component {}", path.display()))?;
    let mut features = ComponentFeatures::default();
    for payload in Parser::new(0).parse_all(&wasm) {
        if let Payload::ImportSection(section) = payload? {
            for import in section {
                let import = import?;
                match import.module {
                    "greentic:host/http-v1" => features.http = true,
                    "greentic:host/secrets-v1" => features.secrets = true,
                    "greentic:host/filesystem-v1" => features.filesystem = true,
                    "greentic:host/kv-v1" => features.filesystem = true,
                    _ => (),
                }
            }
        }
    }
    Ok(features)
}
