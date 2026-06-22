//! Store-pull + verify for the `local-wasm` MCP transport.
//!
//! Given a pinned `(component_ref, version, digest)` triple, download the
//! matching `.gtxpack` from the Greentic store, verify it, and extract
//! `extension.wasm` into the [`mcp_local`](crate::mcp_local) cache so the
//! in-process executor can load it.
//!
//! Three independent checks gate caching, all of which must pass:
//! 1. **Integrity** — `sha256(.gtxpack) == component_digest` (the pinned wire
//!    row digest from Task 1). Defends against a swapped/corrupted artifact.
//! 2. **Authenticity** — the Ed25519 signature embedded in the archive's
//!    `describe.json` verifies against a trusted-signer allowlist
//!    ([`GREENTIC_MCP_TRUSTED_SIGNERS`]). Defends against an attacker who
//!    controls the store but not a publisher key.
//! 3. **Atomic install** — the bare `extension.wasm` is written to
//!    `<cache>/<ref>.wasm` via temp-file + rename so a concurrent reader never
//!    observes a half-written file.
//!
//! The Ed25519 chain is verified HERE. The downstream `mcp-exec` executor only
//! enforces the SHA256 `required_digest` on the extracted wasm — it never sees
//! the archive or the publisher signature.
//!
//! ## Canonicalization (the correctness gate)
//!
//! The store signs the describe document **before** injecting the `signature`
//! object: `serde_jcs::to_vec(&describe)` of the unsigned describe — JCS
//! canonicalization, RFC 8785 (`greentic-store-api` publish handler). It then
//! inserts `signature {algorithm, publicKey, value}` and repacks the archive so
//! `describe.json` carries that signature verbatim. To reconstruct the signed
//! bytes we parse `describe.json`, drop the `signature` field, and
//! `serde_jcs::to_vec` the remainder — yielding bytes identical to what the
//! store signed. We use JCS (not plain `serde_json::to_vec`) so the two sides
//! agree even on values where JCS and serde_json diverge (floats, non-ASCII
//! keys, exponent-form numbers).

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::mcp_local::cache_dir;

/// Env var holding the trusted-signer allowlist: a comma-separated list of
/// `ed25519:<base64-standard-pubkey>` entries. An empty/unset allowlist makes
/// every signature verification fail closed.
pub(crate) const TRUSTED_SIGNERS_ENV: &str = "GREENTIC_MCP_TRUSTED_SIGNERS";
/// Base URL of the store HTTP API (no trailing path).
pub(crate) const STORE_URL_ENV: &str = "GREENTIC_STORE_URL";
/// Optional `gts_` service token sent as a bearer credential.
pub(crate) const STORE_TOKEN_ENV: &str = "GREENTIC_STORE_TOKEN";

/// The ZIP entry names this loader looks for inside the `.gtxpack`.
const DESCRIBE_ENTRY: &str = "describe.json";
const WASM_ENTRY: &str = "extension.wasm";

/// Maximum size of a `.gtxpack` artifact accepted from the store. Prevents a
/// hostile or misconfigured store from OOM-ing the runner before the digest check.
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum DECOMPRESSED size of any single ZIP entry inside a `.gtxpack`. The
/// `MAX_ARTIFACT_BYTES` cap bounds the compressed archive, but a hostile (or
/// MITM'd) store could ship a small archive whose entries inflate hugely (a zip
/// bomb). Bound the inflated output so extraction can never OOM the runner. A
/// generous ceiling — real `extension.wasm` binaries are well under this.
const MAX_ZIP_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// Typed, non-panicking failure modes for the store-pull path.
#[derive(Debug, thiserror::Error)]
pub enum StorePullError {
    /// Required configuration (e.g. the store URL) is missing or malformed.
    #[error("store-pull config error: {0}")]
    Config(String),
    /// The artifact download failed (transport, status, or body read).
    #[error("store-pull network error: {0}")]
    Network(String),
    /// `sha256(.gtxpack)` did not match the pinned digest.
    #[error("store-pull integrity error: {0}")]
    Integrity(String),
    /// The describe signature was absent, malformed, or untrusted.
    #[error("store-pull signature error: {0}")]
    Signature(String),
    /// The downloaded bytes were not a well-formed `.gtxpack` archive.
    #[error("store-pull archive error: {0}")]
    Archive(String),
    /// Writing the extracted wasm into the cache failed.
    #[error("store-pull io error: {0}")]
    Io(String),
}

