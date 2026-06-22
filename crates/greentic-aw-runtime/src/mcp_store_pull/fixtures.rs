//! Shared test fixtures for cross-module unit tests (e.g. `mcp_source` tests
//! that need to build signed `.gtxpack` archives without re-duplicating the
//! signing/archive logic).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::io::Write as _;

pub(crate) use super::hex_sha256;

/// Resolve the `router_echo` fixture wasm if built; else `None` (test self-skips).
pub(crate) fn fixture_wasm() -> Option<std::path::PathBuf> {
    let p = std::env::var("GREENTIC_MCP_ROUTER_ECHO_WASM")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../greentic-mcp/target/wasm32-wasip2/release/router_echo.wasm")
        });
    p.exists().then_some(p)
}

/// A minimal unsigned describe document for `router_echo 1.0.0`.
pub(crate) fn sample_describe() -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "greentic.ai/v1",
        "kind": "ProviderExtension",
        "metadata": {
            "id": "router_echo",
            "version": "1.0.0",
            "summary": "echo router for tests"
        }
    })
}

/// Sign `describe` the same way the store does — JCS-canonicalize the
/// unsigned describe (`serde_jcs::to_vec`), sign those bytes, then inject
/// `signature {algorithm, publicKey, value}`.
pub(crate) fn sign_describe_like_store(
    describe: &serde_json::Value,
    signing: &SigningKey,
) -> Vec<u8> {
    let message = serde_jcs::to_vec(describe).unwrap();
    let signature = signing.sign(&message);
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    let public_b64 =
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().to_bytes());
    let mut signed = describe.clone();
    signed.as_object_mut().unwrap().insert(
        "signature".to_string(),
        serde_json::json!({
            "algorithm": "ed25519",
            "publicKey": public_b64,
            "value": signature_b64,
        }),
    );
    serde_json::to_vec(&signed).unwrap()
}

/// Build a `.gtxpack` ZIP from raw `describe.json` bytes + `extension.wasm` bytes.
pub(crate) fn build_gtxpack(describe_json: &[u8], wasm: &[u8]) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        writer.start_file(DESCRIBE_ENTRY, options).unwrap();
        writer.write_all(describe_json).unwrap();
        writer.start_file(WASM_ENTRY, options).unwrap();
        writer.write_all(wasm).unwrap();
        writer.finish().unwrap();
    }
    buf
}

/// Format a trusted-signer env value for `signing`.
pub(crate) fn pubkey_env_value(signing: &SigningKey) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().to_bytes())
    )
}
