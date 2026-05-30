//! Bundle SQL config — the `sql:` block in the bundle's demo config.
//! Filled in fully by Task 1.

use serde::Deserialize;

/// Database engine kind for a configured connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Mysql,
    Sqlite,
}
