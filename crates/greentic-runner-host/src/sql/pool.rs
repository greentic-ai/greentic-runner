//! Per-engine read-only sqlx connection pools.
//! Pool builder logic is filled in by Task 2.

/// A live sqlx pool for one configured database connection.
pub enum ConnectionPool {
    Postgres(sqlx::PgPool),
    Mysql(sqlx::MySqlPool),
    Sqlite(sqlx::SqlitePool),
}
