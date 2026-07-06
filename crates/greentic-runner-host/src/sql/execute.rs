//! Read-only query execution with row limit and JSON row mapping.
//!
//! # Row→JSON strategy (per engine)
//!
//! For each column we attempt type-decode in preference order. A type-mismatch
//! `Err` from `try_get` means "try the next type"; `Ok(None)` means SQL NULL →
//! `Value::Null`.
//!
//! ## SQLite
//! `Option<i64>` → `Option<f64>` → `Option<String>` → `Option<Vec<u8>>`
//! (rendered as lossy UTF-8 or base64 when not valid UTF-8) → `Value::Null`.
//!
//! ## Postgres
//! `Option<i64>` → `Option<f64>` → `Option<bool>` → `Option<serde_json::Value>`
//! (json/jsonb columns decoded natively via the `json` sqlx feature) →
//! `Option<String>` → `Value::Null`.
//!
//! ## MySQL
//! `Option<i64>` → `Option<f64>` → `Option<bool>` → `Option<String>` →
//! `Option<Vec<u8>>` (lossy UTF-8) → `Value::Null`.
//!
//! # Empty-result column extraction (known v1 limitation)
//!
//! Column names are extracted from each row via `row.columns()`. When the
//! result set is empty (zero rows), we return `columns: vec![]`. The upstream
//! caller (`/sql/<conn>/query`) documents this limitation; a future revision
//! can use `sqlx::describe` (compile-time or runtime) to get column metadata
//! without fetching rows.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;
use sqlx::{Column, Row};

use crate::sql::config::Engine;
use crate::sql::pool::ConnectionPool;

// ── Public API ───────────────────────────────────────────────────────────────

/// The result of a read-only SQL query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QueryResult {
    /// Column names in SELECT order. Empty when the result set has zero rows
    /// (v1 limitation — see module docs).
    pub columns: Vec<String>,
    /// Each element is one row; each inner element is the JSON-mapped cell.
    pub rows: Vec<Vec<Value>>,
    /// Number of rows returned (after the cap is applied).
    pub row_count: u64,
    /// `true` when the result set was larger than `max_rows` and was trimmed.
    pub truncated: bool,
}

/// Execute `sql` read-only against `pool` (which must match `engine`),
/// returning at most `max_rows` rows mapped to JSON.
///
/// - **SQLite** pools are already opened read-only (Task 2); we fetch directly.
/// - **Postgres / MySQL**: we wrap the query in a `SET TRANSACTION READ ONLY`
///   transaction and always roll back, ensuring no writes can persist.
///
/// Returns `Err(String)` on any database or SQL error.
pub async fn run(
    engine: Engine,
    pool: &ConnectionPool,
    sql: &str,
    max_rows: u32,
) -> Result<QueryResult, String> {
    let cap = u64::from(max_rows);
    match (engine, pool) {
        (Engine::Sqlite, ConnectionPool::Sqlite(p)) => {
            let rows = sqlx::query(sql).fetch_all(p).await.map_err(map_err)?;
            Ok(map_rows_sqlite(rows, cap))
        }
        (Engine::Postgres, ConnectionPool::Postgres(p)) => {
            let mut tx = p.begin().await.map_err(map_err)?;
            sqlx::query("SET TRANSACTION READ ONLY")
                .execute(&mut *tx)
                .await
                .map_err(map_err)?;
            let rows = sqlx::query(sql)
                .fetch_all(&mut *tx)
                .await
                .map_err(map_err)?;
            tx.rollback().await.ok();
            Ok(map_rows_pg(rows, cap))
        }
        (Engine::Mysql, ConnectionPool::Mysql(p)) => {
            let mut conn = p.acquire().await.map_err(map_err)?;
            sqlx::query("START TRANSACTION READ ONLY")
                .execute(&mut *conn)
                .await
                .map_err(map_err)?;
            let rows = sqlx::query(sql)
                .fetch_all(&mut *conn)
                .await
                .map_err(map_err)?;
            sqlx::query("ROLLBACK").execute(&mut *conn).await.ok();
            Ok(map_rows_mysql(rows, cap))
        }
        _ => Err("engine/pool mismatch".to_string()),
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn map_err(error: sqlx::Error) -> String {
    format!("query error: {error}")
}

/// Trim `rows` to at most `cap` elements; return `(kept, truncated)`.
fn apply_cap<T>(mut rows: Vec<T>, cap: u64) -> (Vec<T>, bool) {
    let len = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let truncated = len > cap;
    if truncated {
        rows.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    }
    (rows, truncated)
}

// ── SQLite ───────────────────────────────────────────────────────────────────

fn map_rows_sqlite(rows: Vec<sqlx::sqlite::SqliteRow>, cap: u64) -> QueryResult {
    let (rows, truncated) = apply_cap(rows, cap);
    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);

    let columns: Vec<String> = rows
        .first()
        .map(|row| {
            row.columns()
                .iter()
                .map(|col| col.name().to_string())
                .collect()
        })
        .unwrap_or_default();

    let mapped_rows = rows
        .iter()
        .map(|row| {
            (0..row.columns().len())
                .map(|index| sqlite_cell_to_json(row, index))
                .collect()
        })
        .collect();

    QueryResult {
        columns,
        rows: mapped_rows,
        row_count,
        truncated,
    }
}

/// Map a single SQLite cell to a `serde_json::Value`.
///
/// Try-order: `i64` → `f64` → `String` → `Vec<u8>` (lossy UTF-8 or base64) → Null.
fn sqlite_cell_to_json(row: &sqlx::sqlite::SqliteRow, index: usize) -> Value {
    // i64
    if let Ok(Some(integer_value)) = row.try_get::<Option<i64>, _>(index) {
        return Value::Number(integer_value.into());
    }
    // f64
    if let Ok(Some(float_value)) = row.try_get::<Option<f64>, _>(index) {
        if let Some(json_number) = serde_json::Number::from_f64(float_value) {
            return Value::Number(json_number);
        }
        return Value::Null; // NaN / Inf — not representable in JSON
    }
    // String (TEXT affinity)
    if let Ok(Some(text_value)) = row.try_get::<Option<String>, _>(index) {
        return Value::String(text_value);
    }
    // BLOB — render as lossy UTF-8 when valid, otherwise base64
    if let Ok(Some(blob_value)) = row.try_get::<Option<Vec<u8>>, _>(index) {
        let rendered = match std::str::from_utf8(&blob_value) {
            Ok(utf8) => utf8.to_string(),
            Err(_) => BASE64.encode(&blob_value),
        };
        return Value::String(rendered);
    }
    // NULL (all Ok(None) paths lead here, or explicit NULL)
    Value::Null
}

// ── Postgres ─────────────────────────────────────────────────────────────────

fn map_rows_pg(rows: Vec<sqlx::postgres::PgRow>, cap: u64) -> QueryResult {
    let (rows, truncated) = apply_cap(rows, cap);
    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);

    let columns: Vec<String> = rows
        .first()
        .map(|row| {
            row.columns()
                .iter()
                .map(|col| col.name().to_string())
                .collect()
        })
        .unwrap_or_default();

    let mapped_rows = rows
        .iter()
        .map(|row| {
            (0..row.columns().len())
                .map(|index| pg_cell_to_json(row, index))
                .collect()
        })
        .collect();

    QueryResult {
        columns,
        rows: mapped_rows,
        row_count,
        truncated,
    }
}

