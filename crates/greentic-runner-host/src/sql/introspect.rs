//! Per-engine schema introspection → the gateway `Schema` shape.
//! Full implementation is added by Task 3.

use serde::Serialize;

/// A single column in a database table.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
}

/// A database table with its columns.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

/// The introspected schema for one database connection, ready to serialize
/// for the `/sql/<conn>/schema` response contract.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Schema {
    pub engine: String,
    pub tables: Vec<Table>,
}
