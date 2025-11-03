//! Executor library for loading and running `wasix:mcp` compatible Wasm components.
//! Users supply an [`ExecConfig`] describing how to resolve artifacts and what
//! runtime constraints to enforce, then call [`exec`] with a structured request.

mod config;
pub mod describe;
mod error;
mod resolve;
mod runner;
mod store;
mod verify;

pub use config::{ExecConfig, RuntimePolicy, VerifyPolicy};
pub use error::ExecError;
pub use store::{ToolInfo, ToolStore};

use greentic_types::TenantCtx;
use serde_json::Value;

use crate::error::RunnerError;
use crate::runner::Runner;

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub component: String,
    pub action: String,
    pub args: Value,
    pub tenant: Option<TenantCtx>,
}

/// Execute a single action exported by an MCP component.
///
/// Resolution, verification, and runtime enforcement are performed in sequence,
/// with detailed errors surfaced through [`ExecError`].
pub fn exec(req: ExecRequest, cfg: &ExecConfig) -> Result<Value, ExecError> {
    let resolved = resolve::resolve(&req.component, &cfg.store)
        .map_err(|err| ExecError::resolve(&req.component, err))?;

    let verified = verify::verify(&req.component, resolved, &cfg.security)
        .map_err(|err| ExecError::verification(&req.component, err))?;

    let runner = runner::DefaultRunner::new(&cfg.runtime)
        .map_err(|err| ExecError::runner(&req.component, err))?;

    let result = runner.run(
        &req,
        &verified,
        runner::ExecutionContext {
            runtime: &cfg.runtime,
            http_enabled: cfg.http_enabled,
            tenant: req.tenant.as_ref(),
        },
    );

    let value = match result {
        Ok(v) => v,
        Err(RunnerError::ActionNotFound { .. }) => {
            return Err(ExecError::not_found(
                req.component.clone(),
                req.action.clone(),
            ));
        }
        Err(err) => return Err(ExecError::runner(&req.component, err)),
    };

    if let Some(code) = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if code == "iface-error.not-found" {
            return Err(ExecError::not_found(req.component, req.action));
        } else {
            return Err(ExecError::tool_error(
                req.component,
                req.action,
                code,
                value,
            ));
        }
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RuntimePolicy, VerifyPolicy};
    use crate::error::RunnerError;
    use crate::store::ToolStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::verify::VerifiedArtifact;

    #[derive(Default)]
    struct MockRunner;

    impl runner::Runner for MockRunner {
        fn run(
            &self,
            request: &ExecRequest,
            artifact: &VerifiedArtifact,
            _ctx: runner::ExecutionContext<'_>,
        ) -> Result<Value, RunnerError> {
            let mut payload = request.args.clone();
            if let Value::Object(map) = &mut payload {
                map.insert(
                    "component_digest".to_string(),
                    Value::String(artifact.resolved.digest.clone()),
                );
            }
            Ok(payload)
        }
    }

    #[test]
    fn local_resolve_and_verify_success() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let wasm_path = tempdir.path().join("echo.component.wasm");
        std::fs::write(&wasm_path, b"fake wasm contents").expect("write");

        let digest = crate::resolve::resolve(
            "echo.component",
            &ToolStore::LocalDir(PathBuf::from(tempdir.path())),
        )
        .expect("resolve")
        .digest;

        let mut required = HashMap::new();
        required.insert("echo.component".to_string(), digest.clone());

        let cfg = ExecConfig {
            store: ToolStore::LocalDir(PathBuf::from(tempdir.path())),
            security: VerifyPolicy {
                allow_unverified: false,
                required_digests: required,
                trusted_signers: Vec::new(),
            },
            runtime: RuntimePolicy::default(),
            http_enabled: false,
        };

        let req = ExecRequest {
            component: "echo.component".into(),
            action: "noop".into(),
            args: json!({"message": "hello"}),
            tenant: None,
        };

        // Inject our mock runner to exercise pipeline without executing wasm.
        let resolved =
            crate::resolve::resolve(&req.component, &cfg.store).expect("resolve second time");
        let verified =
            crate::verify::verify(&req.component, resolved, &cfg.security).expect("verify");
        let result = MockRunner
            .run(
                &req,
                &verified,
                runner::ExecutionContext {
                    runtime: &cfg.runtime,
                    http_enabled: cfg.http_enabled,
                    tenant: req.tenant.as_ref(),
                },
            )
            .expect("run");

        assert_eq!(
            result.get("component_digest").and_then(Value::as_str),
            Some(digest.as_str())
        );
    }
}
