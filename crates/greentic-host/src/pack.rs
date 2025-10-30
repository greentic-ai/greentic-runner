use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::HostConfig;
use crate::verify;

pub struct PackRuntime {
    path: PathBuf,
    config: Arc<HostConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDescriptor {
    pub id: String,
    #[serde(rename = "type")]
    pub flow_type: String,
    #[serde(default)]
    pub description: Option<String>,
}

impl PackRuntime {
    pub async fn load(path: impl AsRef<Path>, config: Arc<HostConfig>) -> Result<Self> {
        let path = path.as_ref();
        verify::verify_pack(path).await?;
        tracing::info!(pack_path = %path.display(), "pack verification complete");
        Ok(Self {
            path: path.to_path_buf(),
            config,
        })
    }

    pub async fn list_flows(&self) -> Result<Vec<FlowDescriptor>> {
        tracing::trace!(
            tenant = %self.config.tenant,
            pack_path = %self.path.display(),
            "listing flows from pack"
        );
        // TODO: call into pack-export world to retrieve flows
        Ok(vec![])
    }

    pub async fn run_flow(
        &self,
        _flow_id: &str,
        _input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // TODO: dispatch flow execution via Wasmtime
        Ok(serde_json::json!({}))
    }
}
