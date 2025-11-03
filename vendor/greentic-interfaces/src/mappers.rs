//! Conversion helpers between generated WIT bindings and `greentic-types`.

use std::convert::{TryFrom, TryInto};

use greentic_types as types;
use semver::Version;
use time::OffsetDateTime;

use crate::bindings;

type MapperResult<T> = Result<T, types::GreenticError>;

fn invalid_input(msg: impl Into<String>) -> types::GreenticError {
    types::GreenticError::new(types::ErrorCode::InvalidInput, msg)
}

fn i128_to_i64(value: i128) -> MapperResult<i64> {
    value
        .try_into()
        .map_err(|_| invalid_input("numeric overflow converting deadline"))
}

fn timestamp_ms_to_offset(ms: i64) -> MapperResult<OffsetDateTime> {
    let nanos = (ms as i128)
        .checked_mul(1_000_000)
        .ok_or_else(|| invalid_input("timestamp overflow"))?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|err| invalid_input(format!("invalid timestamp: {err}")))
}

fn offset_to_timestamp_ms(dt: &OffsetDateTime) -> MapperResult<i64> {
    let nanos = dt.unix_timestamp_nanos();
    let ms = nanos
        .checked_div(1_000_000)
        .ok_or_else(|| invalid_input("timestamp division overflow"))?;
    ms.try_into()
        .map_err(|_| invalid_input("timestamp overflow converting to milliseconds"))
}

type WitTenantCtx = bindings::greentic::interfaces_types::types::TenantCtx;
type WitImpersonation = bindings::greentic::interfaces_types::types::Impersonation;
type WitSessionCursor = bindings::greentic::interfaces_types::types::SessionCursor;
type WitOutcome = bindings::greentic::interfaces_types::types::Outcome;
type WitOutcomePending = bindings::greentic::interfaces_types::types::OutcomePending;
type WitOutcomeError = bindings::greentic::interfaces_types::types::OutcomeError;
type WitErrorCode = bindings::greentic::interfaces_types::types::ErrorCode;
type WitAllowList = bindings::greentic::interfaces_types::types::AllowList;
type WitProtocol = bindings::greentic::interfaces_types::types::Protocol;
type WitNetworkPolicy = bindings::greentic::interfaces_types::types::NetworkPolicy;
type WitSpanContext = bindings::greentic::interfaces_types::types::SpanContext;
type WitPackRef = bindings::greentic::interfaces_types::types::PackRef;
type WitSignature = bindings::greentic::interfaces_types::types::Signature;
type WitSignatureAlgorithm = bindings::greentic::interfaces_types::types::SignatureAlgorithm;

impl From<WitImpersonation> for types::Impersonation {
    fn from(value: WitImpersonation) -> Self {
        Self {
            actor_id: types::UserId::from(value.actor_id),
            reason: value.reason,
        }
    }
}

impl From<types::Impersonation> for WitImpersonation {
    fn from(value: types::Impersonation) -> Self {
        Self {
            actor_id: value.actor_id.into(),
            reason: value.reason,
        }
    }
}

impl TryFrom<WitTenantCtx> for types::TenantCtx {
    type Error = types::GreenticError;

    fn try_from(value: WitTenantCtx) -> MapperResult<Self> {
        let deadline = value
            .deadline_ms
            .map(|ms| types::InvocationDeadline::from_unix_millis(ms as i128));

        Ok(Self {
            env: types::EnvId::from(value.env),
            tenant: types::TenantId::from(value.tenant.clone()),
            tenant_id: types::TenantId::from(value.tenant_id),
            team: value.team.map(types::TeamId::from),
            team_id: value.team_id.map(types::TeamId::from),
            user: value.user.map(types::UserId::from),
            user_id: value.user_id.map(types::UserId::from),
            trace_id: value.trace_id,
            correlation_id: value.correlation_id,
            deadline,
            attempt: value.attempt,
            idempotency_key: value.idempotency_key,
            impersonation: value.impersonation.map(types::Impersonation::from),
        })
    }
}

impl TryFrom<types::TenantCtx> for WitTenantCtx {
    type Error = types::GreenticError;

    fn try_from(value: types::TenantCtx) -> MapperResult<Self> {
        let deadline_ms = match value.deadline {
            Some(deadline) => Some(i128_to_i64(deadline.unix_millis())?),
            None => None,
        };

        Ok(Self {
            env: value.env.into(),
            tenant: value.tenant.into(),
            tenant_id: value.tenant_id.into(),
            team: value.team.map(Into::into),
            team_id: value.team_id.map(Into::into),
            user: value.user.map(Into::into),
            user_id: value.user_id.map(Into::into),
            trace_id: value.trace_id,
            correlation_id: value.correlation_id,
            deadline_ms,
            attempt: value.attempt,
            idempotency_key: value.idempotency_key,
            impersonation: value.impersonation.map(Into::into),
        })
    }
}

