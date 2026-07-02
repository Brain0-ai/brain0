//! Abstract, append-only storage for brain0.
//!
//! This is the **public, open-core** storage layer: the [`Storage`] trait (the stable
//! extension interface) plus the local [`SqliteStorage`] backend. Any **Postgres/pgvector**
//! backend is a premium/team capability and lives in the private `brain0-enterprise` repo,
//! which implements this same [`Storage`] trait.
//!
//! The graph is **append-only**: `append_*_version` and `put_edge` are insert-only and
//! idempotent on their deterministic keys; node rows carry only the evolving "current"
//! pointers and the two risk scalars, which may be updated (e.g. a-posteriori risk).
//!
//! Heavy content lives in a separate [`PayloadStore`]; the index holds only `*_ref`s
//!.

pub mod backend;
pub mod payload;
mod sqlite;
mod summary_cache;

pub use payload::{
    reference_for, EncryptedPayloadStore, FsPayloadStore, InMemoryPayloadStore, PayloadStore,
};
pub use sqlite::SqliteStorage;
pub use summary_cache::SummaryCacheDb;

use brain0_model::{ArtifactId, Level, TaskId, VersionId};
use brain0_model::{
    ArtifactNode, ArtifactVersion, Edge, EdgeKind, RiskState, TaskNode, TaskVersion, Timestamp,
};
use thiserror::Error;

/// Errors produced by the storage layer.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid stored data: {0}")]
    Invalid(String),
    #[error(transparent)]
    Crypto(#[from] brain0_crypto::CryptoError),
    /// Error surfaced by an out-of-tree [`Storage`] backend (e.g. a database driver). The
    /// open-core crate has no dependency on those drivers, so backends flatten their errors
    /// into this variant via [`StorageError::backend`] (stable extension API).
    #[error("backend error: {0}")]
    Backend(String),
}

impl StorageError {
    /// Wrap an out-of-tree backend's error (e.g. `postgres::Error`) into [`StorageError`].
    /// Lets a backend defined in another crate use `?` despite the orphan rule preventing it
    /// from implementing `From<DriverError> for StorageError` directly.
    pub fn backend(err: impl std::fmt::Display) -> Self {
        StorageError::Backend(err.to_string())
    }
}

/// Convenience result type for storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// A semantic-search candidate: a task node with its cosine similarity to the query and
/// its creation time (so the search layer can apply a recency bias —).
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub task_id: TaskId,
    pub cosine: f32,
    pub created_at: Timestamp,
}

/// An append-only security audit entry. Never contains secrets/payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub timestamp: Timestamp,
    pub event_type: String,
    pub detail: String,
}

/// The result of a content-addressed integrity check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityStatus {
    /// Stored content matches its content-addressed reference.
    Ok,
    /// Stored content does not match its reference (corrupted or tampered).
    Corrupt,
    /// Nothing is stored for this reference (e.g. purged).
    Missing,
}

/// Verify that the content behind a payload reference still hashes to that reference. Works
/// for any [`PayloadStore`] (the encrypted store decrypts before hashing the plaintext).
pub fn verify_payload(
    store: &dyn PayloadStore,
    reference: &brain0_model::PayloadRef,
) -> Result<IntegrityStatus> {
    match store.get(reference)? {
        None => Ok(IntegrityStatus::Missing),
        Some(content) if &reference_for(&content) == reference => Ok(IntegrityStatus::Ok),
        Some(_) => Ok(IntegrityStatus::Corrupt),
    }
}

/// The append-only graph store. All methods are synchronous; a single backend instance is
/// `Send` and is used from one thread at a time. Concurrency across writers is achieved by
/// opening one instance per client against the same database.
pub trait Storage {
    /// Apply the schema (idempotent).
    fn migrate(&self) -> Result<()>;

    // --- artifact nodes ---
    fn put_artifact_node(&self, node: &ArtifactNode) -> Result<()>;
    fn get_artifact_node(&self, id: &ArtifactId) -> Result<Option<ArtifactNode>>;
    fn find_artifact_by_path(
        &self,
        repo: &str,
        level: Level,
        qualified_path: &str,
    ) -> Result<Option<ArtifactNode>>;
    fn list_artifacts(&self, repo: &str, level: Level) -> Result<Vec<ArtifactNode>>;
    fn children(&self, parent: &ArtifactId) -> Result<Vec<ArtifactNode>>;
    fn update_artifact_risk(&self, id: &ArtifactId, risk: RiskState) -> Result<()>;

    // --- task nodes ---
    fn put_task_node(&self, node: &TaskNode) -> Result<()>;
    fn get_task_node(&self, id: &TaskId) -> Result<Option<TaskNode>>;

