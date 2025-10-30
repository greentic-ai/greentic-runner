use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use greentic_mcp::{ExecConfig, RuntimePolicy, ToolStore, VerifyPolicy};
use serde::Deserialize;
use serde_yaml_bw as serde_yaml;

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub tenant: String,
    pub bindings_path: PathBuf,
    pub flow_type_bindings: HashMap<String, FlowBinding>,
    pub mcp: McpConfig,
    pub rate_limits: RateLimits,
    pub http_enabled: bool,
    pub secrets_policy: SecretsPolicy,
    pub webhook_policy: WebhookPolicy,
    pub timers: Vec<TimerBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BindingsFile {
    pub tenant: String,
    #[serde(default)]
    pub flow_type_bindings: HashMap<String, FlowBinding>,
    pub mcp: McpConfig,
    #[serde(default)]
    pub rate_limits: RateLimits,
    #[serde(default)]
    pub timers: Vec<TimerBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlowBinding {
    pub adapter: String,
    #[serde(default)]
    pub config: serde_yaml::Value,
    #[serde(default)]
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpConfig {
    pub store: serde_yaml::Value,
    #[serde(default)]
    pub security: serde_yaml::Value,
    #[serde(default)]
    pub runtime: serde_yaml::Value,
    #[serde(default)]
    pub http_enabled: Option<bool>,
    #[serde(default)]
    pub retry: Option<McpRetryConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimits {
    #[serde(default = "default_messaging_qps")]
    pub messaging_send_qps: u32,
    #[serde(default = "default_messaging_burst")]
    pub messaging_burst: u32,
}

#[derive(Debug, Clone)]
pub struct SecretsPolicy {
    allowed: HashSet<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpRetryConfig {
    #[serde(default = "default_mcp_retry_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_mcp_retry_base_delay_ms")]
    pub base_delay_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct WebhookPolicy {
    allow_paths: Vec<String>,
    deny_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebhookBindingConfig {
    #[serde(default)]
    pub allow_paths: Vec<String>,
    #[serde(default)]
    pub deny_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TimerBinding {
    pub flow_id: String,
    pub cron: String,
    #[serde(default)]
    pub schedule_id: Option<String>,
}

impl HostConfig {
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read bindings file {:?}", path))?;
        let bindings: BindingsFile = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse bindings file {:?}", path))?;

        let secrets_policy = SecretsPolicy::from_bindings(&bindings);
        let http_enabled = bindings
            .mcp
            .http_enabled
            .unwrap_or(bindings.flow_type_bindings.contains_key("messaging"));
        let webhook_policy = bindings
            .flow_type_bindings
            .get("webhook")
            .and_then(|binding| {
                serde_yaml::from_value::<WebhookBindingConfig>(binding.config.clone())
                    .map(WebhookPolicy::from)
                    .map_err(|err| {
                        tracing::warn!(error = %err, "failed to parse webhook binding config");
                        err
                    })
                    .ok()
            })
            .unwrap_or_default();

        Ok(Self {
            tenant: bindings.tenant.clone(),
            bindings_path: path.to_path_buf(),
            flow_type_bindings: bindings.flow_type_bindings.clone(),
            mcp: bindings.mcp.clone(),
            rate_limits: bindings.rate_limits.clone(),
            http_enabled,
            secrets_policy,
            webhook_policy,
            timers: bindings.timers.clone(),
        })
    }

    pub fn messaging_binding(&self) -> Option<&FlowBinding> {
        self.flow_type_bindings.get("messaging")
    }

    pub fn mcp_retry_config(&self) -> McpRetryConfig {
        self.mcp.retry.clone().unwrap_or_default()
    }

    pub fn mcp_exec_config(&self) -> Result<ExecConfig> {
        self.mcp
            .to_exec_config(self.bindings_path.parent())
            .context("failed to build MCP exec configuration")
    }
}

impl SecretsPolicy {
    fn from_bindings(bindings: &BindingsFile) -> Self {
        let allowed = bindings
            .flow_type_bindings
            .values()
            .flat_map(|binding| binding.secrets.iter().cloned())
            .collect::<HashSet<_>>();
        Self { allowed }
    }

    pub fn is_allowed(&self, key: &str) -> bool {
        self.allowed.contains(key)
    }
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            messaging_send_qps: default_messaging_qps(),
            messaging_burst: default_messaging_burst(),
        }
    }
}

fn default_messaging_qps() -> u32 {
    10
}

fn default_messaging_burst() -> u32 {
    20
}

impl From<WebhookBindingConfig> for WebhookPolicy {
    fn from(value: WebhookBindingConfig) -> Self {
        Self {
            allow_paths: value.allow_paths,
            deny_paths: value.deny_paths,
        }
    }
}

impl WebhookPolicy {
    pub fn is_allowed(&self, path: &str) -> bool {
        if self
            .deny_paths
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            return false;
        }

        if self.allow_paths.is_empty() {
            return true;
        }

        self.allow_paths
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }
}

impl TimerBinding {
    pub fn schedule_id(&self) -> &str {
        self.schedule_id.as_deref().unwrap_or(self.flow_id.as_str())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum StoreBinding {
    #[serde(rename = "http-single")]
    HttpSingle {
        name: String,
        url: String,
        #[serde(default)]
        cache_dir: Option<String>,
    },
    #[serde(rename = "local-dir")]
    LocalDir { path: String },
}

#[derive(Debug, Default, Deserialize)]
struct RuntimeBinding {
    #[serde(default)]
    max_memory_mb: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    fuel: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct SecurityBinding {
    #[serde(default)]
    require_signature: bool,
    #[serde(default)]
    required_digests: HashMap<String, String>,
    #[serde(default)]
    trusted_signers: Vec<String>,
}

impl McpConfig {
    fn to_exec_config(&self, base_dir: Option<&Path>) -> Result<ExecConfig> {
        let store_cfg: StoreBinding = serde_yaml::from_value(self.store.clone())
            .context("invalid MCP store configuration")?;
        let runtime_cfg: RuntimeBinding =
            serde_yaml::from_value(self.runtime.clone()).unwrap_or_default();
        let security_cfg: SecurityBinding =
            serde_yaml::from_value(self.security.clone()).unwrap_or_default();

        let store = match store_cfg {
            StoreBinding::HttpSingle {
                name,
                url,
                cache_dir,
            } => ToolStore::HttpSingleFile {
                name,
                url,
                cache_dir: resolve_optional_path(base_dir, cache_dir)
                    .unwrap_or_else(|| default_cache_dir(base_dir)),
            },
            StoreBinding::LocalDir { path } => {
                ToolStore::LocalDir(resolve_required_path(base_dir, path))
            }
        };

        let runtime = RuntimePolicy {
            fuel: runtime_cfg.fuel,
            max_memory: runtime_cfg.max_memory_mb.map(|mb| mb * 1024 * 1024),
            wallclock_timeout: Duration::from_millis(runtime_cfg.timeout_ms.unwrap_or(30_000)),
        };

        let security = VerifyPolicy {
            allow_unverified: !security_cfg.require_signature,
            required_digests: security_cfg.required_digests,
            trusted_signers: security_cfg.trusted_signers,
        };

        Ok(ExecConfig {
            store,
            security,
            runtime,
            http_enabled: self.http_enabled.unwrap_or(false),
        })
    }
}

impl Default for McpRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_mcp_retry_attempts(),
            base_delay_ms: default_mcp_retry_base_delay_ms(),
        }
    }
}

fn default_mcp_retry_attempts() -> u32 {
    3
}

fn default_mcp_retry_base_delay_ms() -> u64 {
    250
}

fn resolve_required_path(base: Option<&Path>, value: String) -> PathBuf {
    let candidate = PathBuf::from(&value);
    if candidate.is_absolute() {
        candidate
    } else if let Some(base) = base {
        base.join(candidate)
    } else {
        PathBuf::from(value)
    }
}

fn resolve_optional_path(base: Option<&Path>, value: Option<String>) -> Option<PathBuf> {
    value.map(|v| resolve_required_path(base, v))
}

fn default_cache_dir(base: Option<&Path>) -> PathBuf {
    if let Some(dir) = env::var_os("GREENTIC_CACHE_DIR") {
        PathBuf::from(dir)
    } else if let Some(base) = base {
        base.join(".greentic/tool-cache")
    } else {
        env::temp_dir().join("greentic-tool-cache")
    }
}
