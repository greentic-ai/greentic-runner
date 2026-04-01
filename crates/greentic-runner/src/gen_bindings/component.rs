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
            for imports in section {
                let imports = imports?;
                let module = match imports {
                    wasmparser::Imports::Single(_, import) => import.module,
                    wasmparser::Imports::Compact1 { module, .. } => module,
                    wasmparser::Imports::Compact2 { module, .. } => module,
                };
                match module {
                    "greentic:host/http-v1" => features.http = true,
                    "greentic:host/secrets-v1" => features.secrets = true,
                    "greentic:host/filesystem-v1" | "greentic:host/kv-v1" => {
                        features.filesystem = true;
                    }
                    _ => (),
                }
            }
        }
    }
    Ok(features)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyze_component_reports_read_errors() {
        let err = analyze_component(std::path::Path::new("does-not-exist.wasm"))
            .expect_err("missing component should error");
        assert!(err.to_string().contains("failed to read component"));
    }
}
