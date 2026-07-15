//! Tests for the per-tenant agentic-worker MCP tool catalog.
//!
//! Split by concern:
//! - `catalog` — catalog-building, role filtering, TTL cache tests
//! - `dispatch` — dispatch, parse_rows, and local-wasm integration tests

mod catalog;
mod dispatch;
