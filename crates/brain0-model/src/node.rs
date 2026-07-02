//! The two node families of the bipartite graph: task nodes (intent) and artifact
//! nodes (code).

use serde::{Deserialize, Serialize};

use crate::attribution::{Agent, Author};
use crate::id::{hashed, ArtifactId, SessionId, TaskId, VersionId};
use crate::risk::RiskState;
use crate::Timestamp;

/// Hierarchical granularity of an artifact — the levels of the magnifying glass.
///
/// Aggregate levels (`Repo`, `Module`, `File`) are *derived* from their children;
/// `Symbol` is the leaf unit (with `File` acting as the leaf when no grammar is
/// available —).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Repo,
    Module,
    File,
    Symbol,
}

impl Level {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Repo => "repo",
            Level::Module => "module",
            Level::File => "file",
            Level::Symbol => "symbol",
        }
    }

    /// The level immediately above this one in the containment hierarchy, if any.
    #[must_use]
    pub fn parent(self) -> Option<Level> {
        match self {
            Level::Repo => None,
            Level::Module => Some(Level::Repo),
            Level::File => Some(Level::Module),
            Level::Symbol => Some(Level::File),
        }
    }

    /// Whether this level is *derived* from its children (aggregate) rather than observed
    /// directly.
    #[must_use]
    pub fn is_aggregate(self) -> bool {
        !matches!(self, Level::Symbol)
    }
}

impl std::str::FromStr for Level {
    type Err = crate::ModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "repo" => Ok(Level::Repo),
            "module" => Ok(Level::Module),
            "file" => Ok(Level::File),
            "symbol" => Ok(Level::Symbol),
            other => Err(crate::ModelError::UnknownLevel(other.to_owned())),
        }
    }
}

/// Source language of an artifact, as a lowercase identifier (`"python"`,
/// `"typescript"`, ...). A newtype rather than an enum so new Tree-sitter grammars can be
/// added without touching the data model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Lang(String);

impl Lang {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An artifact node: a persistent piece of code that evolves over time. Its `id` is the
/// deterministic symbol identity; it accumulates [`crate::ArtifactVersion`]s rather than
/// spawning new nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactNode {
    pub id: ArtifactId,
    pub level: Level,
    /// Canonical repository identifier (e.g. remote URL or org/name), used for
    /// cross-machine identity convergence.
    pub repo: String,
    /// Normalized, fully-qualified path of the artifact at its current coordinates.
    pub qualified_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Lang>,
    /// Containing artifact one level up (`None` for the repo root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ArtifactId>,
    /// The most recent version of this artifact.
    pub current_version: VersionId,
    pub risk: RiskState,
}

impl ArtifactId {
    /// Derive the deterministic, cross-machine artifact identity.
    ///
    /// For leaf symbols the `fingerprint` is the structural AST fingerprint; for aggregate
    /// levels (file/module/repo) it is the empty string, so an aggregate's identity is a
    /// pure function of its coordinates.
    #[must_use]
    pub fn derive(repo: &str, level: Level, qualified_path: &str, fingerprint: &str) -> Self {
        let raw = hashed(
            "brain0.artifact.v1",
            &[
                repo.as_bytes(),
                level.as_str().as_bytes(),
                qualified_path.as_bytes(),
                fingerprint.as_bytes(),
            ],
        );
        Self::new(format!("art_{raw}"))
    }
}

/// A task node: an intent — a user prompt and/or an agent work session, summarizing the
/// decisions taken. It persists and accumulates [`crate::TaskVersion`]s as the agent
/// reports incrementally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskId,
    pub session_id: SessionId,
    pub agent: Agent,
    pub author: Author,
    pub created_at: Timestamp,
    pub current_version: VersionId,
    /// Which agent-artifact adapter produced this intent (e.g. `"codex"`, `"claude-code"`),
    /// or `None` for observer-originated tasks. Additive field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_adapter: Option<String>,
    /// Working directory of the originating session, for project scoping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cwd: Option<String>,
    /// The model the agent used for this session (e.g. `claude-sonnet-4-6`), when the transcript
    /// records it. Drives "which model received the read" (DLP) + provenance attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Human reviewers of this change, parsed from commit-message trailers (`Reviewed-by:` /
    /// `Acked-by:`). Empty when unreviewed — the basis for "AI-assisted but unreviewed" (§2.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
}

impl TaskId {
    /// Derive a task id from its session and an ordinal within that session. Two events
    /// for the same logical intent (same session + ordinal) converge on one node.
    #[must_use]
    pub fn derive(session_id: &SessionId, ordinal: u64) -> Self {
        let raw = hashed(
            "brain0.task.v1",
            &[session_id.as_str().as_bytes(), &ordinal.to_be_bytes()],
        );
        Self::new(format!("tsk_{raw}"))
    }

    /// Derive a task id for an *observer-originated* intent: a commit or checkpoint treated
    /// as an intent when no agent reported one (so the graph is complete even for agents /// that declare nothing —). Deterministic across machines.
    #[must_use]
    pub fn for_source(repo: &str, source_ref: &str) -> Self {
        let raw = hashed(
            "brain0.task.source.v1",
            &[repo.as_bytes(), source_ref.as_bytes()],
        );
        Self::new(format!("tsk_{raw}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn level_hierarchy() {
        assert_eq!(Level::Symbol.parent(), Some(Level::File));
        assert_eq!(Level::Repo.parent(), None);
        assert!(Level::File.is_aggregate());
        assert!(!Level::Symbol.is_aggregate());
    }

    #[test]
    fn level_str_roundtrip() {
        for level in [Level::Repo, Level::Module, Level::File, Level::Symbol] {
            assert_eq!(Level::from_str(level.as_str()).unwrap(), level);
        }
        assert!(Level::from_str("nope").is_err());
    }

    #[test]
    fn artifact_id_is_deterministic_and_coordinate_sensitive() {
        let a = ArtifactId::derive("repo", Level::Symbol, "mod::f", "fp1");
        let b = ArtifactId::derive("repo", Level::Symbol, "mod::f", "fp1");
        assert_eq!(a, b);
        assert!(a.as_str().starts_with("art_"));
        // Different path or fingerprint → different id.
        assert_ne!(
            a,
            ArtifactId::derive("repo", Level::Symbol, "mod::g", "fp1")
        );
        assert_ne!(
            a,
            ArtifactId::derive("repo", Level::Symbol, "mod::f", "fp2")
        );
        assert_ne!(
            a,
            ArtifactId::derive("other", Level::Symbol, "mod::f", "fp1")
        );
    }

    #[test]
    fn task_id_is_deterministic() {
        let s = SessionId::new("sess-1");
        assert_eq!(TaskId::derive(&s, 0), TaskId::derive(&s, 0));
        assert_ne!(TaskId::derive(&s, 0), TaskId::derive(&s, 1));
    }
}