    // --- versions (append-only) ---
    fn append_artifact_version(&self, version: &ArtifactVersion) -> Result<()>;
    fn get_artifact_version(&self, id: &VersionId) -> Result<Option<ArtifactVersion>>;
    /// Version chain for an artifact, oldest first (the "by place / timeline" axis).
    fn artifact_versions(&self, artifact_id: &ArtifactId) -> Result<Vec<ArtifactVersion>>;
    fn append_task_version(&self, version: &TaskVersion) -> Result<()>;
    fn task_versions(&self, task_id: &TaskId) -> Result<Vec<TaskVersion>>;

    // --- edges (append-only, idempotent) ---
    fn put_edge(&self, edge: &Edge) -> Result<()>;
    fn out_edges(&self, kind: EdgeKind, src: &str) -> Result<Vec<Edge>>;
    fn in_edges(&self, kind: EdgeKind, dst: &str) -> Result<Vec<Edge>>;

    // --- current fingerprint (hash + shingles) for identity resolution ---
    /// Store the current structural fingerprint of an artifact (hash + shingles).
    fn put_artifact_fingerprint(
        &self,
        artifact_id: &ArtifactId,
        hash: &str,
        shingles: &[u64],
    ) -> Result<()>;
    /// Fetch the current structural fingerprint of an artifact.
    fn get_artifact_fingerprint(
        &self,
        artifact_id: &ArtifactId,
    ) -> Result<Option<(String, Vec<u64>)>>;

    // --- embedding model metadata: fixed dimension per store ---
    /// Record the embedding model + dimension for this store (idempotent).
    fn set_embedding_meta(&self, model: &str, dim: usize) -> Result<()>;
    /// The store's embedding `(model, dim)`, if set.
    fn get_embedding_meta(&self) -> Result<Option<(String, usize)>>;

