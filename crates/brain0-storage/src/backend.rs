//! Stable helpers for implementing [`Storage`](crate::Storage) in out-of-tree backends.
//!
//! brain0 is **open core**: the public [`Storage`](crate::Storage) trait is the extension
//! interface and the local [`SqliteStorage`](crate::SqliteStorage) is the reference backend.
//! Team/hosted backends — for example the PostgreSQL + pgvector backend in the private
//! `brain0-enterprise` repo — live in separate workspaces and implement the *same* trait
//! against the *same* logical schema.
//!
//! To keep every backend serializing the decision graph identically, this module re-exports
//! the column lists and the column⇄model codecs used by the reference backend. They are the
//! one place the on-disk column layout is encoded; reuse them instead of re-deriving it.
//!
//! Stability: this is a deliberately small, documented surface intended for backend authors.
//! It does not expose any SQLite-specific connection or pragma internals.

pub use crate::sqlite::{
    change_kind_from_cols, change_kind_to_cols, source_from_cols, source_to_cols, ARTIFACT_COLS,
    AV_COLS, TASK_COLS, TV_COLS,
};

// Embedding-shingle blob codec, shared by all backends that persist fingerprints.
pub use crate::{decode_u64s, encode_u64s};
