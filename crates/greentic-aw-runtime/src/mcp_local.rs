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

/// Build an `ExecConfig` over the local cache dir.
///
/// Phase 1 trusts the cache contents (`allow_unverified: true`); signature
/// pinning is added when the store-pull path lands (later phase) and supplies
/// `required_digests`/`trusted_signers`.
///
/// TODO(phase-2): flip `allow_unverified` to signature pinning BEFORE
/// `local-wasm` rows become registerable through admin. This unverified posture
/// is only safe today because Phase 1 ships no admin path to register a
/// `local-wasm` server — the cache is operator-seeded — so untrusted code
/// cannot reach this executor in production until verification lands.
fn exec_config() -> ExecConfig {
    ExecConfig {
        store: ToolStore::LocalDir(cache_dir()),
        security: VerifyPolicy {
            allow_unverified: true,
            required_digests: HashMap::new(),
            trusted_signers: Vec::new(),
        },
        runtime: RuntimePolicy::default(),
        // Router tools commonly wrap REST/HTTP APIs (e.g. the generated
        // OpenAPI routers), so the component is granted outbound HTTP. "No HTTP
        // hop" refers to the host<->MCP transport, not the tool's own egress.
        http_enabled: true,
        secrets_store: None,
    }
}

/// List a local component's tools. Returns an empty vec and emits a `warn` on any failure.
pub async fn local_list_tools(component_ref: &str) -> Vec<ToolDef> {
    let component = component_ref.to_string();
    let res = tokio::task::spawn_blocking(move || list_tools(&component, &exec_config())).await;
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
    let req = ExecRequest {
        component,
        action,
        args: cloned_args,
        tenant: None,
    };
    let res = tokio::task::spawn_blocking(move || exec(req, &exec_config())).await;
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