impl From<WitSessionCursor> for types::SessionCursor {
    fn from(value: WitSessionCursor) -> Self {
        Self {
            node_pointer: value.node_pointer,
            wait_reason: value.wait_reason,
            outbox_marker: value.outbox_marker,
        }
    }
}

impl From<types::SessionCursor> for WitSessionCursor {
    fn from(value: types::SessionCursor) -> Self {
        Self {
            node_pointer: value.node_pointer,
            wait_reason: value.wait_reason,
            outbox_marker: value.outbox_marker,
        }
    }
}

impl From<WitErrorCode> for types::ErrorCode {
    fn from(value: WitErrorCode) -> Self {
        match value {
            WitErrorCode::Unknown => Self::Unknown,
            WitErrorCode::InvalidInput => Self::InvalidInput,
            WitErrorCode::NotFound => Self::NotFound,
            WitErrorCode::Conflict => Self::Conflict,
            WitErrorCode::Timeout => Self::Timeout,
            WitErrorCode::Unauthenticated => Self::Unauthenticated,
            WitErrorCode::PermissionDenied => Self::PermissionDenied,
            WitErrorCode::RateLimited => Self::RateLimited,
            WitErrorCode::Unavailable => Self::Unavailable,
            WitErrorCode::Internal => Self::Internal,
        }
    }
}

impl From<types::ErrorCode> for WitErrorCode {
    fn from(value: types::ErrorCode) -> Self {
        match value {
            types::ErrorCode::Unknown => Self::Unknown,
            types::ErrorCode::InvalidInput => Self::InvalidInput,
            types::ErrorCode::NotFound => Self::NotFound,
            types::ErrorCode::Conflict => Self::Conflict,
            types::ErrorCode::Timeout => Self::Timeout,
            types::ErrorCode::Unauthenticated => Self::Unauthenticated,
            types::ErrorCode::PermissionDenied => Self::PermissionDenied,
            types::ErrorCode::RateLimited => Self::RateLimited,
            types::ErrorCode::Unavailable => Self::Unavailable,
            types::ErrorCode::Internal => Self::Internal,
        }
    }
}

impl From<WitOutcome> for types::Outcome<String> {
    fn from(value: WitOutcome) -> Self {
        match value {
            WitOutcome::Done(val) => Self::Done(val),
            WitOutcome::Pending(payload) => Self::Pending {
                reason: payload.reason,
                expected_input: payload.expected_input,
            },
            WitOutcome::Error(payload) => Self::Error {
                code: payload.code.into(),
                message: payload.message,
            },
        }
    }
}

impl From<types::Outcome<String>> for WitOutcome {
    fn from(value: types::Outcome<String>) -> Self {
        match value {
            types::Outcome::Done(val) => Self::Done(val),
            types::Outcome::Pending {
                reason,
                expected_input,
            } => Self::Pending(WitOutcomePending {
                reason,
                expected_input,
            }),
            types::Outcome::Error { code, message } => Self::Error(WitOutcomeError {
                code: code.into(),
                message,
            }),
        }
    }
}

impl From<WitProtocol> for types::Protocol {
    fn from(value: WitProtocol) -> Self {
        match value {
            WitProtocol::Http => Self::Http,
            WitProtocol::Https => Self::Https,
            WitProtocol::Tcp => Self::Tcp,
            WitProtocol::Udp => Self::Udp,
            WitProtocol::Grpc => Self::Grpc,
            WitProtocol::Custom(v) => Self::Custom(v),
        }
    }
}

impl From<types::Protocol> for WitProtocol {
    fn from(value: types::Protocol) -> Self {
        match value {
            types::Protocol::Http => Self::Http,
            types::Protocol::Https => Self::Https,
            types::Protocol::Tcp => Self::Tcp,
            types::Protocol::Udp => Self::Udp,
            types::Protocol::Grpc => Self::Grpc,
            types::Protocol::Custom(v) => Self::Custom(v),
        }
    }
}

