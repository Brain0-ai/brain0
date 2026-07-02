//! Typed edges — what makes the graph navigable rather than decorative.
//!
//! Two navigation axes are mandatory:
//! * **by intent** — "what did this prompt do" via [`Edge::TaskModifiesArtifact`];
//! * **by place** — "who touched this symbol, in what temporal order" via
//!   [`Edge::ArtifactVersionSucceeds`] on the persistent artifact node (the primary debug
//!   path).

use serde::{Deserialize, Serialize};

use crate::id::{ArtifactId, TaskId, VersionId};
use crate::version::ChangeKind;

/// Discriminant for an edge, used for indexing/queries in storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    TaskModifiesArtifact,
    ArtifactContains,
    ArtifactDependsOn,
    ArtifactVersionSucceeds,
    TaskFollows,
}

impl EdgeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::TaskModifiesArtifact => "task_modifies_artifact",
            EdgeKind::ArtifactContains => "artifact_contains",
            EdgeKind::ArtifactDependsOn => "artifact_depends_on",
            EdgeKind::ArtifactVersionSucceeds => "artifact_version_succeeds",
            EdgeKind::TaskFollows => "task_follows",
        }
    }
}

/// A typed edge in the graph. Each variant carries strongly-typed endpoints plus any
/// edge-specific attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Edge {
    /// Links an intent to the code it changed. The "by intent" axis.
    TaskModifiesArtifact {
        task: TaskId,
        artifact: ArtifactId,
        /// The specific artifact version produced by this task.
        version: VersionId,
        change_kind: ChangeKind,
        lines_added: u32,
        lines_removed: u32,
    },
    /// Containment hierarchy of the magnifying glass (repo ⊃ module ⊃ file ⊃ symbol).
    ArtifactContains {
        parent: ArtifactId,
        child: ArtifactId,
    },
    /// Dependency graph between artifacts, used for blast-radius and risk.
    ArtifactDependsOn {
        dependent: ArtifactId,
        dependency: ArtifactId,
    },
    /// Temporal chain of an artifact's versions. The "by place / timeline" axis.
    ArtifactVersionSucceeds {
        predecessor: VersionId,
        successor: VersionId,
    },
    /// Sequence/derivation between tasks in the same session or work chain.
    TaskFollows {
        predecessor: TaskId,
        successor: TaskId,
    },
}

impl Edge {
    #[must_use]
    pub fn kind(&self) -> EdgeKind {
        match self {
            Edge::TaskModifiesArtifact { .. } => EdgeKind::TaskModifiesArtifact,
            Edge::ArtifactContains { .. } => EdgeKind::ArtifactContains,
            Edge::ArtifactDependsOn { .. } => EdgeKind::ArtifactDependsOn,
            Edge::ArtifactVersionSucceeds { .. } => EdgeKind::ArtifactVersionSucceeds,
            Edge::TaskFollows { .. } => EdgeKind::TaskFollows,
        }
    }

    /// The source and destination ids as plain strings, for generic storage. The pair is
    /// unique per edge kind and is what the storage layer keys on.
    #[must_use]
    pub fn endpoints(&self) -> (String, String) {
        match self {
            Edge::TaskModifiesArtifact { task, artifact, .. } => {
                (task.as_str().to_owned(), artifact.as_str().to_owned())
            }
            Edge::ArtifactContains { parent, child } => {
                (parent.as_str().to_owned(), child.as_str().to_owned())
            }
            Edge::ArtifactDependsOn {
                dependent,
                dependency,
            } => (
                dependent.as_str().to_owned(),
                dependency.as_str().to_owned(),
            ),
            Edge::ArtifactVersionSucceeds {
                predecessor,
                successor,
            } => (
                predecessor.as_str().to_owned(),
                successor.as_str().to_owned(),
            ),
            Edge::TaskFollows {
                predecessor,
                successor,
            } => (
                predecessor.as_str().to_owned(),
                successor.as_str().to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_endpoints() {
        let e = Edge::ArtifactContains {
            parent: ArtifactId::new("art_p"),
            child: ArtifactId::new("art_c"),
        };
        assert_eq!(e.kind(), EdgeKind::ArtifactContains);
        assert_eq!(e.endpoints(), ("art_p".to_owned(), "art_c".to_owned()));
    }

    #[test]
    fn task_modifies_roundtrips() {
        let e = Edge::TaskModifiesArtifact {
            task: TaskId::new("tsk_1"),
            artifact: ArtifactId::new("art_1"),
            version: VersionId::new("ver_1"),
            change_kind: ChangeKind::Added,
            lines_added: 10,
            lines_removed: 0,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(e, serde_json::from_str(&json).unwrap());
        assert_eq!(e.kind().as_str(), "task_modifies_artifact");
    }
}
