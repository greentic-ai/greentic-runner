//! Per-tenant agentic-worker component tool catalog.
//!
//! Exposes greentic `.gtpack` **components** (the canonical Component layer)
//! to an agentic worker as LLM tools, mirroring [`crate::mcp_source`] for the
//! MCP surface. A worker's `AgentConfig.tools` entry of the form
//! `ToolRef { extension_id: "component:<component_ref>", tool_name: "<operation>" }`
//! resolves here: the catalog supplies the LLM-facing `description`/`parameters`
//! for the list seam and routes the call to a [`ComponentInvoker`] for dispatch.
//!
//! The actual WASM component instantiation lives in the runner host (over its
//! `PackRuntime` component host), behind the [`ComponentInvoker`] trait, so
//! this crate stays free of any `wasmtime`/runner-host dependency — it sees
//! only the trait and JSON.
//!
//! Resilience contract (a component tool must never break an agent step):
//! - Building a catalog is infallible: a [`ComponentInvoker`] that surfaces no
//!   operations simply yields an empty catalog. [`ComponentToolSource::catalog`]
//!   never returns or propagates an error.
//! - [`ComponentToolCatalog::dispatch`] always returns a JSON
//!   [`serde_json::Value`] and never panics — an unknown `(component_ref,
//!   operation)` or an invoker failure becomes `{"error": "..."}` so the LLM
//!   observes it as a normal tool result.
//!
//! Like the MCP source (and unlike the designer's `mcp__server__tool` string
//! mangling), every tool is keyed by a `(component_ref, operation)` tuple.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::json;

use crate::tenant::TenantContext;

/// How long a built catalog is reused before a rebuild is considered.
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);

/// LLM-facing schema for one component operation: enough to build an
/// `LlmToolSchema` in [`crate::tools::list_tools_for_llm`].
#[derive(Clone, Debug)]
pub struct ComponentToolEntry {
    pub description: String,
    pub parameters: serde_json::Value,
}

/// One component operation discoverable as an agentic-worker tool: the
/// `(component_ref, operation)` identity plus its LLM-facing schema. Produced
/// by [`ComponentInvoker::list_operations`].
#[derive(Clone, Debug)]
pub struct ComponentOperation {
    pub component_ref: String,
    pub operation: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Host-side seam that resolves and invokes greentic components. The concrete
/// implementation lives in the runner host (backed by `PackRuntime`); this
/// crate depends only on the trait + JSON so it need not pull in
/// `wasmtime`/runner-host.
///
/// Both methods are total: `list_operations` returns whatever is currently
/// exposed (possibly empty), and `invoke` reports failure via `Err(String)`
/// which [`ComponentToolCatalog::dispatch`] wraps into an `{"error": ...}`
/// value — neither aborts an agent step.
pub trait ComponentInvoker: Send + Sync {
    /// Describe every component operation exposed to agentic-worker tools.
    fn list_operations(&self) -> Vec<ComponentOperation>;

    /// Invoke one component operation with JSON `args_json`. Returns the raw
    /// component output value on success, or a stringified error on any
    /// failure (bad args, instantiation error, component trap, timeout).
    fn invoke<'a>(
        &'a self,
        component_ref: &'a str,
        operation: &'a str,
        args_json: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>;
}

/// Immutable per-tenant view of the component-tool surface. Carries the
/// LLM-facing schemas (list seam) plus the [`ComponentInvoker`] handle needed
/// to dispatch a call.
pub struct ComponentToolCatalog {
    /// `(component_ref, operation)` → LLM-facing tool schema.
    tools: HashMap<(String, String), ComponentToolEntry>,
    invoker: Arc<dyn ComponentInvoker>,
    fetched_at: Instant,
}

impl ComponentToolCatalog {
    fn from_invoker(invoker: Arc<dyn ComponentInvoker>) -> Self {
        let mut tools = HashMap::new();
        for op in invoker.list_operations() {
            tools.insert(
                (op.component_ref, op.operation),
                ComponentToolEntry {
                    description: op.description,
                    parameters: op.parameters,
                },
            );
        }
        Self {
            tools,
            invoker,
            fetched_at: Instant::now(),
        }
    }

    /// Iterate every `(component_ref, operation)` key with its schema.
    pub fn tools(&self) -> impl Iterator<Item = (&(String, String), &ComponentToolEntry)> {
        self.tools.iter()
    }