/// Map a single Postgres cell to a `serde_json::Value`.
///
/// Try-order: `i64` → `f64` → `bool` → `serde_json::Value` (json/jsonb) →
/// `String` → Null.
fn pg_cell_to_json(row: &sqlx::postgres::PgRow, index: usize) -> Value {
    // i64 (covers bigint, int8, serial8, smallint, int4, int2 via implicit cast)
    if let Ok(Some(integer_value)) = row.try_get::<Option<i64>, _>(index) {
        return Value::Number(integer_value.into());
    }
    // f64 (float8, float4, numeric)
    if let Ok(Some(float_value)) = row.try_get::<Option<f64>, _>(index) {
        if let Some(json_number) = serde_json::Number::from_f64(float_value) {
            return Value::Number(json_number);
        }
        return Value::Null;
    }
    // bool
    if let Ok(Some(bool_value)) = row.try_get::<Option<bool>, _>(index) {
        return Value::Bool(bool_value);
    }
    // json / jsonb (sqlx `json` feature decodes these natively)
    if let Ok(Some(json_value)) = row.try_get::<Option<serde_json::Value>, _>(index) {
        return json_value;
    }
    // text, varchar, char, uuid, etc.
    if let Ok(Some(text_value)) = row.try_get::<Option<String>, _>(index) {
        return Value::String(text_value);
    }
    Value::Null
}

// ── MySQL ────────────────────────────────────────────────────────────────────

fn map_rows_mysql(rows: Vec<sqlx::mysql::MySqlRow>, cap: u64) -> QueryResult {
    let (rows, truncated) = apply_cap(rows, cap);
    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);

    let columns: Vec<String> = rows
        .first()
        .map(|row| {
            row.columns()
                .iter()
                .map(|col| col.name().to_string())
                .collect()
        })
        .unwrap_or_default();

    let mapped_rows = rows
        .iter()
        .map(|row| {
            (0..row.columns().len())
                .map(|index| mysql_cell_to_json(row, index))
                .collect()
        })
        .collect();

    QueryResult {
        columns,
        rows: mapped_rows,
        row_count,
        truncated,
    }
}

