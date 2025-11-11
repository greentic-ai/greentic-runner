use std::convert::TryInto;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

pub struct PackVerifier {
    key: VerifyingKey,
}

impl PackVerifier {
    pub fn from_env_value(value: &str) -> Result<Self> {
        let (alg, key_b64) = value
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid PACK_PUBLIC_KEY format"))?;
        if !alg.eq_ignore_ascii_case("ed25519") {
            bail!("unsupported public key algorithm `{alg}`");
        }
        let raw = STANDARD_NO_PAD
            .decode(key_b64.trim())
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(key_b64.trim()))
            .context("PACK_PUBLIC_KEY is not valid base64")?;
        let key_bytes: &[u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("PACK_PUBLIC_KEY must be 32 bytes"))?;
        let key = VerifyingKey::from_bytes(key_bytes)
            .map_err(|_| anyhow!("PACK_PUBLIC_KEY is not a valid Ed25519 key"))?;
        Ok(Self { key })
    }

    pub fn verify(&self, message: &[u8], signature_b64: &str) -> Result<()> {
        let sig_bytes = STANDARD_NO_PAD
            .decode(signature_b64.trim())
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(signature_b64.trim()))
            .context("pack signature is not valid base64")?;
        let signature = Signature::from_slice(&sig_bytes)
            .map_err(|_| anyhow!("pack signature is not a valid Ed25519 signature"))?;
        self.key
            .verify(message, &signature)
            .context("pack signature verification failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn accepts_valid_signatures() {
        let secret = SigningKey::from_bytes(&[7u8; 32]);
        let public_b64 = STANDARD.encode(secret.verifying_key().as_bytes());
        let verifier = PackVerifier::from_env_value(&format!("ed25519:{public_b64}")).unwrap();
        let message = b"sha256:deadbeef";
        let signature = secret.sign(message);
        let signature_b64 = STANDARD.encode(signature.to_bytes());
        verifier.verify(message, &signature_b64).unwrap();
    }

    #[test]
    fn rejects_invalid_signatures() {
        let secret = SigningKey::from_bytes(&[1u8; 32]);
        let public_b64 = STANDARD.encode(secret.verifying_key().as_bytes());
        let verifier = PackVerifier::from_env_value(&format!("ed25519:{public_b64}")).unwrap();
        let bad_sig = STANDARD.encode([0xAAu8; 64]);
        assert!(verifier.verify(b"msg", &bad_sig).is_err());
    }
}
