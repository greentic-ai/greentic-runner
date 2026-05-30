//! In-runtime SQL gateway: serves `/sql/<conn>/schema` + `/sql/<conn>/query`
//! over the runner's axum server, backed by read-only sqlx pools, so the
//! `greentic.sql` design extension can run text-to-data inside one bundle.

pub mod config;
pub mod execute;
pub mod introspect;
pub mod pool;
pub mod routes;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use introspect::Schema;
use pool::ConnectionPool;

/// One configured connection's live pool + engine.
pub struct SqlConnection {
    pub engine: config::Engine,
    pub pool: ConnectionPool,
}

/// Runtime state for the SQL gateway: connections, the cached schema per
/// connection, and the shared bearer token expected on `/sql` requests.
#[derive(Clone)]
pub struct SqlGateway {
    inner: Arc<SqlGatewayInner>,
}

struct SqlGatewayInner {
    connections: HashMap<String, SqlConnection>,
    // Populated and read by the /schema route (Task 5); suppress dead_code until wired.
    #[allow(dead_code)]
    schema_cache: RwLock<HashMap<String, Schema>>,
    auth_token: String,
}

impl SqlGateway {
    #[must_use]
    pub fn new(connections: HashMap<String, SqlConnection>, auth_token: String) -> Self {
        Self {
            inner: Arc::new(SqlGatewayInner {
                connections,
                schema_cache: RwLock::new(HashMap::new()),
                auth_token,
            }),
        }
    }

    #[must_use]
    pub fn connection(&self, name: &str) -> Option<&SqlConnection> {
        self.inner.connections.get(name)
    }

    #[must_use]
    pub fn check_token(&self, presented: &str) -> bool {
        // constant-time compare to prevent timing-based token enumeration
        use subtle::ConstantTimeEq;
        presented
            .as_bytes()
            .ct_eq(self.inner.auth_token.as_bytes())
            .into()
    }

    // Used by the /schema route handler (Task 5).
    #[allow(dead_code)]
    pub(crate) fn cache(&self) -> &RwLock<HashMap<String, Schema>> {
        &self.inner.schema_cache
    }
}
