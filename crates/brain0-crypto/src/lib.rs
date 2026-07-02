//! Envelope encryption and key management for brain0.
//!
//! Each blob is sealed with a fresh random **DEK** (data encryption key) using
//! ChaCha20-Poly1305 AEAD; the DEK is then wrapped with a **KEK** (key encryption key)
//! supplied by a [`KeyProvider`] (OS env / restricted file / keystore). This gives:
//!
//! * **at-rest protection** — the server/disk never sees the DEK in clear;
//! * **KEK rotation** without re-encrypting payloads ([`Envelope::rewrap`] only re-wraps the
//!   small DEK);
//! * **crypto-shredding** — destroying a blob (its only wrapped DEK) makes the content
//!   irrecoverable.
//!
//! Fail-closed: a missing/invalid key is an error; brain0 never falls back to plaintext.

#![forbid(unsafe_code)]

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use thiserror::Error;

/// Errors from the crypto layer.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("key unavailable: {0}")]
    KeyUnavailable(String),
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("decryption failed (wrong key or tampered data)")]
    DecryptFailed,
    #[error("encryption failed")]
    EncryptFailed,
    #[error("malformed encrypted blob")]
    Malformed,
    #[error("rng failure: {0}")]
    Rng(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, CryptoError>;

/// Length of a key in bytes (256-bit).
pub const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

fn random_bytes(buf: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buf).map_err(|e| CryptoError::Rng(e.to_string()))
}

fn seal(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<([u8; NONCE_LEN], Vec<u8>)> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; NONCE_LEN];
    random_bytes(&mut nonce)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| CryptoError::EncryptFailed)?;
    Ok((nonce, ciphertext))
}

fn open(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| CryptoError::DecryptFailed)
}

/// Supplies the key-encryption key. Implementations must fail-closed.
pub trait KeyProvider {
    /// Stable identifier of the current KEK (for rotation bookkeeping).
    fn kek_id(&self) -> &str;
    /// The 32-byte KEK, or an error if unavailable.
    fn kek(&self) -> Result<[u8; KEY_LEN]>;
}

/// An in-memory KEK (tests, or a key already loaded from a keystore).
#[derive(Debug, Clone)]
pub struct StaticKeyProvider {
    id: String,
    key: [u8; KEY_LEN],
}

impl StaticKeyProvider {
    #[must_use]
    pub fn new(id: impl Into<String>, key: [u8; KEY_LEN]) -> Self {
        Self { id: id.into(), key }
    }

    /// Generate a fresh random KEK (e.g. for first-run bootstrap).
    pub fn generate(id: impl Into<String>) -> Result<Self> {
        let mut key = [0u8; KEY_LEN];
        random_bytes(&mut key)?;
        Ok(Self::new(id, key))
    }
}

impl KeyProvider for StaticKeyProvider {
    fn kek_id(&self) -> &str {
        &self.id
    }
    fn kek(&self) -> Result<[u8; KEY_LEN]> {
        Ok(self.key)
    }
}

/// Reads the KEK from an environment variable (64 hex chars). Fail-closed if unset/invalid.
#[derive(Debug, Clone)]
pub struct EnvKeyProvider {
    id: String,
    var: String,
}

impl EnvKeyProvider {
    #[must_use]
    pub fn new(id: impl Into<String>, var: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            var: var.into(),
        }
    }
}

impl KeyProvider for EnvKeyProvider {
    fn kek_id(&self) -> &str {
        &self.id
    }
    fn kek(&self) -> Result<[u8; KEY_LEN]> {
        let value = std::env::var(&self.var)
            .map_err(|_| CryptoError::KeyUnavailable(format!("env {} not set", self.var)))?;
        decode_key(value.trim())
    }
}

/// Reads the KEK from a restricted (0600) file containing 64 hex chars; can generate one
/// on first use.
#[derive(Debug, Clone)]
pub struct FileKeyProvider {
    id: String,
    path: std::path::PathBuf,
    generate_if_missing: bool,
}

impl FileKeyProvider {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        path: impl Into<std::path::PathBuf>,
        generate_if_missing: bool,
    ) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            generate_if_missing,
        }
    }
}