/// Ensure `<cache_dir>/<component_ref>.wasm` exists and is verified.
///
/// Short-circuits only when `<ref>.wasm`, its `<ref>.wasm.sha256` sidecar, AND
/// the `<ref>.wasm.gtxpack` marker are all present AND the marker equals this
/// call's `component_digest` — so a version/digest upgrade (a re-registered or
/// security-patched component) re-pulls instead of serving the stale cached
/// wasm. A wasm-only entry (or one with a stale/missing marker) is re-pulled and
/// re-verified. Otherwise the matching `.gtxpack` is downloaded, its sha256 is
/// checked against `component_digest`, the embedded describe signature is verified
/// against the
/// trusted allowlist, and `extension.wasm` is extracted into the cache atomically
/// alongside a fresh sidecar.
///
/// # Arguments
/// * `component_ref` — the store extension name; also the cache file stem.
/// * `component_version` — the pinned version segment of the artifact URL.
/// * `component_digest` — hex SHA256 of the `.gtxpack` (case-insensitive).
///
/// # Errors
/// Returns a [`StorePullError`] on any config, network, integrity, signature,
/// archive, or IO failure. Never panics; nothing is cached on error.
pub async fn ensure_cached(
    component_ref: &str,
    component_version: &str,
    component_digest: &str,
) -> Result<(), StorePullError> {
    let dest = cache_dir().join(format!("{component_ref}.wasm"));
    // Cache-hit ONLY when the cached wasm, its execution-digest sidecar, AND the
    // pinned-gtxpack marker are all present and the marker matches THIS request's
    // `component_digest`. Keying on file existence alone (the previous behaviour)
    // ignored the version/digest: a re-registered component (new version, new
    // digest — e.g. a security patch) would short-circuit on the stale cached
    // wasm and the runner would keep executing the OLD code indefinitely.
    // Comparing the marker forces a re-pull whenever the pinned digest changes.
    if dest.exists()
        && sidecar_path(&dest).exists()
        && gtxpack_marker_matches(&gtxpack_marker_path(&dest), component_digest)
    {
        return Ok(());
    }

    let base = std::env::var(STORE_URL_ENV)
        .map_err(|_| StorePullError::Config(format!("{STORE_URL_ENV} unset")))?;
    let url = format!(
        "{}/api/v1/extensions/{component_ref}/{component_version}/artifact",
        base.trim_end_matches('/')
    );

    // 1. Download the .gtxpack archive.
    let archive_bytes = download_artifact(&url).await?;

    // 2. Integrity: sha256(.gtxpack) must equal the pinned digest.
    let computed = hex_sha256(&archive_bytes);
    if !computed.eq_ignore_ascii_case(component_digest) {
        return Err(StorePullError::Integrity(format!(
            "digest mismatch for {component_ref}@{component_version}: computed {computed}, expected {component_digest}"
        )));
    }

    // 3. Unzip into the describe document + the bare wasm bytes.
    let (describe, wasm) = unzip_describe_and_wasm(&archive_bytes)?;

    // 4. Authenticity: Ed25519 over the describe-minus-signature.
    verify_describe_signature(&describe, &trusted_signers())?;

    // 5. Atomically install. The write ORDER is load-bearing for crash- and
    //    concurrency-safety:
    //    (a) the execution-digest sidecar (`sha256(extension.wasm)`) FIRST — so a
    //        concurrent reader never observes the wasm WITHOUT its sidecar, which
    //        `mcp_local::exec_config_for` would treat as an operator-seeded cache
    //        and run with `allow_unverified: true`. This is `sha256(extension.wasm)`
    //        — the artifact mcp-exec actually runs — not the gtxpack digest.
    //    (b) the verified wasm itself.
    //    (c) the pinned-gtxpack marker LAST, as the completion sentinel — its
    //        presence AND matching value is what the cache-hit check above trusts.
    //        Writing it last means a half-finished pull (sidecar/wasm but no marker)
    //        re-pulls rather than serving an unconfirmed cache.
    let wasm_digest = hex_sha256(&wasm);
    write_atomic(&sidecar_path(&dest), wasm_digest.as_bytes())
        .map_err(|e| StorePullError::Io(format!("write wasm sidecar: {e}")))?;
    write_atomic(&dest, &wasm).map_err(|e| StorePullError::Io(e.to_string()))?;
    write_atomic(&gtxpack_marker_path(&dest), component_digest.as_bytes())
        .map_err(|e| StorePullError::Io(format!("write gtxpack marker: {e}")))?;

    Ok(())
}

