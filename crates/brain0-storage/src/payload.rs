//! Heavy payload storage — the dedicated store for prompts, transcripts, diffs, and
//! decision summaries, kept separate from the light index.
//!
//! Payloads are content-addressed (BLAKE3): writing identical content twice yields the
//! same [`PayloadRef`] and stores it once. The index only ever holds the `PayloadRef`; the
//! content is hydrated on demand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use brain0_crypto::{
    restrict_dir_permissions, restrict_permissions, EncryptedBlob, Envelope, KeyProvider,
};
use brain0_model::PayloadRef;

use crate::{Result, StorageError};

const REF_SCHEME: &str = "blake3:";

/// A pluggable heavy-payload store (local filesystem, or a remote object store).
pub trait PayloadStore: Send + Sync {
    /// Store raw content and return its content-addressed reference.
    fn put(&self, content: &[u8]) -> Result<PayloadRef>;

    /// Fetch content by reference, or `None` if it is not present.
    fn get(&self, reference: &PayloadRef) -> Result<Option<Vec<u8>>>;

    /// Destroy the content behind a reference, making it irrecoverable. For
    /// the encrypted store this is crypto-shredding (the only wrapped DEK is deleted with
    /// the blob). Returns whether anything was removed.
    fn shred(&self, reference: &PayloadRef) -> Result<bool>;

    /// Convenience: store UTF-8 text.
    fn put_str(&self, text: &str) -> Result<PayloadRef> {
        self.put(text.as_bytes())
    }

    /// Convenience: fetch content as UTF-8 text (lossy).
    fn get_str(&self, reference: &PayloadRef) -> Result<Option<String>> {
        Ok(self
            .get(reference)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }
}

/// The content-addressed reference for some bytes (without storing them). Useful when the
/// reference must be known before/independently of writing.
#[must_use]
pub fn reference_for(content: &[u8]) -> PayloadRef {
    PayloadRef::new(format!("{REF_SCHEME}{}", blake3::hash(content).to_hex()))
}

/// Filesystem-backed payload store: one file per blob under a dedicated directory, sharded
/// by the first two hex chars of the hash to keep directories small.
#[derive(Debug)]
pub struct FsPayloadStore {
    root: PathBuf,
}

impl FsPayloadStore {
    /// Open (creating if needed) a payload store rooted at `root`, with owner-only
    /// directory permissions.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        restrict_dir_permissions(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, hex: &str) -> PathBuf {
        let (shard, rest) = hex.split_at(2.min(hex.len()));
        self.root.join(shard).join(format!("{rest}.blob"))
    }
}

impl PayloadStore for FsPayloadStore {
    fn put(&self, content: &[u8]) -> Result<PayloadRef> {
        let hex = blake3::hash(content).to_hex().to_string();
        let path = self.path_for(&hex);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Write to a temp file then rename for atomicity.
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, content)?;
            std::fs::rename(&tmp, &path)?;
            restrict_permissions(&path)?;
        }
        Ok(PayloadRef::new(format!("{REF_SCHEME}{hex}")))
    }

    fn get(&self, reference: &PayloadRef) -> Result<Option<Vec<u8>>> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        let path = self.path_for(hex);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn shred(&self, reference: &PayloadRef) -> Result<bool> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        match std::fs::remove_file(self.path_for(hex)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }
}

/// In-memory payload store, for tests and ephemeral runs.
#[derive(Debug, Default)]
pub struct InMemoryPayloadStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl InMemoryPayloadStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PayloadStore for InMemoryPayloadStore {
    fn put(&self, content: &[u8]) -> Result<PayloadRef> {
        let hex = blake3::hash(content).to_hex().to_string();
        self.blobs
            .lock()
            .expect("payload mutex not poisoned")
            .entry(hex.clone())
            .or_insert_with(|| content.to_vec());
        Ok(PayloadRef::new(format!("{REF_SCHEME}{hex}")))
    }

    fn get(&self, reference: &PayloadRef) -> Result<Option<Vec<u8>>> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        Ok(self
            .blobs
            .lock()
            .expect("payload mutex not poisoned")
            .get(hex)
            .cloned())
    }

    fn shred(&self, reference: &PayloadRef) -> Result<bool> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        Ok(self
            .blobs
            .lock()
            .expect("payload mutex not poisoned")
            .remove(hex)
            .is_some())
    }
}

/// Filesystem payload store with **application-layer envelope encryption** at rest
///: each blob is sealed with a per-blob DEK wrapped by the KEK from a
/// [`KeyProvider`]. Refs are content-addressed on the *plaintext* (so they match what the
/// index stored and dedup works), while the on-disk content is ciphertext. For the remote
/// deployment this is client-side encryption: the server never sees plaintext.
#[derive(Debug)]
pub struct EncryptedPayloadStore<K: KeyProvider> {
    root: PathBuf,
    envelope: Envelope<K>,
}

