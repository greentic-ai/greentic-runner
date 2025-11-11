use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use greentic_pack::builder::{FlowBundle, PackBuilder, PackMeta};
use runner_core::{Index, PackConfig, PackManager};
use semver::Version;
use serde_json::json;
use tempfile::NamedTempFile;

fn main() -> Result<()> {
    ensure_example_pack()?;
    ensure_env_defaults()?;

    let cfg = PackConfig::from_env()?;
    let index = Index::load(&cfg.index_location)?;
    let manager = PackManager::new(cfg.clone())?;

    let resolved = manager.resolve_all_for_index(&index)?;
    for (tenant, packs) in resolved.tenants() {
        println!(
            "tenant `{tenant}` main pack {}@{} cached at {}",
            packs.main.reference.name,
            packs.main.reference.version.cache_label(),
            packs.main.path.display()
        );
    }
    Ok(())
}

fn ensure_env_defaults() -> Result<()> {
    if env::var("PACK_SOURCE").is_err() {
        unsafe {
            env::set_var("PACK_SOURCE", "fs");
        }
    }
    if env::var("PACK_INDEX_URL").is_err() {
        unsafe {
            env::set_var("PACK_INDEX_URL", "./examples/index.json");
        }
    }
    if env::var("PACK_CACHE_DIR").is_err() {
        unsafe {
            env::set_var("PACK_CACHE_DIR", "./.packs");
        }
    }
    Ok(())
}

fn ensure_example_pack() -> Result<()> {
    let pack_path = Path::new("examples/packs/demo.gtpack");
    if pack_path.exists() {
        return Ok(());
    }

    if let Some(parent) = pack_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut wasm_file = NamedTempFile::new()?;
    wasm_file.write_all(b"\0asm\x01\0\0\0")?;

    PackBuilder::new(example_meta())
        .with_flow(example_flow())
        .with_component_wasm("demo.component", Version::parse("1.0.0")?, wasm_file.path())
        .build(pack_path)?;

    println!("built example pack at {}", pack_path.display());
    Ok(())
}

fn example_meta() -> PackMeta {
    PackMeta {
        pack_id: "ai.greentic.runner.example".into(),
        version: Version::parse("0.1.0").unwrap(),
        name: "Runner Example".into(),
        description: Some("Minimal pack for fs resolver showcase".into()),
        authors: vec!["Greentic".into()],
        license: None,
        imports: vec![],
        entry_flows: vec!["qa".into()],
        created_at_utc: "2025-01-01T00:00:00Z".into(),
        annotations: serde_json::Map::new(),
    }
}

fn example_flow() -> FlowBundle {
    let flow_json = json!({
        "id": "qa",
        "kind": "flow/v1",
        "entry": "start",
        "nodes": []
    });
    let hash = blake3::hash(&serde_json::to_vec(&flow_json).unwrap())
        .to_hex()
        .to_string();
    FlowBundle {
        id: "qa".into(),
        kind: "flow/v1".into(),
        entry: "start".into(),
        yaml: "id: qa\nentry: start\n".into(),
        json: flow_json,
        hash_blake3: hash,
        nodes: Vec::new(),
    }
}