/// Map a single MySQL cell to a `serde_json::Value`.
///
/// Try-order: `i64` → `f64` → `bool` → `String` → `Vec<u8>` (lossy UTF-8) →
/// Null.
fn mysql_cell_to_json(row: &sqlx::mysql::MySqlRow, index: usize) -> Value {
    // i64
    if let Ok(Some(integer_value)) = row.try_get::<Option<i64>, _>(index) {
        return Value::Number(integer_value.into());
    }
    // f64
    if let Ok(Some(float_value)) = row.try_get::<Option<f64>, _>(index) {
        if let Some(json_number) = serde_json::Number::from_f64(float_value) {
            return Value::Number(json_number);
        }
        return Value::Null;
    }
    // bool (MySQL BIT(1) / TINYINT(1) sometimes decodes as bool)
    if let Ok(Some(bool_value)) = row.try_get::<Option<bool>, _>(index) {
        return Value::Bool(bool_value);
    }
    // text, varchar, char, enum, etc.
    if let Ok(Some(text_value)) = row.try_get::<Option<String>, _>(index) {
        return Value::String(text_value);
    }
    // BLOB / BINARY — lossy UTF-8
    if let Ok(Some(blob_value)) = row.try_get::<Option<Vec<u8>>, _>(index) {
        return Value::String(String::from_utf8_lossy(&blob_value).into_owned());
    }
    Value::Null
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::pool::ConnectionPool;

    // ── apply_cap ────────────────────────────────────────────────────────────

    #[test]
    fn apply_cap_truncates_when_over_limit() {
        let rows = vec![1, 2, 3, 4, 5];
        let (kept, truncated) = apply_cap(rows, 3);
        assert_eq!(kept, vec![1, 2, 3]);
        assert!(truncated);
    }

    #[test]
    fn apply_cap_keeps_all_when_under_limit() {
        let rows = vec![10, 20];
        let (kept, truncated) = apply_cap(rows, 10);
        assert_eq!(kept, vec![10, 20]);
        assert!(!truncated);
    }

    #[test]
    fn apply_cap_keeps_all_when_exactly_at_limit() {
        let rows = vec!["a", "b", "c"];
        let (kept, truncated) = apply_cap(rows, 3);
        assert_eq!(kept, vec!["a", "b", "c"]);
        assert!(!truncated);
    }

    #[test]
    fn apply_cap_empty_rows_not_truncated() {
        let rows: Vec<i32> = vec![];
        let (kept, truncated) = apply_cap(rows, 5);
        assert!(kept.is_empty());
        assert!(!truncated);
    }

    // ── SQLite end-to-end ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn sqlite_query_caps_and_maps_integers_and_text() {
        let pool = sqlx::sqlite::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER, name TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')")
            .execute(&pool)
            .await
            .unwrap();

        let cp = ConnectionPool::Sqlite(pool);
        let result = run(Engine::Sqlite, &cp, "SELECT id, name FROM t ORDER BY id", 2)
            .await
            .unwrap();

        assert_eq!(result.columns, vec!["id", "name"]);
        assert_eq!(result.rows.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.row_count, 2);
        // Row 0: id=1, name="a"
        assert_eq!(result.rows[0][0], serde_json::json!(1));
        assert_eq!(result.rows[0][1], serde_json::json!("a"));
        // Row 1: id=2, name="b"
        assert_eq!(result.rows[1][0], serde_json::json!(2));
        assert_eq!(result.rows[1][1], serde_json::json!("b"));
    }

    #[tokio::test]
    async fn sqlite_query_maps_null_correctly() {
        let pool = sqlx::sqlite::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER, name TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        // Third row has NULL name
        sqlx::query("INSERT INTO t VALUES (1,'a'),(2,'b'),(3,NULL)")
            .execute(&pool)
            .await
            .unwrap();

        let cp = ConnectionPool::Sqlite(pool);
        let result = run(
            Engine::Sqlite,
            &cp,
            "SELECT id, name FROM t ORDER BY id",
            10,
        )
        .await
        .unwrap();

        assert_eq!(result.rows.len(), 3);
        assert!(!result.truncated);
        // Row 2: id=3, name=NULL
        assert_eq!(result.rows[2][0], serde_json::json!(3));
        assert_eq!(result.rows[2][1], Value::Null);
    }

    #[tokio::test]
    async fn sqlite_query_no_truncation_when_under_cap() {
        let pool = sqlx::sqlite::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1),(2)")
            .execute(&pool)
            .await
            .unwrap();

        let cp = ConnectionPool::Sqlite(pool);
        let result = run(Engine::Sqlite, &cp, "SELECT id FROM t ORDER BY id", 100)
            .await
            .unwrap();

        assert_eq!(result.rows.len(), 2);
        assert!(!result.truncated);
        assert_eq!(result.row_count, 2);
    }

    #[tokio::test]
    async fn sqlite_query_empty_result_has_empty_columns() {
        let pool = sqlx::sqlite::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER, name TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        // No rows inserted

        let cp = ConnectionPool::Sqlite(pool);
        let result = run(Engine::Sqlite, &cp, "SELECT id, name FROM t", 10)
            .await
            .unwrap();

        // v1 known limitation: columns is empty when result set is empty
        assert_eq!(result.columns, Vec::<String>::new());
        assert_eq!(result.rows.len(), 0);
        assert!(!result.truncated);
        assert_eq!(result.row_count, 0);
    }

    #[tokio::test]
    async fn sqlite_engine_pool_mismatch_returns_err() {
        // Using a Sqlite pool but passing Engine::Postgres should return an error.
        let pool = sqlx::sqlite::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        let cp = ConnectionPool::Sqlite(pool);
        let result = run(Engine::Postgres, &cp, "SELECT 1", 10).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("mismatch"));
    }
}