impl<K: KeyProvider> EncryptedPayloadStore<K> {
    /// Open (creating if needed) an encrypted store at `root` with the given key provider.
    pub fn open(root: impl AsRef<Path>, provider: K) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        restrict_dir_permissions(&root)?;
        Ok(Self {
            root,
            envelope: Envelope::new(provider),
        })
    }

    fn path_for(&self, hex: &str) -> PathBuf {
        let (shard, rest) = hex.split_at(2.min(hex.len()));
        self.root.join(shard).join(format!("{rest}.enc"))
    }
}

impl<K: KeyProvider + Send + Sync> PayloadStore for EncryptedPayloadStore<K> {
    fn put(&self, content: &[u8]) -> Result<PayloadRef> {
        let hex = blake3::hash(content).to_hex().to_string();
        let path = self.path_for(&hex);
        if !path.exists() {
            let bytes = self.envelope.encrypt(content)?.to_bytes();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            restrict_permissions(&path)?;
        }
        Ok(PayloadRef::new(format!("{REF_SCHEME}{hex}")))
    }

    fn get(&self, reference: &PayloadRef) -> Result<Option<Vec<u8>>> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        let path = self.path_for(hex);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let blob = EncryptedBlob::from_bytes(&bytes)?;
                Ok(Some(self.envelope.decrypt(&blob)?))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Crypto-shred: delete the blob (its only wrapped DEK + ciphertext), irrecoverable.
    fn shred(&self, reference: &PayloadRef) -> Result<bool> {
        let hex = reference
            .as_str()
            .strip_prefix(REF_SCHEME)
            .ok_or_else(|| StorageError::Invalid(format!("bad payload ref: {reference}")))?;
        match std::fs::remove_file(self.path_for(hex)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_crypto::{StaticKeyProvider, KEY_LEN};

    fn roundtrip(store: &dyn PayloadStore) {
        let r1 = store.put_str("hello brain0").unwrap();
        let r2 = store.put_str("hello brain0").unwrap();
        assert_eq!(r1, r2, "content-addressed: same content → same ref");
        assert_eq!(store.get_str(&r1).unwrap().as_deref(), Some("hello brain0"));
        let missing = reference_for(b"never stored");
        assert_eq!(store.get(&missing).unwrap(), None);
    }

    #[test]
    fn in_memory_roundtrip() {
        roundtrip(&InMemoryPayloadStore::new());
    }

    #[test]
    fn fs_roundtrip() {
        let dir = std::env::temp_dir().join(format!("brain0-payload-test-{}", std::process::id()));
        let store = FsPayloadStore::open(&dir).unwrap();
        roundtrip(&store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypted_roundtrip_and_ciphertext_is_opaque() {
        let dir = std::env::temp_dir().join(format!("brain0-enc-payload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let provider = StaticKeyProvider::new("k1", [3u8; KEY_LEN]);
        let store = EncryptedPayloadStore::open(&dir, provider).unwrap();
        let secret = "prompt with AKIA-like secret content";
        let r = store.put_str(secret).unwrap();
        assert_eq!(store.get_str(&r).unwrap().as_deref(), Some(secret));

        // On disk it's ciphertext, not the plaintext.
        let hex = r.as_str().strip_prefix("blake3:").unwrap();
        let (shard, rest) = hex.split_at(2);
        let on_disk = std::fs::read(dir.join(shard).join(format!("{rest}.enc"))).unwrap();
        assert!(!on_disk.windows(6).any(|w| w == b"secret"));

        // Crypto-shred makes it irrecoverable; the ref now resolves to nothing.
        assert!(store.shred(&r).unwrap());
        assert_eq!(store.get(&r).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn integrity_verification_detects_corruption() {
        use crate::{verify_payload, IntegrityStatus};
        let dir = std::env::temp_dir().join(format!("brain0-integrity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FsPayloadStore::open(&dir).unwrap();
        let r = store.put_str("decision summary content").unwrap();
        assert_eq!(verify_payload(&store, &r).unwrap(), IntegrityStatus::Ok);

        // Tamper with the on-disk blob → integrity check fails.
        let hex = r.as_str().strip_prefix("blake3:").unwrap();
        let (shard, rest) = hex.split_at(2);
        std::fs::write(dir.join(shard).join(format!("{rest}.blob")), b"tampered").unwrap();
        assert_eq!(
            verify_payload(&store, &r).unwrap(),
            IntegrityStatus::Corrupt
        );

        let missing = reference_for(b"never stored at all");
        assert_eq!(
            verify_payload(&store, &missing).unwrap(),
            IntegrityStatus::Missing
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let dir = std::env::temp_dir().join(format!("brain0-enc-wrong-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = {
            let store =
                EncryptedPayloadStore::open(&dir, StaticKeyProvider::new("k", [1u8; KEY_LEN]))
                    .unwrap();
            store.put_str("top secret").unwrap()
        };
        let other =
            EncryptedPayloadStore::open(&dir, StaticKeyProvider::new("k", [2u8; KEY_LEN])).unwrap();
        assert!(
            other.get(&r).is_err(),
            "wrong key must fail closed, not return data"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
