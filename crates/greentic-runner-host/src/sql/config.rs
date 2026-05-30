//! Bundle SQL config (the `sql:` block) — pure, host-testable.

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    Sqlite,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub engine: Engine,
    /// Secret URI holding the DSN/connection string for this connection.
    pub dsn_secret: String,
    #[serde(default = "default_true")]
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct SqlConfig {
    /// Secret URI holding the shared bearer token the extension must present.
    pub auth_token_secret: String,
    #[serde(default)]
    pub connections: std::collections::BTreeMap<String, ConnectionConfig>,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sql_block() {
        let yaml = r#"
auth_token_secret: secret://sql/gateway_token
connections:
  analytics:
    engine: postgres
    dsn_secret: secret://sql/analytics/dsn
    read_only: true
  app:
    engine: sqlite
    dsn_secret: secret://sql/app/dsn
"#;
        let cfg: SqlConfig = serde_yaml_bw::from_str(yaml).unwrap();
        assert_eq!(cfg.connections.len(), 2);
        assert_eq!(cfg.connections["analytics"].engine, Engine::Postgres);
        assert!(cfg.connections["app"].read_only);
    }
}
