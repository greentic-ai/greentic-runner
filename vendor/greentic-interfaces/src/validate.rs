//! Validation routines for WIT-derived metadata.

use greentic_types::error::{ErrorCode, GResult, GreenticError};
use semver::Version;

use crate::bindings;

fn invalid_input(message: impl Into<String>) -> GreenticError {
    GreenticError::new(ErrorCode::InvalidInput, message)
}

/// Validates provider metadata generated from WIT bindings.
pub fn validate_provider_meta(
    meta: bindings::greentic::interfaces_provider::provider::ProviderMeta,
) -> GResult<()> {
    if meta.name.trim().is_empty() {
        return Err(invalid_input("provider name must not be empty"));
    }

    Version::parse(&meta.version)
        .map_err(|err| invalid_input(format!("invalid semantic version: {err}")))?;

    for domain in &meta.allow_list.domains {
        if domain.trim().is_empty() {
            return Err(invalid_input(
                "allow-list domains must not contain empty entries",
            ));
        }
    }

    for port in &meta.allow_list.ports {
        if *port == 0 {
            return Err(invalid_input("allow-list ports must be greater than zero"));
        }
    }

    for protocol in &meta.allow_list.protocols {
        if let bindings::greentic::interfaces_types::types::Protocol::Custom(value) = protocol {
            if value.trim().is_empty() {
                return Err(invalid_input(
                    "custom protocol identifiers must not be empty",
                ));
            }
        }
    }

    if meta.network_policy.deny_on_miss {
        // strict policies must include at least one allowed entry to avoid total lockout
        let allow = &meta.network_policy.egress;
        if allow.domains.is_empty() && allow.ports.is_empty() && allow.protocols.is_empty() {
            return Err(invalid_input(
                "network policy denying misses requires explicit allow rules",
            ));
        }
    }

    Ok(())
}