/// Return the sidecar path for a cached wasm file (e.g. `router_echo.wasm.sha256`).
pub(crate) fn sidecar_path(wasm_dest: &std::path::Path) -> std::path::PathBuf {
    let mut path = wasm_dest.to_path_buf();
    let new_extension = match path.extension() {
        Some(ext) => format!("{}.sha256", ext.to_string_lossy()),
        None => "sha256".to_string(),
    };
    path.set_extension(new_extension);
    path
}

/// Path of the pinned-gtxpack-digest marker for a cached wasm
/// (e.g. `router_echo.wasm.gtxpack`). It stores the `component_digest` (the
/// `sha256(.gtxpack)`) that produced the cached wasm; written LAST on a
/// successful pull and read by [`ensure_cached`] to detect version/digest
/// upgrades on a cache hit (a changed pin ⇒ re-pull).
fn gtxpack_marker_path(wasm_dest: &std::path::Path) -> std::path::PathBuf {
    let mut path = wasm_dest.to_path_buf();
    let new_extension = match path.extension() {
        Some(ext) => format!("{}.gtxpack", ext.to_string_lossy()),
        None => "gtxpack".to_string(),
    };
    path.set_extension(new_extension);
    path
}

/// True only when the marker file exists and its contents equal `expected_digest`
/// (case-insensitive, whitespace-trimmed). A missing/unreadable marker or any
/// mismatch returns `false`, forcing a re-pull — fail-safe toward re-verifying
/// rather than serving a possibly-stale cached wasm.
fn gtxpack_marker_matches(marker: &std::path::Path, expected_digest: &str) -> bool {
    match std::fs::read_to_string(marker) {
        Ok(contents) => contents.trim().eq_ignore_ascii_case(expected_digest.trim()),
        Err(_) => false,
    }
}

/// Verify the Ed25519 signature embedded in `describe.json`.
///
/// Reconstructs the exact bytes the store signed — the describe document with
/// its `signature` field removed, re-serialized via `serde_jcs::to_vec` (JCS,
/// RFC 8785) — then checks the decoded signature against every key in
/// `trusted`. Succeeds if any
/// trusted key validates the signature; fails closed when `trusted` is empty.
///
/// # Errors
/// [`StorePullError::Signature`] when the signature is absent, the describe is
/// not an object, the base64 / signature bytes are malformed, the allowlist is
/// empty, or no trusted key validates.
pub fn verify_describe_signature(
    describe: &serde_json::Value,
    trusted: &[VerifyingKey],
) -> Result<(), StorePullError> {
    let signature_object = describe
        .get("signature")
        .ok_or_else(|| StorePullError::Signature("describe.json has no signature".into()))?;
    let signature_b64 = signature_object
        .get("value")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            StorePullError::Signature("signature.value missing or not a string".into())
        })?;

    // Reconstruct the signed message: describe without the `signature` field,
    // serialized exactly as the store did. The store signs
    // `serde_jcs::to_vec(&describe)` (JCS / RFC 8785) BEFORE injecting the
    // signature, so we must canonicalize with the SAME serializer — not plain
    // `serde_json::to_vec` (which diverges from JCS on floats / non-ASCII keys).
    let mut unsigned = describe.clone();
    unsigned
        .as_object_mut()
        .ok_or_else(|| StorePullError::Signature("describe.json is not a JSON object".into()))?
        .remove("signature");
    let signed_message = serde_jcs::to_vec(&unsigned)
        .map_err(|e| StorePullError::Signature(format!("re-serialize describe (JCS): {e}")))?;

    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|e| StorePullError::Signature(format!("signature is not valid base64: {e}")))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|e| StorePullError::Signature(format!("malformed Ed25519 signature: {e}")))?;

    // Fail closed: an empty allowlist trusts nobody.
    if trusted.is_empty() {
        return Err(StorePullError::Signature(format!(
            "no trusted signers configured ({TRUSTED_SIGNERS_ENV} empty)"
        )));
    }

    // `verify_strict` (not `verify`) — additionally rejects signatures with a
    // small-order public-key/`R` component. A legitimately generated signature
    // never triggers this; using the strict check matches the admin's
    // `store_verify` path and removes a theoretical malleability foothold.
    if trusted
        .iter()
        .any(|key| key.verify_strict(&signed_message, &signature).is_ok())
    {
        Ok(())
    } else {
        Err(StorePullError::Signature(
            "describe signature does not match any trusted signer".into(),
        ))
    }
}