impl KeyProvider for FileKeyProvider {
    fn kek_id(&self) -> &str {
        &self.id
    }
    fn kek(&self) -> Result<[u8; KEY_LEN]> {
        match std::fs::read_to_string(&self.path) {
            Ok(content) => decode_key(content.trim()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.generate_if_missing => {
                let mut key = [0u8; KEY_LEN];
                random_bytes(&mut key)?;
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&self.path, hex::encode(key))?;
                restrict_permissions(&self.path)?;
                Ok(key)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CryptoError::KeyUnavailable(
                format!("key file {} missing", self.path.display()),
            )),
            Err(e) => Err(CryptoError::Io(e)),
        }
    }
}

fn decode_key(s: &str) -> Result<[u8; KEY_LEN]> {
    let bytes = hex::decode(s).map_err(|e| CryptoError::InvalidKey(e.to_string()))?;
    if bytes.len() != KEY_LEN {
        return Err(CryptoError::InvalidKey(format!(
            "expected {KEY_LEN} bytes, got {}",
            bytes.len()
        )));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// Restrict a file to owner-only (0600) on Unix; no-op elsewhere.
pub fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Restrict a directory to owner-only (0700) on Unix; no-op elsewhere.
pub fn restrict_dir_permissions(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

const MAGIC: &[u8; 4] = b"B0E1";

/// A sealed blob: ciphertext plus the KEK-wrapped DEK and nonces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedBlob {
    pub kek_id: String,
    pub data_nonce: [u8; NONCE_LEN],
    pub dek_nonce: [u8; NONCE_LEN],
    pub wrapped_dek: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl EncryptedBlob {
    /// Serialize to a self-describing byte frame.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        let id = self.kek_id.as_bytes();
        out.extend_from_slice(&(id.len() as u16).to_le_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(&self.data_nonce);
        out.extend_from_slice(&self.dek_nonce);
        out.extend_from_slice(&(self.wrapped_dek.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.wrapped_dek);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parse a byte frame.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut cur = 0usize;
        let take = |cur: &mut usize, n: usize| -> Result<&[u8]> {
            let end = cur.checked_add(n).ok_or(CryptoError::Malformed)?;
            let slice = bytes.get(*cur..end).ok_or(CryptoError::Malformed)?;
            *cur = end;
            Ok(slice)
        };
        if take(&mut cur, 4)? != MAGIC {
            return Err(CryptoError::Malformed);
        }
        let id_len = u16::from_le_bytes(take(&mut cur, 2)?.try_into().unwrap()) as usize;
        let kek_id = String::from_utf8(take(&mut cur, id_len)?.to_vec())
            .map_err(|_| CryptoError::Malformed)?;
        let data_nonce: [u8; NONCE_LEN] = take(&mut cur, NONCE_LEN)?.try_into().unwrap();
        let dek_nonce: [u8; NONCE_LEN] = take(&mut cur, NONCE_LEN)?.try_into().unwrap();
        let wd_len = u16::from_le_bytes(take(&mut cur, 2)?.try_into().unwrap()) as usize;
        let wrapped_dek = take(&mut cur, wd_len)?.to_vec();
        let ciphertext = bytes.get(cur..).ok_or(CryptoError::Malformed)?.to_vec();
        Ok(Self {
            kek_id,
            data_nonce,
            dek_nonce,
            wrapped_dek,
            ciphertext,
        })
    }
}

/// Envelope-encryption operations over a [`KeyProvider`].
#[derive(Debug)]
pub struct Envelope<K: KeyProvider> {
    provider: K,
}

impl<K: KeyProvider> Envelope<K> {
    #[must_use]
    pub fn new(provider: K) -> Self {
        Self { provider }
    }

    /// Encrypt plaintext with a fresh DEK wrapped by the current KEK.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<EncryptedBlob> {
        let kek = self.provider.kek()?;
        let mut dek = [0u8; KEY_LEN];
        random_bytes(&mut dek)?;
        let (data_nonce, ciphertext) = seal(&dek, plaintext)?;
        let (dek_nonce, wrapped_dek) = seal(&kek, &dek)?;
        Ok(EncryptedBlob {
            kek_id: self.provider.kek_id().to_owned(),
            data_nonce,
            dek_nonce,
            wrapped_dek,
            ciphertext,
        })
    }

    /// Decrypt a blob (fails closed if the KEK is wrong or data tampered).
    pub fn decrypt(&self, blob: &EncryptedBlob) -> Result<Vec<u8>> {
        let kek = self.provider.kek()?;
        let dek_vec = open(&kek, &blob.dek_nonce, &blob.wrapped_dek)?;
        let dek: [u8; KEY_LEN] = dek_vec
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Malformed)?;
        open(&dek, &blob.data_nonce, &blob.ciphertext)
    }

    /// Re-wrap a blob's DEK under a new KEK (rotation) without touching the ciphertext.
    pub fn rewrap<N: KeyProvider>(
        &self,
        blob: &EncryptedBlob,
        new: &Envelope<N>,
    ) -> Result<EncryptedBlob> {
        let kek = self.provider.kek()?;
        let dek_vec = open(&kek, &blob.dek_nonce, &blob.wrapped_dek)?;
        let dek: [u8; KEY_LEN] = dek_vec
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::Malformed)?;
        let new_kek = new.provider.kek()?;
        let (dek_nonce, wrapped_dek) = seal(&new_kek, &dek)?;
        Ok(EncryptedBlob {
            kek_id: new.provider.kek_id().to_owned(),
            data_nonce: blob.data_nonce,
            dek_nonce,
            wrapped_dek,
            ciphertext: blob.ciphertext.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str) -> StaticKeyProvider {
        StaticKeyProvider::new(id, [7u8; KEY_LEN])
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let env = Envelope::new(provider("k1"));
        let blob = env.encrypt(b"a secret prompt").unwrap();
        assert_eq!(env.decrypt(&blob).unwrap(), b"a secret prompt");
        // Ciphertext does not contain the plaintext.
        assert!(!blob.ciphertext.windows(6).any(|w| w == b"secret"));
    }

    #[test]
    fn wrong_key_fails_closed() {
        let env = Envelope::new(StaticKeyProvider::new("k1", [1u8; KEY_LEN]));
        let blob = env.encrypt(b"data").unwrap();
        let other = Envelope::new(StaticKeyProvider::new("k2", [2u8; KEY_LEN]));
        assert!(matches!(
            other.decrypt(&blob),
            Err(CryptoError::DecryptFailed)
        ));
    }

    #[test]
    fn blob_serialization_roundtrips() {
        let env = Envelope::new(provider("k1"));
        let blob = env.encrypt(b"hello").unwrap();
        let bytes = blob.to_bytes();
        assert_eq!(EncryptedBlob::from_bytes(&bytes).unwrap(), blob);
        assert!(EncryptedBlob::from_bytes(b"xx").is_err());
    }

    #[test]
    fn kek_rotation_preserves_plaintext_without_touching_ciphertext() {
        let old = Envelope::new(StaticKeyProvider::new("old", [1u8; KEY_LEN]));
        let new = Envelope::new(StaticKeyProvider::new("new", [9u8; KEY_LEN]));
        let blob = old.encrypt(b"rotate me").unwrap();
        let rewrapped = old.rewrap(&blob, &new).unwrap();
        assert_eq!(rewrapped.ciphertext, blob.ciphertext); // payload not re-encrypted
        assert_eq!(rewrapped.kek_id, "new");
        assert_eq!(new.decrypt(&rewrapped).unwrap(), b"rotate me");
        assert!(old.decrypt(&rewrapped).is_err()); // old KEK no longer opens it
    }

    #[test]
    fn env_provider_fails_closed_when_missing() {
        let p = EnvKeyProvider::new("e", "BRAIN0_TEST_NONEXISTENT_KEK");
        assert!(matches!(p.kek(), Err(CryptoError::KeyUnavailable(_))));
    }

    #[test]
    fn file_provider_generates_and_reads_back() {
        let path = std::env::temp_dir().join(format!("brain0-kek-{}.key", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = FileKeyProvider::new("f", &path, true);
        let k1 = p.kek().unwrap();
        let k2 = p.kek().unwrap(); // read back, stable
        assert_eq!(k1, k2);
        let _ = std::fs::remove_file(&path);
    }
}
