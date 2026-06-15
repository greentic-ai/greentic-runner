//! Production [`SessionResumer`] that resumes a paused flow by feeding a
//! synthesized [`IngressEnvelope`] back through the runtime ingress entry
//! ([`StateMachineRuntime::handle`]).
//!
//! ## How a resume is keyed (verified against `engine/runtime.rs`)
//!
//! When a `sorla.call await` node pauses, `PackFlowAdapter::call` persists the
//! wait via `FlowResumeStore::save(envelope, wait)`. The wait is keyed by
//! `build_store_ctx`, which derives:
//!   * a **store hint** = `session_hint` (else `canonical_session_hint`), and
//!     when `pack_id` is present, suffixed with `::pack=<pack_id>`;
//!   * a **user id** = `sha256(store_hint)` (see `derive_user_id`);
//!   * a **reply scope** = the envelope's `ReplyScope` (whose `scope_hash`
//!     covers `conversation`/`thread`/`reply_to`).
//!
//! To resume, `FlowResumeStore::fetch(envelope)` re-derives the same triplet,
//! so the synthesized envelope MUST reproduce the *exact* store hint and reply
//! scope used at save time. The dispatch correlation id echoed in the response
//! is `ctx.session_id` — the canonical session hint (optionally carrying the
//! `::pack=<id>` suffix). This resumer splits that suffix off, sets the BARE
//! hint as `session_hint` (so `build_store_ctx` re-appends the pack suffix
//! exactly once), carries the recovered `pack_id`, and rebuilds the reply scope
//! by parsing the conversation out of the canonical hint
//! (`tenant:provider:channel:conversation:user`).
//!
//! ### Keying + routing caveat (IMPORTANT — see PR notes)
//!
//! Resuming through `handle` requires the synthesized envelope to (a) ROUTE via
//! `StateMachine::step`, which needs a *registered* `(pack_id, flow_id)`, and
//! (b) KEY the saved wait via `FlowResumeStore::fetch`, which needs the store
//! hint (`<bare hint>::pack=<pack_id>`) and the reply scope (`scope_hash` over
//! `conversation`/`thread`/`reply_to`).
//!
//! The dispatch wire contract ([`greentic_types::RuntimeDispatchResponse`] plus
//! the `Greentic-*` headers) carries only `(tenant, env, correlation_id)`. The
//! correlation id is the canonical session hint, which embeds the conversation
//! (4th `:`-segment) but NOT the `pack_id` or `flow_id` needed to route.
//!
//! This resumer recovers `pack_id` from a `::pack=<id>` suffix (the convention
//! pinned by `tests/sorla_node.rs`, where `ctx.session_id` carries it) and
//! `flow_id` from an additional `::flow=<id>` marker. For production resume to
//! work, the dispatch side must therefore emit a correlation id of the form
//! `<bare hint>::pack=<pack_id>::flow=<flow_id>` (or the wire contract must be
//! extended to carry pack/flow, or a server-side `correlation_id → (pack, flow,
//! scope)` map must exist). A bare canonical hint alone is NOT resumable. Waits
//! whose original inbound used a non-empty `thread`/`reply_to` are likewise not
//! recoverable from the hint and must be resumed by an inbound activity.

use anyhow::Result;
use async_trait::async_trait;
use greentic_types::{ReplyScope, TenantCtx};
use serde_json::Value;
use std::sync::Arc;

use super::dispatch_listener::SessionResumer;
use crate::engine::runtime::{IngressEnvelope, StateMachineRuntime};

/// Marker appended to a store hint when a pack id is known (mirrors
/// `build_store_ctx` in `engine/runtime.rs`).
const PACK_HINT_MARKER: &str = "::pack=";

/// Marker carrying the routing flow id in the correlation id. Unlike `::pack=`,
/// this is NOT part of the store hint — it is consumed only to route the resume
/// envelope through `StateMachine::step`, which requires a registered
/// `(pack_id, flow_id)`. It is stripped before the store hint is derived.
const FLOW_HINT_MARKER: &str = "::flow=";

/// Resumes paused flow sessions by driving the runtime ingress entry.
pub struct RuntimeSessionResumer {
    runtime: Arc<StateMachineRuntime>,
}

