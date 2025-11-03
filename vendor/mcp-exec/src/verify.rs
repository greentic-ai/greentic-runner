//! Verification helpers that enforce digest and signature policies before execution.

use crate::config::VerifyPolicy;
use crate::error::VerificationError;
use crate::resolve::ResolvedArtifact;

#[derive(Clone, Debug)]
pub struct VerifiedArtifact {
    pub resolved: ResolvedArtifact,
    #[allow(dead_code)]
    pub verified_digest: Option<String>,
    #[allow(dead_code)]
    pub verified_signer: Option<String>,
}

pub fn verify(
    component: &str,
    artifact: ResolvedArtifact,
    policy: &VerifyPolicy,
) -> Result<VerifiedArtifact, VerificationError> {
    if let Some(expected_digest) = policy.required_digests.get(component) {
        if artifact.digest != *expected_digest {
            return Err(VerificationError::DigestMismatch {
                expected: expected_digest.clone(),
                actual: artifact.digest,
            });
        }
    } else if !policy.allow_unverified {
        return Err(VerificationError::UnsignedRejected);
    }

    // Signature verification will be added once the signing infrastructure is finalized.
    Ok(VerifiedArtifact {
        verified_digest: Some(artifact.digest.clone()),
        resolved: artifact,
        verified_signer: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VerifyPolicy;
    use crate::resolve;
    use crate::store::ToolStore;
    use std::path::PathBuf;

    #[test]
    fn rejects_unverified_when_policy_requires_digest() {
        let mut required = std::collections::HashMap::new();
        required.insert("tool".into(), "expected-digest".into());
        let policy = VerifyPolicy {
            allow_unverified: false,
            required_digests: required,
            ..Default::default()
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let wasm_path = tmp.path().join("tool.wasm");
        std::fs::write(&wasm_path, b"bytes").expect("write wasm");

        let artifact = resolve::resolve("tool", &ToolStore::LocalDir(PathBuf::from(tmp.path())))
            .expect("resolve");

        let err = verify("tool", artifact, &policy).expect_err("should fail");
        assert!(matches!(err, VerificationError::DigestMismatch { .. }));
    }

    #[test]
    fn allows_unsigned_when_policy_permits() {
        let policy = VerifyPolicy {
            allow_unverified: true,
            ..Default::default()
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let wasm_path = tmp.path().join("tool.wasm");
        std::fs::write(&wasm_path, b"bytes").expect("write wasm");

        let artifact = resolve::resolve("tool", &ToolStore::LocalDir(PathBuf::from(tmp.path())))
            .expect("resolve");

        let verified = verify("tool", artifact.clone(), &policy).expect("verify");
        assert_eq!(verified.resolved.digest, artifact.digest);
        assert_eq!(
            verified.verified_digest.as_deref(),
            Some(artifact.digest.as_str())
        );
        assert!(verified.verified_signer.is_none());
    }
}
