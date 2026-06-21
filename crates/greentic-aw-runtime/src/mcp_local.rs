//! Local (in-process) `wasix:mcp` execution for the `local-wasm` MCP transport.
//!
//! Resolves a component from a versioned on-disk cache and runs its
//! `list-tools` / `call-tool` through `greentic-mcp-exec` (Wasmtime). The
//! executor is synchronous, so every entry point hops onto `spawn_blocking`.
//! Both functions are infallible by the rail's contract: list degrades to empty
//! with a `warn`; call returns `{"error": ...}` on any failure.

use std::collections::HashMap;
use std::path::PathBuf;

use greentic_mcp_exec::{
    ExecConfig, ExecRequest, RuntimePolicy, ToolDef, ToolStore, VerifyPolicy, exec, list_tools,
};
use serde_json::{Value, json};

/// Directory holding cached local MCP `*.wasm` files.
///
/// Resolution order:
/// 1. `GREENTIC_MCP_LOCAL_CACHE_DIR` env var (explicit override).
/// 2. `$GREENTIC_EXTENSIONS_DIR/mcp-local` (standard extensions hierarchy).
/// 3. `./.mcp-local` (process-local fallback).
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GREENTIC_MCP_LOCAL_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(root) = std::env::var("GREENTIC_EXTENSIONS_DIR") {
        return PathBuf::from(root).join("mcp-local");
    }
    PathBuf::from(".mcp-local")
}

/// Build an `ExecConfig` for a component, pinning the wasm digest when a
/// sidecar `<ref>.wasm.sha256` exists in the cache.
///
/// **Verified path** (store-pull, Task 2 has run): the sidecar exists, so the
/// config sets `allow_unverified: false` and `required_digests = {component_ref
/// => sha256(extension.wasm)}`. The key is the file stem — the exact string
/// mcp-exec uses when it resolves `component_ref.wasm` from the local store.
///
/// **Fallback path** (operator-seeded Phase-1 cache, no sidecar): the config
/// sets `allow_unverified: true` with an empty `required_digests`. This
/// preserves the Phase-1 behavior for caches that were seeded directly without
/// going through the store-pull path.
pub(crate) fn exec_config_for(component_ref: &str) -> ExecConfig {
    let pinned_digest = read_pinned_digest(component_ref);
    let security = if let Some(wasm_digest) = pinned_digest {
        let mut required_digests = HashMap::new();
        required_digests.insert(component_ref.to_string(), wasm_digest);
        VerifyPolicy {
            allow_unverified: false,
            required_digests,
            trusted_signers: Vec::new(),
        }
    } else {
        // No sidecar: operator-seeded cache; maintain Phase-1 trust posture.
        VerifyPolicy {
            allow_unverified: true,
            required_digests: HashMap::new(),
            trusted_signers: Vec::new(),
        }
    };
    ExecConfig {
        store: ToolStore::LocalDir(cache_dir()),
        security,
        runtime: RuntimePolicy::default(),
        // Router tools commonly wrap REST/HTTP APIs (e.g. the generated
        // OpenAPI routers), so the component is granted outbound HTTP. "No HTTP
        // hop" refers to the host<->MCP transport, not the tool's own egress.
        http_enabled: true,
        secrets_store: None,
    }
}

/// Read the pinned wasm digest from the sidecar file `<cache>/<ref>.wasm.sha256`.
///
/// Returns `Some(hex_digest)` when the sidecar exists and is readable UTF-8;
/// `None` when the sidecar is absent (operator-seeded cache) or unreadable.
fn read_pinned_digest(component_ref: &str) -> Option<String> {
    let wasm_dest = cache_dir().join(format!("{component_ref}.wasm"));
    let sidecar = crate::mcp_store_pull::sidecar_path(&wasm_dest);
    std::fs::read_to_string(&sidecar)
        .ok()
        .map(|content| content.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// List a local component's tools. Returns an empty vec and emits a `warn` on any failure.
pub async fn local_list_tools(component_ref: &str) -> Vec<ToolDef> {
    let component = component_ref.to_string();
    let config = exec_config_for(component_ref);
    let res = tokio::task::spawn_blocking(move || list_tools(&component, &config)).await;
    match res {
        Ok(Ok(tools)) => tools,
        Ok(Err(e)) => {
            tracing::warn!(
                component = %component_ref,
                error = %e,
                "local mcp list_tools failed; skipping"
            );
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(
                component = %component_ref,
                error = %e,
                "local mcp list_tools task panicked; skipping"
            );
            Vec::new()
        }
    }
}

/// Call a local component's tool. Returns `{"error": ...}` on any failure; never panics.
pub async fn local_call_tool(component_ref: &str, tool: &str, args: &Value) -> Value {
    let component = component_ref.to_string();
    let action = tool.to_string();
    let cloned_args = args.clone();
    let config = exec_config_for(component_ref);
    let req = ExecRequest {
        component,
        action,
        args: cloned_args,
        tenant: None,
    };
    let res = tokio::task::spawn_blocking(move || exec(req, &config)).await;
    match res {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => json!({ "error": format!("local mcp call failed: {e}") }),
        Err(e) => json!({ "error": format!("local mcp call panicked: {e}") }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
mod tests {
    use super::*;

    /// Resolve a built router_echo wasm if present; else return None (test self-skips).
    fn fixture_wasm() -> Option<std::path::PathBuf> {
        let p = std::env::var("GREENTIC_MCP_ROUTER_ECHO_WASM")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../greentic-mcp/target/wasm32-wasip2/release/router_echo.wasm")
            });
        p.exists().then_some(p)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn local_call_tool_runs_in_process() {
        let Some(src) = fixture_wasm() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // Safety: serial attribute ensures no concurrent env-var mutation between
        // the two tests that share GREENTIC_MCP_LOCAL_CACHE_DIR.
        unsafe { std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", dir.path()) };
        std::fs::copy(&src, dir.path().join("router_echo.wasm")).unwrap();

        let out = local_call_tool("router_echo", "echo", &serde_json::json!({"message": "hi"}))
            .await;
        assert!(!out.to_string().contains("\"error\""), "got: {out}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn local_call_tool_missing_component_returns_error_value() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("GREENTIC_MCP_LOCAL_CACHE_DIR", dir.path()) };
        let out = local_call_tool("nope", "echo", &serde_json::json!({})).await;
        assert!(out.to_string().contains("error"), "got: {out}");
    }
}