    // --- embeddings + vector search ---
    /// Store a task embedding. Rejects a vector whose length disagrees with the store's
    /// declared `embedding_dim` (mixing dimensions is forbidden —).
    fn put_task_embedding(&self, task_id: &TaskId, vector: &[f32]) -> Result<()>;
    /// Top-`k` task nodes by cosine similarity to `query`.
    fn search_tasks_by_vector(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;

    // --- append-only ingest cursors ---
    /// The byte offset processed so far for `(adapter, file)`, if any.
    fn get_cursor(&self, adapter: &str, file: &str) -> Result<Option<u64>>;
    /// Persist the byte offset processed for `(adapter, file)`.
    fn set_cursor(&self, adapter: &str, file: &str, offset: u64) -> Result<()>;

    // --- append-only security audit log ---
    /// Append an audit event (no secret values / payload).
    fn append_audit(&self, event_type: &str, detail: &str, at: Timestamp) -> Result<()>;
    /// The most recent audit events (newest first), up to `limit`.
    fn list_audit(&self, limit: usize) -> Result<Vec<AuditEvent>>;

    /// All distinct payload references stored in the index (for integrity verification).
    fn all_payload_refs(&self) -> Result<Vec<brain0_model::PayloadRef>>;
    /// All task node ids (for re-embedding migration).
    fn all_task_ids(&self) -> Result<Vec<TaskId>>;
    /// Distinct repo ids present in the index (read-side repo inference, e.g. `brain0 report`).
    /// Additive with a conservative default so out-of-tree backends keep compiling; backends
    /// should override it (the SQLite backend does).
    fn repos(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    // --- checkpoint manifests (the `watch` safety net; `brain0 rewind` reads these) ---
    /// Record one file of a checkpoint's full-tree manifest. Additive with erroring defaults
    /// so out-of-tree backends keep compiling; the SQLite backend implements them.
    fn put_checkpoint_file(
        &self,
        checkpoint_id: &str,
        rel_path: &str,
        content_ref: &brain0_model::PayloadRef,
        at: &Timestamp,
    ) -> Result<()> {
        let _ = (checkpoint_id, rel_path, content_ref, at);
        Err(StorageError::backend(
            "checkpoint manifests unsupported by this backend",
        ))
    }
    /// A checkpoint's manifest: `(rel_path, content_ref)` pairs, sorted by path.
    fn checkpoint_files(
        &self,
        checkpoint_id: &str,
    ) -> Result<Vec<(String, brain0_model::PayloadRef)>> {
        let _ = checkpoint_id;
        Ok(Vec::new())
    }
    /// Recorded checkpoints as `(checkpoint_id, at, file_count)`, newest first.
    fn list_checkpoints(&self) -> Result<Vec<(String, Timestamp, usize)>> {
        Ok(Vec::new())
    }

    /// Remove every task embedding (used before a re-embedding migration —).
    fn clear_all_embeddings(&self) -> Result<()>;

    // --- purge / crypto-shred: topology stays, payload becomes a tombstone ---
    /// Mark every version referencing `reference` as `payload_purged` (keeps the node/edge
    /// topology intact — append-only — while recording the content is gone).
    fn mark_payload_purged(&self, reference: &brain0_model::PayloadRef) -> Result<()>;
    /// Delete a task's embedding (invalidate the derived vector when its payload is purged).
    fn delete_task_embedding(&self, task_id: &TaskId) -> Result<()>;
    /// Payload references attached to a task's versions (prompt + decision summary).
    fn task_payload_refs(&self, task_id: &TaskId) -> Result<Vec<brain0_model::PayloadRef>>;
    /// `(task_id, payload_ref)` for task-version payloads older than `cutoff` and not yet
    /// purged (for retention).
    fn payloads_older_than(
        &self,
        cutoff: Timestamp,
    ) -> Result<Vec<(TaskId, brain0_model::PayloadRef)>>;
}

/// Default dimension of the local (offline) embedding. Must match the TypeScript
/// `LocalEmbeddingProvider` default so the Rust ingest and the TS indexer/agent share one
/// vector space.
pub const LOCAL_EMBED_DIM: usize = 256;

fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for unit in text.encode_utf16() {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Deterministic, offline embedding via signed feature hashing — a faithful Rust port of
/// the TypeScript `localEmbed`, so vectors produced on either side are interchangeable.
#[must_use]
pub fn local_embed(text: &str, dim: usize) -> Vec<f32> {
    let mut vec = vec![0.0f32; dim];
    for token in text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let bucket = (fnv1a(token) as usize) % dim;
        let sign = if fnv1a(&format!("sign:{token}")) & 1 == 1 {
            1.0
        } else {
            -1.0
        };
        vec[bucket] += sign;
    }
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut vec {
            *x /= norm;
        }
    }
    vec
}

/// Enforce TLS on a Postgres connection string: refuse anything weaker than
/// `sslmode=verify-full`/`verify-ca` — no plaintext fallback. Pure + testable without a
/// server.
pub fn require_tls(conn_str: &str) -> Result<()> {
    let lower = conn_str.to_lowercase();
    if lower.contains("sslmode=verify-full") || lower.contains("sslmode=verify-ca") {
        Ok(())
    } else {
        Err(StorageError::Invalid(
            "refusing Postgres connection without sslmode=verify-full (no plaintext fallback)"
                .to_owned(),
        ))
    }
}

/// Cosine similarity of two equal-length vectors. Returns 0.0 for mismatched lengths or
/// zero-norm inputs.
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Encode an `f32` slice as little-endian bytes (used for the SQLite embedding BLOB).
#[must_use]
pub fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode little-endian bytes back into an `f32` vector.
#[must_use]
pub fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Encode a `u64` slice (fingerprint shingles) as little-endian bytes.
#[must_use]
pub fn encode_u64s(values: &[u64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 8);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Decode little-endian bytes back into a `u64` vector.
#[must_use]
pub fn decode_u64s(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks_exact(8)
        .map(|chunk| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(chunk);
            u64::from_le_bytes(buf)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn vector_codec_roundtrips() {
        let v = vec![1.5f32, -2.0, 0.0, 3.25];
        assert_eq!(decode_vector(&encode_vector(&v)), v);
    }

    #[test]
    fn local_embed_is_deterministic_and_normalized() {
        let a = local_embed("fix the parser bug", 256);
        let b = local_embed("fix the parser bug", 256);
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn local_embed_similar_beats_unrelated() {
        let q = local_embed("fix the parser bug", 256);
        let similar = local_embed("parser bug fix needed", 256);
        let unrelated = local_embed("banana smoothie recipe", 256);
        assert!(cosine(&q, &similar) > cosine(&q, &unrelated));
    }

    /// Golden vector locking the algorithm so the Rust and TS embedders stay in lock-step.
    /// The matching TS test (`packages/agent/src/embeddings.test.ts`) asserts the same numbers.
    #[test]
    fn require_tls_rejects_plaintext_and_accepts_verify_full() {
        assert!(require_tls("host=db user=u sslmode=verify-full").is_ok());
        assert!(require_tls("postgresql://u@db?sslmode=verify-ca").is_ok());
        assert!(require_tls("host=db user=u").is_err()); // no sslmode → rejected
        assert!(require_tls("host=db sslmode=require").is_err()); // weaker than verify → rejected
    }

    #[test]
    fn local_embed_golden() {
        let v = local_embed("brain zero", 8);
        let rounded: Vec<f32> = v.iter().map(|x| (x * 1000.0).round() / 1000.0).collect();
        assert_eq!(rounded, vec![0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0]);
    }
}