    /// Number of operations in the catalog.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the catalog exposes no operations.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// LLM-facing schema for one operation, if present.
    pub fn tool_entry(&self, component_ref: &str, operation: &str) -> Option<&ComponentToolEntry> {
        self.tools
            .get(&(component_ref.to_string(), operation.to_string()))
    }

    /// Invoke one component operation, always returning a JSON value. An
    /// unknown `(component_ref, operation)` or an invoker failure is surfaced
    /// as `{"error": "..."}` so the LLM observes it as a normal tool result.
    pub async fn dispatch(
        &self,
        component_ref: &str,
        operation: &str,
        args_json: &str,
    ) -> serde_json::Value {
        if self.tool_entry(component_ref, operation).is_none() {
            return json!({
                "error": format!("unknown component tool '{component_ref}/{operation}'")
            });
        }
        match self
            .invoker
            .invoke(component_ref, operation, args_json)
            .await
        {
            Ok(value) => value,
            Err(e) => json!({ "error": e }),
        }
    }

    /// Build a catalog directly from a tool map + invoker, bypassing
    /// [`ComponentToolSource`]. Test-only: lets `tools.rs` exercise the
    /// list/dispatch seams without standing up a real invoker.
    #[cfg(test)]
    pub(crate) fn for_tests(
        tools: HashMap<(String, String), ComponentToolEntry>,
        invoker: Arc<dyn ComponentInvoker>,
    ) -> Self {
        Self {
            tools,
            invoker,
            fetched_at: Instant::now(),
        }
    }
}

/// Per-tenant, TTL-gated source of agentic-worker component tool catalogs.
///
/// Mirrors [`crate::mcp_source::McpToolSource`]: a built catalog is cached per
/// tenant behind a short TTL so the per-step resolution in
/// [`crate::r#loop::run_step`] does not re-enumerate the pack's components on
/// every iteration. The [`ComponentInvoker`] is the host-injected seam over
/// the pack component runtime.
pub struct ComponentToolSource {
    invoker: Arc<dyn ComponentInvoker>,
    cache: DashMap<String, Arc<ComponentToolCatalog>>,
}

impl ComponentToolSource {
    /// Construct a source over a host-provided component invoker.
    pub fn new(invoker: Arc<dyn ComponentInvoker>) -> Self {
        Self {
            invoker,
            cache: DashMap::new(),
        }
    }

    /// Stable per-tenant cache key — the same `(tenant_id, env_id)` pair
    /// `TenantContext::key_prefix` is built from.
    fn cache_key(tenant: &TenantContext) -> String {
        format!("{}:{}", tenant.tenant_id, tenant.env_id)
    }