impl RuntimeSessionResumer {
    /// Build a resumer over the live runtime ingress handle.
    ///
    /// `runtime` is the same [`StateMachineRuntime`] that serves inbound
    /// ingress; resuming routes through its `handle` so the existing
    /// `FlowResumeStore::fetch` + `FlowEngine::resume` path runs unchanged.
    pub fn new(runtime: Arc<StateMachineRuntime>) -> Self {
        Self { runtime }
    }

    /// Build the [`IngressEnvelope`] that re-keys the saved wait.
    ///
    /// Split out (and `pub(crate)`) so it can be unit-tested without a broker.
    ///
    /// * `session_hint` is set to the bare canonical hint so
    ///   `FlowResumeStore::fetch` re-derives the same store key (with the pack
    ///   suffix re-appended exactly once by `build_store_ctx`).
    /// * `pack_id` is recovered from the `::pack=<id>` suffix when present so
    ///   `StateMachineRuntime::handle` (which requires a `pack_id`) succeeds and
    ///   the store hint is reproduced identically.
    /// * `flow_id` is recovered from the `::flow=<id>` marker when present so the
    ///   resume routes through `StateMachine::step` (which requires a registered
    ///   `(pack_id, flow_id)`). The saved wait's snapshot then supplies the real
    ///   resume flow/node. When absent, the bare pack id is used as a fallback
    ///   flow id (only relevant when no wait is found).
    /// * the reply scope's `conversation` is parsed from the canonical hint so
    ///   the scope hash matches the saved wait.
    /// * `payload` carries the runtime `output` as the resume flow input.
    ///
    /// NOTE: the dispatch wire contract ([`greentic_types::RuntimeDispatchResponse`]
    /// plus headers) carries only `(tenant, env, correlation_id)`. Routing a
    /// resume requires `pack_id` AND `flow_id`; this resumer recovers them from
    /// the `::pack=`/`::flow=` markers in the correlation id. Until those markers
    /// are emitted at dispatch time (or the response carries the fields), a bare
    /// canonical hint cannot be resumed — see the module docs.
    pub(crate) fn build_resume_envelope(
        tenant: &TenantCtx,
        correlation_id: &str,
        output: Value,
    ) -> IngressEnvelope {
        let (without_flow, flow_marker) = split_marker(correlation_id, FLOW_HINT_MARKER);
        let (bare_hint, pack_id) = split_pack_suffix(&without_flow);
        let parts: Vec<&str> = bare_hint.splitn(5, ':').collect();
        let provider = parts.get(1).map(|segment| segment.to_string());
        let channel = parts.get(2).map(|segment| segment.to_string());
        let conversation = parts.get(3).map(|segment| segment.to_string());
        let user = parts.get(4).map(|segment| segment.to_string());

        let flow_id = flow_marker
            .or_else(|| pack_id.clone())
            .unwrap_or_else(|| "resume.flow".to_string());

        IngressEnvelope {
            tenant: tenant.tenant_id.to_string(),
            env: Some(tenant.env.to_string()),
            pack_id,
            // Routes the resume into `PackFlowAdapter::call`; the saved wait's
            // snapshot overrides the actual resume flow/node from there.
            flow_id,
            flow_type: None,
            action: None,
            // The store hint is `session_hint` (+ `::pack=<pack_id>` re-appended
            // by `build_store_ctx`). We set the BARE hint here and carry the pack
            // id separately so the suffix is reproduced exactly once — setting
            // the suffixed correlation id here AND `pack_id` would double-suffix
            // and miss the saved wait.
            session_hint: Some(bare_hint.clone()),
            provider,
            channel,
            conversation: conversation.clone(),
            user,
            activity_id: None,
            timestamp: None,
            payload: output,
            metadata: None,
            reply_scope: conversation.map(|conversation| ReplyScope {
                conversation,
                thread: None,
                reply_to: None,
                correlation: None,
            }),
        }
    }
}

#[async_trait]
impl SessionResumer for RuntimeSessionResumer {
    async fn resume(&self, tenant: TenantCtx, correlation_id: &str, output: Value) -> Result<()> {
        let envelope = Self::build_resume_envelope(&tenant, correlation_id, output);
        self.runtime.handle(envelope).await.map(|_| ())
    }
}

/// Split a store hint into its bare canonical hint and the pack id encoded in
/// the trailing `::pack=<id>` marker (if any). Mirrors the suffix added by
/// `build_store_ctx`.
fn split_pack_suffix(hint: &str) -> (String, Option<String>) {
    split_marker(hint, PACK_HINT_MARKER)
}

