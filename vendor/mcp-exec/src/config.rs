//! Configuration primitives describing how the executor resolves, verifies, and
//! runs Wasm components.

use std::collections::HashMap;
use std::time::Duration;

use crate::store::ToolStore;

/// Configuration for a single executor invocation.
#[derive(Clone, Debug)]
pub struct ExecConfig {
    pub store: ToolStore,
    pub security: VerifyPolicy,
    pub runtime: RuntimePolicy,
    pub http_enabled: bool,
}

/// Policy describing how artifacts must be verified prior to execution.
#[derive(Clone, Debug, Default)]
pub struct VerifyPolicy {
    /// Whether artifacts without a matching digest/signature are still allowed.
    pub allow_unverified: bool,
    /// Expected digests (hex encoded) keyed by component identifier.
    pub required_digests: HashMap<String, String>,
    /// Signers that are trusted to vouch for artifacts.
    pub trusted_signers: Vec<String>,
}

/// Runtime resource limits applied to the Wasm execution.
#[derive(Clone, Debug)]
pub struct RuntimePolicy {
    pub fuel: Option<u64>,
    pub max_memory: Option<u64>,
    pub wallclock_timeout: Duration,
}

impl Default for RuntimePolicy {
    fn default() -> Self {
        Self {
            fuel: None,
            max_memory: None,
            wallclock_timeout: Duration::from_secs(30),
        }
    }
}
