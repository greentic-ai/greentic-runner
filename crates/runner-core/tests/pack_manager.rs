use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Result, anyhow};
use greentic_pack::builder::{FlowBundle, PackBuilder, PackMeta};
use runner_core::packs::PackDigest;
use runner_core::{Index, IndexLocation, PackConfig, PackManager, PackSource};
use semver::Version;
use serde_json::json;
use tiny_http::{Response, Server};

fn sample_meta() -> PackMeta {
    PackMeta {
        pack_id: "ai.greentic.runner.tests".into(),
        version: Version::parse("0.1.0").unwrap(),
        name: "Test Runner".into(),
        description: None,
        authors: vec!["Greentic".into()],
        license: None,
        imports: vec![],
        entry_flows: vec!["qa".into()],
        created_at_utc: "2025-01-01T00:00:00Z".into(),
        annotations: serde_json::Map::new(),
    }
}

fn sample_flow() -> FlowBundle {
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

fn build_test_pack(dir: &Path) -> Result<PathBuf> {
    let wasm_path = dir.join("component.wasm");
    fs::write(&wasm_path, b"\0asm\x01\0\0\0")?;
    let out_path = dir.join("sample.gtpack");
    PackBuilder::new(sample_meta())
        .with_flow(sample_flow())
        .with_component_wasm(
            "demo.component",
            Version::parse("1.0.0")?,
            wasm_path.as_path(),
        )
        .build(&out_path)?;
    Ok(out_path)
}

fn compute_digest(path: &Path) -> Result<PackDigest> {
    let bytes = fs::read(path)?;
    Ok(PackDigest::sha256_from_bytes(&bytes))
}

fn write_index(path: &Path, locator: &str, digest: &PackDigest) -> Result<()> {
    let index = json!({
        "demo": {
            "main_pack": {
                "name": "runner.demo",
                "version": "0.1.0",
                "locator": locator,
                "digest": digest.as_str(),
            },
            "overlays": []
        }
    });
    fs::write(path, serde_json::to_vec_pretty(&index)?)?;
    Ok(())
}

fn build_config(index: &Path, cache_dir: &Path, source: PackSource) -> PackConfig {
    PackConfig {
        source,
        index_location: IndexLocation::File(index.to_path_buf()),
        cache_dir: cache_dir.to_path_buf(),
        public_key: None,
    }
}

#[test]
fn resolves_fs_pack() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pack_path = build_test_pack(temp.path())?;
    let digest = compute_digest(&pack_path)?;
    let index_path = temp.path().join("index.json");
    write_index(&index_path, pack_path.to_str().unwrap(), &digest)?;

    let config = build_config(&index_path, &temp.path().join("cache"), PackSource::Fs);
    let index = Index::load(&config.index_location)?;
    let manager = PackManager::new(config)?;
    let resolved = manager.resolve_all_for_index(&index)?;
    let tenant = resolved.tenants().get("demo").expect("tenant missing");
    assert_eq!(tenant.overlays.len(), 0);
    assert_eq!(
        tenant.main.manifest.meta.pack_id,
        "ai.greentic.runner.tests"
    );
    assert_eq!(tenant.main.digest.as_str(), digest.as_str());
    Ok(())
}

#[test]
fn resolves_http_pack() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let pack_path = build_test_pack(temp.path())?;
    let digest = compute_digest(&pack_path)?;
    let bytes = fs::read(&pack_path)?;

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping http resolver test: {err}");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;
    let server =
        Server::from_listener(listener, None).map_err(|err| anyhow!("server error: {err}"))?;
    thread::spawn(move || {
        if let Ok(request) = server.recv() {
            let response = Response::from_data(bytes.clone());
            let _ = request.respond(response);
        }
    });

    let locator = format!("http://{}/pack.gtpack", addr);
    let index_path = temp.path().join("index.json");
    write_index(&index_path, &locator, &digest)?;
    let config = build_config(&index_path, &temp.path().join("cache"), PackSource::Http);
    let index = Index::load(&config.index_location)?;
    let manager = PackManager::new(config)?;
    let resolved = manager.resolve_all_for_index(&index)?;
    let tenant = resolved.tenants().get("demo").expect("tenant missing");
    assert_eq!(tenant.main.digest.as_str(), digest.as_str());
    Ok(())
}
