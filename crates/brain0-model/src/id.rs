//! Identifiers for graph entities.
//!
//! All ids are string newtypes. The "stable" ids of persistent entities (artifacts,
//! tasks) and of versions are **deterministic and content-addressed**: derived with a
//! domain-separated BLAKE3 hash so that two machines observing the same entity in the
//! same state compute the *same* id. This is what lets independent observers converge on
//! one shared graph without merge logic.
//!
//! The deterministic constructors (`ArtifactId::derive`, `VersionId::for_artifact`, ...)
//! live in the modules that own the relevant domain types, but they all route through the
//! crate-private [`hashed`] helper defined here.

use serde::{Deserialize, Serialize};

/// Domain-separated, content-addressed hash → 32 lowercase hex chars (128 bits).
///
/// A unit-separator byte (`0x1f`) is inserted between parts so that, e.g.,
/// `["ab", "c"]` and `["a", "bc"]` never collide.
pub(crate) fn hashed(domain: &str, parts: &[&[u8]]) -> String {
    use std::fmt::Write as _;

    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(&[0x1f]);
        hasher.update(part);
    }
    let hash = hasher.finalize();
    let bytes = hash.as_bytes();

    let mut out = String::with_capacity(32);
    for byte in &bytes[..16] {
        // Infallible: writing to a String never errors.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an already-formed id string (e.g. read back from storage).
            pub fn new(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume and return the underlying string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

define_id!(
    /// Stable identity of an artifact node (a symbol or, in fallback, a file).
    ///
    /// Format: `art_<32 hex>`. Constructed deterministically via [`ArtifactId::derive`].
    ArtifactId
);

define_id!(
    /// Stable identity of a task node (an intent: a prompt and/or agent session).
    ///
    /// Format: `tsk_<32 hex>`. Constructed via [`TaskId::derive`].
    TaskId
);

define_id!(
    /// Identity of a single dated version of an artifact or task.
    ///
    /// Format: `ver_<32 hex>`. Constructed via `VersionId::for_artifact` / `for_task`.
    VersionId
);

define_id!(
    /// External session identifier supplied by the coding agent (opaque to brain0).
    SessionId
);

define_id!(
    /// Reference to a blob in the heavy payload store (prompt, transcript, diff, summary).
    ///
    /// The index only ever stores this reference; the content is hydrated on demand.
    PayloadRef
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_is_deterministic() {
        let a = hashed("dom", &[b"hello", b"world"]);
        let b = hashed("dom", &[b"hello", b"world"]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hashed_separator_prevents_ambiguity() {
        // Without the separator these would collide.
        assert_ne!(hashed("d", &[b"ab", b"c"]), hashed("d", &[b"a", b"bc"]));
    }

    #[test]
    fn hashed_domain_separation() {
        assert_ne!(hashed("d1", &[b"x"]), hashed("d2", &[b"x"]));
    }

    #[test]
    fn id_roundtrips_through_json() {
        let id = ArtifactId::new("art_deadbeef");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"art_deadbeef\"");
        let back: ArtifactId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
