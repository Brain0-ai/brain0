//! Attribution: who and what produced a change.
//!
//! Every version carries both an [`Author`] (the human) and an [`Agent`] (the tool/AI).
//! Attribution is a *tag* on versions, never a partition of the graph:
//! two authors touching the same symbol produce two versions on the *same* timeline.

use serde::{Deserialize, Serialize};

/// The human responsible for a change.
///
/// Derived from git when available; otherwise from the name declared at observer setup
///.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl Author {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: None,
        }
    }

    pub fn with_email(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: Some(email.into()),
        }
    }

    /// Stable key used in deterministic id derivation. Prefers email (more stable than a
    /// display name), falling back to the name.
    pub fn key(&self) -> &str {
        self.email.as_deref().unwrap_or(&self.name)
    }
}

/// The coding agent (or `"human"` for a hand-made change) that produced a change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Agent {
    /// e.g. `"claude-code"`, `"cursor"`, `"aider"`, or `"human"`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Agent {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: None,
        }
    }

    /// The conventional agent value for a change made directly by a human.
    pub fn human() -> Self {
        Self::new("human")
    }

    pub fn with_version(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: Some(version.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_key_prefers_email() {
        assert_eq!(Author::with_email("Ada", "ada@x.io").key(), "ada@x.io");
        assert_eq!(Author::new("Ada").key(), "Ada");
    }

    #[test]
    fn roundtrip() {
        let a = Agent::with_version("claude-code", "1.2.3");
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(a, serde_json::from_str(&json).unwrap());
    }
}