/// Parse the trusted-signer allowlist from [`TRUSTED_SIGNERS_ENV`].
///
/// Accepts a comma-separated list of `ed25519:<base64pubkey>` entries (the
/// `ed25519:` prefix is optional). Entries that are empty, use a non-ed25519
/// algorithm, or do not decode to a 32-byte public key are skipped — a
/// malformed entry cannot silently widen trust. Returns an empty vec when the
/// var is unset, which makes verification fail closed.
pub fn trusted_signers() -> Vec<VerifyingKey> {
    let raw = match std::env::var(TRUSTED_SIGNERS_ENV) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    raw.split(',')
        .filter_map(|entry| parse_trusted_signer(entry.trim()))
        .collect()
}

/// Parse a single `ed25519:<base64>` (or bare `<base64>`) allowlist entry into
/// a [`VerifyingKey`], returning `None` for any malformed entry.
fn parse_trusted_signer(entry: &str) -> Option<VerifyingKey> {
    if entry.is_empty() {
        return None;
    }
    let key_b64 = match entry.split_once(':') {
        Some((algorithm, key)) => {
            if !algorithm.eq_ignore_ascii_case("ed25519") {
                return None;
            }
            key.trim()
        }
        None => entry,
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .ok()?;
    let key_bytes: [u8; 32] = raw.as_slice().try_into().ok()?;
    VerifyingKey::from_bytes(&key_bytes).ok()
}

/// GET the `.gtxpack` bytes, sending the optional `gts_` bearer token.
///
/// Rejects the response before buffering when `Content-Length` exceeds
/// [`MAX_ARTIFACT_BYTES`], and also after buffering in case the header was absent
/// or lied. This limits exposure to a hostile/misconfigured store.
async fn download_artifact(url: &str) -> Result<Vec<u8>, StorePullError> {
    let mut request = reqwest::Client::new().get(url);
    if let Ok(token) = std::env::var(STORE_TOKEN_ENV)
        && !token.is_empty()
    {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|e| StorePullError::Network(format!("GET {url}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(StorePullError::Network(format!(
            "GET {url} returned HTTP {status}"
        )));
    }
    // Pre-flight: reject oversized artifacts before buffering the body.
    if let Some(content_length) = response.content_length()
        && content_length > MAX_ARTIFACT_BYTES
    {
        return Err(StorePullError::Integrity(format!(
            "artifact at {url} claims Content-Length {content_length} which exceeds the {MAX_ARTIFACT_BYTES}-byte cap"
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| StorePullError::Network(format!("read body from {url}: {e}")))?;
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        return Err(StorePullError::Integrity(format!(
            "artifact at {url} is {} bytes which exceeds the {MAX_ARTIFACT_BYTES}-byte cap",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

/// Hex-lowercase SHA256 of `bytes`. Matches the store's `hex::encode` output.
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut accumulator, byte| {
            // `write!` into a String is infallible.
            let _ = write!(accumulator, "{byte:02x}");
            accumulator
        },
    )
}

/// Read the `describe.json` (parsed) and `extension.wasm` (raw) entries from a
/// `.gtxpack` ZIP. Both must be present at the archive root.
fn unzip_describe_and_wasm(
    archive_bytes: &[u8],
) -> Result<(serde_json::Value, Vec<u8>), StorePullError> {
    let cursor = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| StorePullError::Archive(format!("open gtxpack: {e}")))?;

    let describe_raw = read_zip_entry(&mut archive, DESCRIBE_ENTRY)?;
    let describe: serde_json::Value = serde_json::from_slice(&describe_raw)
        .map_err(|e| StorePullError::Archive(format!("parse {DESCRIBE_ENTRY}: {e}")))?;
    let wasm = read_zip_entry(&mut archive, WASM_ENTRY)?;

    Ok((describe, wasm))
}

/// Read a single named entry from `archive`, mapping a missing entry to a
/// descriptive [`StorePullError::Archive`]. The decompressed output is bounded by
/// [`MAX_ZIP_ENTRY_BYTES`] so a zip bomb (a small archive declaring/inflating a
/// huge entry) cannot OOM the runner.
fn read_zip_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, StorePullError> {
    let entry = archive
        .by_name(name)
        .map_err(|e| StorePullError::Archive(format!("{name} not in gtxpack: {e}")))?;
    // `entry.size()` is the attacker-controlled declared size from the ZIP
    // header — clamp the pre-allocation so a lying header can't OOM us before a
    // byte is read.
    let prealloc = usize::try_from(entry.size().min(MAX_ZIP_ENTRY_BYTES)).unwrap_or(0);
    let mut buffer = Vec::with_capacity(prealloc);
    // Cap the actual inflated read at the limit + 1 so we can detect overflow.
    let read = std::io::Read::take(entry, MAX_ZIP_ENTRY_BYTES + 1)
        .read_to_end(&mut buffer)
        .map_err(|e| StorePullError::Archive(format!("read {name}: {e}")))?;
    if read as u64 > MAX_ZIP_ENTRY_BYTES {
        return Err(StorePullError::Archive(format!(
            "{name} decompresses past the {MAX_ZIP_ENTRY_BYTES}-byte limit (possible zip bomb)"
        )));
    }
    Ok(buffer)
}

/// Write `bytes` to `dest` atomically: write a sibling temp file, fsync it, then
/// rename over `dest`. The temp file uses a unique name so concurrent pulls of
/// different refs never collide.
fn write_atomic(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = unique_temp_path(dest);
    {
        let mut file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
    }
    // Rename is atomic on the same filesystem; clean up the temp on failure.
    if let Err(error) = std::fs::rename(&temp, dest) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

/// Build a unique sibling temp path for an atomic write target.
fn unique_temp_path(dest: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let stem = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact");
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{stem}.{pid}.{nonce}.tmp"))
}

/// Shared test fixtures for cross-module unit tests (e.g. `mcp_source` tests
/// that need to build signed `.gtxpack` archives without re-duplicating the
/// signing/archive logic).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod fixtures {
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
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::Write as _;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Resolve a built `router_echo` wasm if present; else `None` (self-skip).
    fn fixture_wasm() -> Option<std::path::PathBuf> {
        let p = std::env::var("GREENTIC_MCP_ROUTER_ECHO_WASM")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../greentic-mcp/target/wasm32-wasip2/release/router_echo.wasm")
            });
        p.exists().then_some(p)
    }

    /// A minimal but realistic unsigned describe document.
    fn sample_describe() -> serde_json::Value {
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

    /// Sign `describe` THE SAME WAY THE STORE DOES: JCS-canonicalize the
    /// unsigned describe with `serde_jcs::to_vec` (RFC 8785), sign those bytes,
    /// then inject the `signature {algorithm, publicKey, value}` object. Returns
    /// the signed describe bytes (what the archive's `describe.json` carries).
    fn sign_describe_like_store(describe: &serde_json::Value, signing: &SigningKey) -> Vec<u8> {
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

    /// Build a `.gtxpack` ZIP with the given `describe.json` + `extension.wasm`.
    fn build_gtxpack(describe_json: &[u8], wasm: &[u8]) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file(DESCRIBE_ENTRY, options).unwrap();
            writer.write_all(describe_json).unwrap();
            writer.start_file(WASM_ENTRY, options).unwrap();
            writer.write_all(wasm).unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    fn pubkey_env_value(signing: &SigningKey) -> String {
        format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().to_bytes())
        )
    }

    /// Stand up a mock store that serves `archive` at the artifact route.
    async fn mock_store(archive: Vec<u8>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extensions/router_echo/1.0.0/artifact"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(archive),
            )
            .mount(&server)
            .await;
        server
    }

    // ---- unit tests (no fixture wasm required) ----

    /// The gtxpack marker is what makes a cache hit honour version/digest
    /// upgrades: a cached wasm is only reused when its marker equals the
    /// requested `component_digest`. Missing or mismatched ⇒ false ⇒ re-pull.
    #[test]
    fn gtxpack_marker_matches_is_case_insensitive_and_fails_safe() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("router_echo.wasm.gtxpack");

        // Missing marker (e.g. a pre-upgrade cache without one) ⇒ re-pull.
        assert!(!gtxpack_marker_matches(&marker, "abc123"));

        // Present + equal (case-insensitive, whitespace-trimmed) ⇒ cache hit.
        std::fs::write(&marker, "  ABC123\n").unwrap();
        assert!(gtxpack_marker_matches(&marker, "abc123"));

        // Present but DIFFERENT (the digest was upgraded) ⇒ re-pull, so the new
        // version actually reaches the runner instead of serving stale wasm.
        assert!(!gtxpack_marker_matches(&marker, "def456"));
    }

    /// The marker path is the wasm path with `.gtxpack` appended, distinct from
    /// the `.sha256` execution-digest sidecar.
    #[test]
    fn gtxpack_marker_path_appends_extension() {
        let wasm = std::path::Path::new("/cache/router_echo.wasm");
        assert_eq!(
            gtxpack_marker_path(wasm),
            std::path::Path::new("/cache/router_echo.wasm.gtxpack")
        );
        assert_ne!(gtxpack_marker_path(wasm), sidecar_path(wasm));
    }

    #[test]
    #[serial_test::serial]
    fn verify_describe_signature_round_trips_store_signing() {
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let describe = sample_describe();
        let signed_bytes = sign_describe_like_store(&describe, &signing);
        let signed: serde_json::Value = serde_json::from_slice(&signed_bytes).unwrap();
        let trusted = vec![signing.verifying_key()];
        verify_describe_signature(&signed, &trusted).expect("valid signature must verify");
    }

    /// Proves the verify genuinely uses JCS (RFC 8785), not plain
    /// `serde_json::to_vec`: a describe carrying an integer-valued float
    /// (`64.0`) serializes as `64.0` under serde_json but `64` under JCS, so the
    /// two canonicalizers DIVERGE on this document. The store signs with JCS, so
    /// the verify must reconstruct with JCS — the old serde_json reconstruction
    /// would compute different bytes and fail. This guards against a silent
    /// regression back to `serde_json::to_vec`.
    #[test]
    #[serial_test::serial]
    fn verify_describe_signature_uses_jcs_canonicalization() {
        let describe = serde_json::json!({
            "apiVersion": "greentic.ai/v1",
            "kind": "ProviderExtension",
            "runtime": { "memoryLimitMB": 64.0 },
            "metadata": { "id": "router_echo", "version": "1.0.0", "summary": "x" }
        });
        // The two canonicalizers MUST differ on this value, else the test would
        // pass even with a serde_json reconstruction (defeating its purpose).
        assert_ne!(
            serde_json::to_vec(&describe).unwrap(),
            serde_jcs::to_vec(&describe).unwrap(),
            "expected serde_json and JCS to diverge on an integer-valued float"
        );
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let signed_bytes = sign_describe_like_store(&describe, &signing);
        let signed: serde_json::Value = serde_json::from_slice(&signed_bytes).unwrap();
        verify_describe_signature(&signed, &[signing.verifying_key()])
            .expect("store-signed (JCS) describe must verify under the JCS reconstruction");
    }

    #[test]
    #[serial_test::serial]
    fn verify_describe_signature_fails_closed_on_empty_allowlist() {
        let signing = SigningKey::from_bytes(&[4u8; 32]);
        let signed_bytes = sign_describe_like_store(&sample_describe(), &signing);
        let signed: serde_json::Value = serde_json::from_slice(&signed_bytes).unwrap();
        let err = verify_describe_signature(&signed, &[]).unwrap_err();
        assert!(matches!(err, StorePullError::Signature(_)), "got: {err:?}");
    }

    #[test]
    #[serial_test::serial]
    fn verify_describe_signature_rejects_untrusted_key() {
        let signer = SigningKey::from_bytes(&[5u8; 32]);
        let other = SigningKey::from_bytes(&[6u8; 32]);
        let signed_bytes = sign_describe_like_store(&sample_describe(), &signer);
        let signed: serde_json::Value = serde_json::from_slice(&signed_bytes).unwrap();
        let err = verify_describe_signature(&signed, &[other.verifying_key()]).unwrap_err();
        assert!(matches!(err, StorePullError::Signature(_)), "got: {err:?}");
    }

    #[test]
    #[serial_test::serial]
    fn verify_describe_signature_rejects_tampered_describe() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let signed_bytes = sign_describe_like_store(&sample_describe(), &signing);
        let mut signed: serde_json::Value = serde_json::from_slice(&signed_bytes).unwrap();
        // Mutate a signed field after the fact; the signature no longer matches.
        signed["metadata"]["summary"] = serde_json::json!("tampered");
        let err = verify_describe_signature(&signed, &[signing.verifying_key()]).unwrap_err();
        assert!(matches!(err, StorePullError::Signature(_)), "got: {err:?}");
    }

    #[test]
    #[serial_test::serial]
    fn trusted_signers_parses_and_rejects_garbage() {
        let signing = SigningKey::from_bytes(&[8u8; 32]);
        let good = pubkey_env_value(&signing);
        unsafe {
            std::env::set_var(
                TRUSTED_SIGNERS_ENV,
                format!("{good}, , rsa:AAAA, not-base64!!"),
            )
        };
        let keys = trusted_signers();
        unsafe { std::env::remove_var(TRUSTED_SIGNERS_ENV) };
        assert_eq!(keys.len(), 1, "only the one valid ed25519 entry survives");
        assert_eq!(keys[0].to_bytes(), signing.verifying_key().to_bytes());
    }

    // ---- integration tests (require the router_echo fixture wasm) ----

    #[tokio::test]
    #[serial_test::serial]
    async fn ensure_cached_verifies_and_caches_happy_path() {
        let Some(src) = fixture_wasm() else {
            return; // self-skip without the fixture wasm
        };
        let wasm = std::fs::read(&src).unwrap();
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let signed_describe = sign_describe_like_store(&sample_describe(), &signing);
        let archive = build_gtxpack(&signed_describe, &wasm);
        let digest = hex_sha256(&archive);
        let server = mock_store(archive).await;

        let cache = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            std::env::set_var(STORE_URL_ENV, server.uri());
            std::env::set_var(TRUSTED_SIGNERS_ENV, pubkey_env_value(&signing));
        }

        let result = ensure_cached("router_echo", "1.0.0", &digest).await;

        let dest = cache.path().join("router_echo.wasm");
        let cached_ok = dest.exists();
        let cached_bytes = if cached_ok {
            std::fs::read(&dest).ok()
        } else {
            None
        };
        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        result.expect("happy path must verify + cache");
        assert!(cached_ok, "extension.wasm must be written to the cache");
        assert_eq!(cached_bytes.as_deref(), Some(wasm.as_slice()));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn ensure_cached_wrong_digest_errors_and_caches_nothing() {
        let Some(src) = fixture_wasm() else {
            return;
        };
        let wasm = std::fs::read(&src).unwrap();
        let signing = SigningKey::from_bytes(&[12u8; 32]);
        let signed_describe = sign_describe_like_store(&sample_describe(), &signing);
        let archive = build_gtxpack(&signed_describe, &wasm);
        let server = mock_store(archive).await;

        let cache = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            std::env::set_var(STORE_URL_ENV, server.uri());
            std::env::set_var(TRUSTED_SIGNERS_ENV, pubkey_env_value(&signing));
        }

        let wrong_digest = "0".repeat(64);
        let result = ensure_cached("router_echo", "1.0.0", &wrong_digest).await;

        let dest = cache.path().join("router_echo.wasm");
        let leaked = dest.exists();
        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        assert!(
            matches!(result, Err(StorePullError::Integrity(_))),
            "got: {result:?}"
        );
        assert!(!leaked, "nothing must be cached on a digest mismatch");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn ensure_cached_untrusted_signature_errors_and_caches_nothing() {
        let Some(src) = fixture_wasm() else {
            return;
        };
        let wasm = std::fs::read(&src).unwrap();
        let signer = SigningKey::from_bytes(&[13u8; 32]);
        let untrusted = SigningKey::from_bytes(&[14u8; 32]);
        let signed_describe = sign_describe_like_store(&sample_describe(), &signer);
        let archive = build_gtxpack(&signed_describe, &wasm);
        let digest = hex_sha256(&archive);
        let server = mock_store(archive).await;

        let cache = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            std::env::set_var(STORE_URL_ENV, server.uri());
            // Trust a DIFFERENT key than the one that signed the describe.
            std::env::set_var(TRUSTED_SIGNERS_ENV, pubkey_env_value(&untrusted));
        }

        let result = ensure_cached("router_echo", "1.0.0", &digest).await;

        let dest = cache.path().join("router_echo.wasm");
        let leaked = dest.exists();
        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        assert!(
            matches!(result, Err(StorePullError::Signature(_))),
            "got: {result:?}"
        );
        assert!(!leaked, "an untrusted signature must cache nothing");
    }

    // ---- Fix 1 regression tests (fixture-independent) ----

    /// Pre-seed a sidecar-less `<ref>.wasm` and confirm that `ensure_cached`
    /// performs a full re-pull + writes the sidecar, proving the fail-open gap is
    /// closed: a wasm without its companion `.sha256` is not treated as verified.
    #[tokio::test]
    #[serial_test::serial]
    async fn wasm_without_sidecar_triggers_repull() {
        let signing = SigningKey::from_bytes(&[42u8; 32]);
        // Use minimal fake wasm bytes — ensure_cached never executes them.
        let fake_wasm = b"fake-wasm-bytes-for-sidecar-test";
        let signed_describe = sign_describe_like_store(&sample_describe(), &signing);
        let archive = build_gtxpack(&signed_describe, fake_wasm);
        let digest = hex_sha256(&archive);

        let cache = tempfile::tempdir().unwrap();
        // Pre-seed the wasm WITHOUT its sidecar — simulates operator-seeded or
        // stale entry from a previous ref that never completed verification.
        let dest = cache.path().join("router_echo.wasm");
        std::fs::write(&dest, b"stale-unverified-bytes").unwrap();
        assert!(dest.exists(), "pre-condition: wasm exists");
        assert!(
            !sidecar_path(&dest).exists(),
            "pre-condition: sidecar absent"
        );

        let server = mock_store(archive).await;
        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            std::env::set_var(STORE_URL_ENV, server.uri());
            std::env::set_var(TRUSTED_SIGNERS_ENV, pubkey_env_value(&signing));
        }

        let result = ensure_cached("router_echo", "1.0.0", &digest).await;

        let sidecar_exists = sidecar_path(&dest).exists();
        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        result.expect("re-pull of sidecar-less wasm must succeed");
        assert!(
            sidecar_exists,
            "sidecar must be written after verified re-pull"
        );
    }

    /// When the wasm, its sidecar, AND a gtxpack marker MATCHING the requested
    /// digest all exist, `ensure_cached` short-circuits without hitting the
    /// network. No mock server is set up, so any network attempt would error,
    /// proving the short-circuit actually happened.
    #[tokio::test]
    #[serial_test::serial]
    async fn cache_with_matching_marker_short_circuits() {
        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join("router_echo.wasm");
        // Seed wasm + sidecar + a marker matching the digest we will request.
        std::fs::write(&dest, b"cached-wasm").unwrap();
        std::fs::write(sidecar_path(&dest), b"somedigest").unwrap();
        std::fs::write(gtxpack_marker_path(&dest), b"pinneddigest").unwrap();

        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            // Bogus store URL — any network attempt would fail.
            std::env::set_var(STORE_URL_ENV, "http://127.0.0.1:1");
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        let result = ensure_cached("router_echo", "1.0.0", "pinneddigest").await;

        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
        }

        result.expect("wasm + sidecar + matching marker must short-circuit with Ok");
    }

    /// Regression for the cache-staleness bug: when the cached wasm's marker
    /// records a DIFFERENT (older) gtxpack digest than the one now requested —
    /// i.e. the component was upgraded/re-registered — `ensure_cached` must NOT
    /// short-circuit on the stale wasm but re-pull and replace it, so the new
    /// (e.g. security-patched) version actually reaches the runner.
    #[tokio::test]
    #[serial_test::serial]
    async fn stale_marker_triggers_repull_on_upgrade() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let new_wasm = b"new-verified-wasm-bytes";
        let signed_describe = sign_describe_like_store(&sample_describe(), &signing);
        let archive = build_gtxpack(&signed_describe, new_wasm);
        let new_digest = hex_sha256(&archive);

        let cache = tempfile::tempdir().unwrap();
        let dest = cache.path().join("router_echo.wasm");
        // Pre-seed a COMPLETE older cache entry: wasm + sidecar + a marker that
        // records the OLD digest (different from `new_digest`).
        std::fs::write(&dest, b"old-cached-wasm").unwrap();
        std::fs::write(sidecar_path(&dest), hex_sha256(b"old-cached-wasm")).unwrap();
        std::fs::write(gtxpack_marker_path(&dest), b"0000oldolddigest").unwrap();

        let server = mock_store(archive).await;
        unsafe {
            std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", cache.path());
            std::env::set_var(STORE_URL_ENV, server.uri());
            std::env::set_var(TRUSTED_SIGNERS_ENV, pubkey_env_value(&signing));
        }

        // Same version path (the mock serves 1.0.0) but a DIFFERENT digest: the
        // staleness check keys on the pinned digest (the marker), not the URL
        // version, so a changed digest alone must force the re-pull.
        let result = ensure_cached("router_echo", "1.0.0", &new_digest).await;

        let cached_after = std::fs::read(&dest).unwrap_or_default();
        let marker_after = std::fs::read_to_string(gtxpack_marker_path(&dest)).unwrap_or_default();
        unsafe {
            std::env::remove_var("GREENTIC_MCP_LOCAL_CACHE_DIR");
            std::env::remove_var(STORE_URL_ENV);
            std::env::remove_var(TRUSTED_SIGNERS_ENV);
        }

        result.expect("stale-marker cache must re-pull the upgraded component");
        assert_eq!(
            cached_after, new_wasm,
            "the upgraded wasm must replace the stale one"
        );
        assert!(
            marker_after.trim().eq_ignore_ascii_case(&new_digest),
            "the marker must be rewritten to the new pinned digest"
        );
    }
}