impl From<WitAllowList> for types::AllowList {
    fn from(value: WitAllowList) -> Self {
        Self {
            domains: value.domains,
            ports: value.ports,
            protocols: value.protocols.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<types::AllowList> for WitAllowList {
    fn from(value: types::AllowList) -> Self {
        Self {
            domains: value.domains,
            ports: value.ports,
            protocols: value.protocols.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<WitNetworkPolicy> for types::NetworkPolicy {
    fn from(value: WitNetworkPolicy) -> Self {
        Self {
            egress: value.egress.into(),
            deny_on_miss: value.deny_on_miss,
        }
    }
}

impl From<types::NetworkPolicy> for WitNetworkPolicy {
    fn from(value: types::NetworkPolicy) -> Self {
        Self {
            egress: value.egress.into(),
            deny_on_miss: value.deny_on_miss,
        }
    }
}

impl TryFrom<WitSpanContext> for types::SpanContext {
    type Error = types::GreenticError;

    fn try_from(value: WitSpanContext) -> MapperResult<Self> {
        let start = value.start_ms.map(timestamp_ms_to_offset).transpose()?;
        let end = value.end_ms.map(timestamp_ms_to_offset).transpose()?;

        Ok(Self {
            tenant: types::TenantId::from(value.tenant),
            session_id: value.session_id.map(types::SessionKey::from),
            flow_id: value.flow_id,
            node_id: value.node_id,
            provider: value.provider,
            start,
            end,
        })
    }
}

impl TryFrom<types::SpanContext> for WitSpanContext {
    type Error = types::GreenticError;

    fn try_from(value: types::SpanContext) -> MapperResult<Self> {
        let start_ms = value
            .start
            .as_ref()
            .map(offset_to_timestamp_ms)
            .transpose()?;
        let end_ms = value.end.as_ref().map(offset_to_timestamp_ms).transpose()?;

        Ok(Self {
            tenant: value.tenant.into(),
            session_id: value.session_id.map(|key| key.to_string()),
            flow_id: value.flow_id,
            node_id: value.node_id,
            provider: value.provider,
            start_ms,
            end_ms,
        })
    }
}

impl From<WitSignatureAlgorithm> for types::SignatureAlgorithm {
    fn from(value: WitSignatureAlgorithm) -> Self {
        match value {
            WitSignatureAlgorithm::Ed25519 => Self::Ed25519,
            WitSignatureAlgorithm::Other(v) => Self::Other(v),
        }
    }
}

impl From<types::SignatureAlgorithm> for WitSignatureAlgorithm {
    fn from(value: types::SignatureAlgorithm) -> Self {
        match value {
            types::SignatureAlgorithm::Ed25519 => Self::Ed25519,
            types::SignatureAlgorithm::Other(v) => Self::Other(v),
        }
    }
}

impl From<WitSignature> for types::Signature {
    fn from(value: WitSignature) -> Self {
        Self {
            key_id: value.key_id,
            algorithm: value.algorithm.into(),
            signature: value.signature,
        }
    }
}

impl From<types::Signature> for WitSignature {
    fn from(value: types::Signature) -> Self {
        Self {
            key_id: value.key_id,
            algorithm: value.algorithm.into(),
            signature: value.signature,
        }
    }
}

impl TryFrom<WitPackRef> for types::PackRef {
    type Error = types::GreenticError;

    fn try_from(value: WitPackRef) -> MapperResult<Self> {
        let version = Version::parse(&value.version)
            .map_err(|err| invalid_input(format!("invalid version: {err}")))?;
        Ok(Self {
            oci_url: value.oci_url,
            version,
            digest: value.digest,
            signatures: value.signatures.into_iter().map(Into::into).collect(),
        })
    }
}

impl From<types::PackRef> for WitPackRef {
    fn from(value: types::PackRef) -> Self {
        Self {
            oci_url: value.oci_url,
            version: value.version.to_string(),
            digest: value.digest,
            signatures: value.signatures.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tenant_ctx() -> types::TenantCtx {
        types::TenantCtx {
            env: types::EnvId::from("prod"),
            tenant: types::TenantId::from("tenant-1"),
            tenant_id: types::TenantId::from("tenant-1"),
            team: Some(types::TeamId::from("team-42")),
            team_id: Some(types::TeamId::from("team-42")),
            user: Some(types::UserId::from("user-7")),
            user_id: Some(types::UserId::from("user-7")),
            trace_id: Some("trace".into()),
            correlation_id: Some("corr".into()),
            deadline: Some(types::InvocationDeadline::from_unix_millis(
                1_700_000_000_000,
            )),
            attempt: 2,
            idempotency_key: Some("idem".into()),
            impersonation: Some(types::Impersonation {
                actor_id: types::UserId::from("actor"),
                reason: Some("maintenance".into()),
            }),
        }
    }

    #[test]
    fn tenant_ctx_roundtrip() {
        let ctx = sample_tenant_ctx();
        let wit = match WitTenantCtx::try_from(ctx.clone()) {
            Ok(value) => value,
            Err(err) => panic!("failed to map to wit: {err}"),
        };
        let back = match types::TenantCtx::try_from(wit) {
            Ok(value) => value,
            Err(err) => panic!("failed to map from wit: {err}"),
        };
        assert_eq!(back.env.as_str(), ctx.env.as_str());
        assert_eq!(back.tenant.as_str(), ctx.tenant.as_str());
        assert!(back.impersonation.is_some());
        assert!(ctx.impersonation.is_some());
        assert_eq!(
            back.impersonation.as_ref().map(|imp| imp.actor_id.as_str()),
            ctx.impersonation.as_ref().map(|imp| imp.actor_id.as_str())
        );
    }

    #[test]
    fn outcome_roundtrip() {
        let pending = types::Outcome::Pending {
            reason: "waiting".into(),
            expected_input: Some(vec!["input".into()]),
        };
        let wit = WitOutcome::from(pending.clone());
        let back = types::Outcome::from(wit);
        match back {
            types::Outcome::Pending { reason, .. } => {
                assert_eq!(reason, "waiting");
            }
            _ => panic!("expected pending"),
        }
    }
}
