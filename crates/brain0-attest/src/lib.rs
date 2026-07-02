//! Signed AI-provenance attestations (`docs/attestation.md`).
//!
//! A [`Signer`] holds an Ed25519 key and signs the bytes of a statement (the in-toto Statement
//! JSON, assembled by the caller); [`verify`] checks a signature against a public key. Keys are
//! 32-byte Ed25519 seeds, hex-encoded; the `key_id` is a short fingerprint of the public key, for
//! rotation. The verify side needs only the public key — it can confirm an attestation without the
//! ability to mint one.

#![forbid(unsafe_code)]

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AttestError {
    #[error("invalid key: {0}")]
    Key(String),
    #[error("invalid signature encoding: {0}")]
    SignatureEncoding(String),
    #[error("signature verification failed")]
    BadSignature,
    #[error("entropy unavailable: {0}")]
    Entropy(String),
}

pub type Result<T> = std::result::Result<T, AttestError>;

/// Short, stable id for a public key (first 16 hex of the key) — for key rotation / selection.
#[must_use]
pub fn key_id_for(public_key_hex: &str) -> String {
    public_key_hex.chars().take(16).collect()
}

/// An Ed25519 signing identity. Can sign and expose its public key; cannot be reconstructed from
/// the public key alone (verify-only parties hold just the public key).
pub struct Signer {
    key: SigningKey,
}

// Redact the private key in Debug — only the public key id is shown, never key material.
impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("key_id", &self.key_id())
            .finish()
    }
}

impl Signer {
    /// Build from a 32-byte seed encoded as 64 hex chars.
    pub fn from_seed_hex(seed_hex: &str) -> Result<Self> {
        let bytes = hex::decode(seed_hex.trim()).map_err(|e| AttestError::Key(e.to_string()))?;
        let seed: [u8; 32] = bytes
            .try_into()
            .map_err(|_| AttestError::Key("seed must be 32 bytes (64 hex chars)".to_owned()))?;
        Ok(Self {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Generate a fresh signing key from the OS CSPRNG; returns the signer + its seed (hex) to persist.
    pub fn generate() -> Result<(Self, String)> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| AttestError::Entropy(e.to_string()))?;
        let seed_hex = hex::encode(seed);
        Ok((
            Self {
                key: SigningKey::from_bytes(&seed),
            },
            seed_hex,
        ))
    }

    #[must_use]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    #[must_use]
    pub fn key_id(&self) -> String {
        key_id_for(&self.public_key_hex())
    }

    /// Sign a message, returning the signature as hex (128 chars).
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> String {
        hex::encode(self.key.sign(message).to_bytes())
    }
}

/// Verify a hex signature over `message` against a hex-encoded Ed25519 public key.
pub fn verify(message: &[u8], signature_hex: &str, public_key_hex: &str) -> Result<()> {
    let pk_bytes: [u8; 32] = hex::decode(public_key_hex.trim())
        .map_err(|e| AttestError::Key(e.to_string()))?
        .try_into()
        .map_err(|_| AttestError::Key("public key must be 32 bytes".to_owned()))?;
    let verifying =
        VerifyingKey::from_bytes(&pk_bytes).map_err(|e| AttestError::Key(e.to_string()))?;
    let sig_bytes: [u8; 64] = hex::decode(signature_hex.trim())
        .map_err(|e| AttestError::SignatureEncoding(e.to_string()))?
        .try_into()
        .map_err(|_| AttestError::SignatureEncoding("signature must be 64 bytes".to_owned()))?;
    let signature = Signature::from_bytes(&sig_bytes);
    verifying
        .verify_strict(message, &signature)
        .map_err(|_| AttestError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_verify_roundtrips() {
        let (signer, seed) = Signer::generate().unwrap();
        let msg = b"in-toto statement bytes";
        let sig = signer.sign(msg);
        verify(msg, &sig, &signer.public_key_hex()).unwrap();
        // The seed reconstructs the same identity.
        let again = Signer::from_seed_hex(&seed).unwrap();
        assert_eq!(again.public_key_hex(), signer.public_key_hex());
        assert_eq!(again.key_id(), signer.key_id());
    }

    #[test]
    fn tampered_message_fails() {
        let (signer, _) = Signer::generate().unwrap();
        let sig = signer.sign(b"original");
        assert!(matches!(
            verify(b"tampered", &sig, &signer.public_key_hex()),
            Err(AttestError::BadSignature)
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let (a, _) = Signer::generate().unwrap();
        let (b, _) = Signer::generate().unwrap();
        let sig = a.sign(b"msg");
        assert!(verify(b"msg", &sig, &b.public_key_hex()).is_err());
    }

    #[test]
    fn malformed_inputs_error_not_panic() {
        assert!(Signer::from_seed_hex("zz").is_err());
        assert!(verify(b"m", "nothex", "nothex").is_err());
    }
}