    /// Return the tenant's component tool catalog, rebuilding when stale or
    /// absent. Infallible by contract.
    pub async fn catalog(&self, tenant: &TenantContext) -> Arc<ComponentToolCatalog> {
        let key = Self::cache_key(tenant);

        if let Some(entry) = self.cache.get(&key) {
            let snap = entry.value();
            if snap.fetched_at.elapsed() < CATALOG_TTL {
                return snap.clone();
            }
        }

        let built = Arc::new(ComponentToolCatalog::from_invoker(self.invoker.clone()));
        self.cache.insert(key, built.clone());
        built
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_support {
    //! Test-only fakes shared with `tools.rs` tests.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scriptable [`ComponentInvoker`]: returns a fixed operation list and a
    /// fixed `invoke` result, and counts `list_operations` calls so the TTL
    /// cache can be asserted.
    pub(crate) struct FakeInvoker {
        ops: Vec<ComponentOperation>,
        result: Result<serde_json::Value, String>,
        pub list_calls: AtomicUsize,
    }

    impl FakeInvoker {
        pub(crate) fn new(
            ops: Vec<ComponentOperation>,
            result: Result<serde_json::Value, String>,
        ) -> Self {
            Self {
                ops,
                result,
                list_calls: AtomicUsize::new(0),
            }
        }
    }

    impl ComponentInvoker for FakeInvoker {
        fn list_operations(&self) -> Vec<ComponentOperation> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            self.ops.clone()
        }

        fn invoke<'a>(
            &'a self,
            _component_ref: &'a str,
            _operation: &'a str,
            _args_json: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>> {
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    /// Build a `ComponentOperation` with an object input schema.
    pub(crate) fn op(
        component_ref: &str,
        operation: &str,
        description: &str,
    ) -> ComponentOperation {
        ComponentOperation {
            component_ref: component_ref.to_string(),
            operation: operation.to_string(),
            description: description.to_string(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    /// A one-entry tool map for `ComponentToolCatalog::for_tests`.
    pub(crate) fn one_tool(
        component_ref: &str,
        operation: &str,
        description: &str,
        parameters: serde_json::Value,
    ) -> HashMap<(String, String), ComponentToolEntry> {
        let mut m = HashMap::new();
        m.insert(
            (component_ref.to_string(), operation.to_string()),
            ComponentToolEntry {
                description: description.to_string(),
                parameters,
            },
        );
        m
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn tenant() -> TenantContext {
        TenantContext::new("acme", "prod")
    }

    #[tokio::test]
    async fn source_lists_component_operations() {
        let invoker = Arc::new(FakeInvoker::new(
            vec![
                op("greentic.refund", "issue_refund", "Issue a refund"),
                op("greentic.refund", "lookup_order", "Look up an order"),
            ],
            Ok(json!({})),
        ));
        let source = ComponentToolSource::new(invoker);
        let catalog = source.catalog(&tenant()).await;

        assert_eq!(catalog.len(), 2);
        let entry = catalog
            .tool_entry("greentic.refund", "issue_refund")
            .expect("operation present");
        assert_eq!(entry.description, "Issue a refund");
        assert!(
            catalog
                .tool_entry("greentic.refund", "lookup_order")
                .is_some()
        );
        assert!(catalog.tool_entry("greentic.refund", "absent").is_none());
    }

    #[tokio::test]
    async fn dispatch_returns_component_value_on_success() {
        let invoker = Arc::new(FakeInvoker::new(
            vec![op("greentic.refund", "issue_refund", "Issue a refund")],
            Ok(json!({ "refund_id": "r-1" })),
        ));
        let source = ComponentToolSource::new(invoker);
        let catalog = source.catalog(&tenant()).await;

        let out = catalog
            .dispatch("greentic.refund", "issue_refund", "{}")
            .await;
        assert_eq!(out, json!({ "refund_id": "r-1" }), "got: {out}");
        assert!(!out.to_string().contains("error"), "got: {out}");
    }

    #[tokio::test]
    async fn dispatch_wraps_invoker_error() {
        let invoker = Arc::new(FakeInvoker::new(
            vec![op("greentic.refund", "issue_refund", "Issue a refund")],
            Err("component trapped".to_string()),
        ));
        let source = ComponentToolSource::new(invoker);
        let catalog = source.catalog(&tenant()).await;

        let out = catalog
            .dispatch("greentic.refund", "issue_refund", "{}")
            .await;
        assert_eq!(out, json!({ "error": "component trapped" }), "got: {out}");
    }

    #[tokio::test]
    async fn dispatch_unknown_operation_errors_without_invoking() {
        let invoker = Arc::new(FakeInvoker::new(
            vec![op("greentic.refund", "issue_refund", "Issue a refund")],
            Ok(json!({ "should": "not be returned" })),
        ));
        let source = ComponentToolSource::new(invoker);
        let catalog = source.catalog(&tenant()).await;

        // An operation not in the catalog must not reach the invoker.
        let out = catalog.dispatch("greentic.refund", "no_such", "{}").await;
        assert!(out.to_string().contains("error"), "got: {out}");
        assert!(
            out.to_string().contains("greentic.refund/no_such"),
            "got: {out}"
        );
    }

    #[tokio::test]
    async fn ttl_cache_reuses_within_window() {
        let invoker = Arc::new(FakeInvoker::new(
            vec![op("greentic.refund", "issue_refund", "Issue a refund")],
            Ok(json!({})),
        ));
        let source = ComponentToolSource::new(invoker.clone());
        let t = tenant();
        let first = source.catalog(&t).await;
        let second = source.catalog(&t).await;

        assert!(
            Arc::ptr_eq(&first, &second),
            "second call must hit TTL cache"
        );
        assert_eq!(
            invoker.list_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "operations enumerated exactly once within the TTL window"
        );
    }

    #[tokio::test]
    async fn for_tests_builds_catalog_with_entry() {
        let invoker = Arc::new(FakeInvoker::new(vec![], Ok(json!({ "ok": true }))));
        let catalog = ComponentToolCatalog::for_tests(
            one_tool(
                "greentic.refund",
                "issue_refund",
                "Issue a refund",
                json!({ "type": "object" }),
            ),
            invoker,
        );
        assert_eq!(catalog.len(), 1);
        let out = catalog
            .dispatch("greentic.refund", "issue_refund", "{}")
            .await;
        assert_eq!(out, json!({ "ok": true }), "got: {out}");
    }
}