/// Split `value` on the LAST occurrence of `marker`, returning the prefix and
/// the captured (non-empty) trailing segment. When the marker is absent or the
/// captured value is empty, the input is returned unchanged with `None`.
fn split_marker(value: &str, marker: &str) -> (String, Option<String>) {
    match value.rsplit_once(marker) {
        Some((prefix, captured)) if !captured.is_empty() => {
            (prefix.to_string(), Some(captured.to_string()))
        }
        _ => (value.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{EnvId, TenantId};
    use serde_json::json;
    use std::str::FromStr;

    fn tenant_ctx() -> TenantCtx {
        TenantCtx::new(
            EnvId::from_str("local").unwrap(),
            TenantId::from_str("demo").unwrap(),
        )
    }

    #[test]
    fn build_resume_envelope_sets_session_hint_to_correlation_id() {
        let envelope = RuntimeSessionResumer::build_resume_envelope(
            &tenant_ctx(),
            "demo:provider:chan:conv:user",
            json!({ "ok": true }),
        );
        assert_eq!(
            envelope.session_hint.as_deref(),
            Some("demo:provider:chan:conv:user")
        );
        assert_eq!(envelope.payload, json!({ "ok": true }));
        assert_eq!(envelope.tenant, "demo");
        assert_eq!(envelope.env.as_deref(), Some("local"));
    }

    #[test]
    fn build_resume_envelope_parses_conversation_into_reply_scope() {
        let envelope = RuntimeSessionResumer::build_resume_envelope(
            &tenant_ctx(),
            "demo:provider:chan:conv:user",
            json!(null),
        );
        let scope = envelope.reply_scope.expect("reply scope from conversation");
        assert_eq!(scope.conversation, "conv");
        assert!(scope.thread.is_none());
        assert!(scope.correlation.is_none());
        assert_eq!(envelope.conversation.as_deref(), Some("conv"));
        assert_eq!(envelope.provider.as_deref(), Some("provider"));
        assert_eq!(envelope.channel.as_deref(), Some("chan"));
        assert_eq!(envelope.user.as_deref(), Some("user"));
    }

    #[test]
    fn build_resume_envelope_recovers_pack_id_from_suffix() {
        let envelope = RuntimeSessionResumer::build_resume_envelope(
            &tenant_ctx(),
            "demo:provider:chan:conv:user::pack=greentic.demo",
            json!(null),
        );
        assert_eq!(envelope.pack_id.as_deref(), Some("greentic.demo"));
        // session_hint is the BARE hint; `build_store_ctx` re-appends the pack
        // suffix exactly once, so it must not be carried here.
        assert_eq!(
            envelope.session_hint.as_deref(),
            Some("demo:provider:chan:conv:user")
        );
        // conversation is still parsed from the bare hint, not the suffix.
        assert_eq!(envelope.conversation.as_deref(), Some("conv"));
    }

    #[test]
    fn build_resume_envelope_recovers_pack_and_flow_for_routing() {
        let envelope = RuntimeSessionResumer::build_resume_envelope(
            &tenant_ctx(),
            "demo:provider:chan:conv:user::pack=greentic.demo::flow=wait.flow",
            json!(null),
        );
        // flow id routes the resume; pack id keys it.
        assert_eq!(envelope.flow_id, "wait.flow");
        assert_eq!(envelope.pack_id.as_deref(), Some("greentic.demo"));
        // the flow marker is stripped before deriving the store hint.
        assert_eq!(
            envelope.session_hint.as_deref(),
            Some("demo:provider:chan:conv:user")
        );
        assert_eq!(envelope.conversation.as_deref(), Some("conv"));
    }

    #[test]
    fn split_pack_suffix_handles_missing_and_empty_marker() {
        assert_eq!(
            split_pack_suffix("a:b:c:d:e"),
            ("a:b:c:d:e".to_string(), None)
        );
        assert_eq!(
            split_pack_suffix("a:b:c:d:e::pack=p"),
            ("a:b:c:d:e".to_string(), Some("p".to_string()))
        );
        assert_eq!(
            split_pack_suffix("a:b:c:d:e::pack="),
            ("a:b:c:d:e::pack=".to_string(), None)
        );
    }
}
