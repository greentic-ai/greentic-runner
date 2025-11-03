use anyhow::Result;
use serde_json::Value;

use crate::{exec, ExecConfig, ExecError, ExecRequest};

#[derive(Debug)]
pub enum Maybe<T> {
    Data(T),
    Unsupported,
}

#[derive(Debug)]
pub struct ToolDescribe {
    pub capabilities: Maybe<Vec<String>>,
    pub secrets: Maybe<Value>,
    pub config_schema: Maybe<Value>,
}

pub fn describe_tool(name: &str, cfg: &ExecConfig) -> Result<ToolDescribe> {
    fn try_action(name: &str, action: &str, cfg: &ExecConfig) -> Result<Maybe<Value>> {
        let req = ExecRequest {
            component: name.to_string(),
            action: action.to_string(),
            args: Value::Object(Default::default()),
            tenant: None,
        };

        match exec(req, cfg) {
            Ok(v) => Ok(Maybe::Data(v)),
            Err(ExecError::NotFound { .. }) => Ok(Maybe::Unsupported),
            Err(ExecError::Tool { code, payload, .. }) if code == "iface-error.not-found" => {
                let _ = payload;
                Ok(Maybe::Unsupported)
            }
            Err(e) => Err(e.into()),
        }
    }

    let capabilities_value = try_action(name, "capabilities", cfg)?;
    let secrets = try_action(name, "list_secrets", cfg)?;
    let config_schema = try_action(name, "config_schema", cfg)?;

    let capabilities = match capabilities_value {
        Maybe::Data(value) => {
            if let Some(arr) = value.as_array() {
                let list = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>();
                Maybe::Data(list)
            } else {
                Maybe::Data(Vec::new())
            }
        }
        Maybe::Unsupported => Maybe::Unsupported,
    };

    Ok(ToolDescribe {
        capabilities,
        secrets,
        config_schema,
    })
}
