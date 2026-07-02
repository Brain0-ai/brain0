//! Dated versions of nodes — the historical memory of the graph.
//!
//! The model is **append-only**: a change produces a *new version* of the existing node,
//! never a new node when the entity is the same. Aggregate-level artifact versions are
//! *derived* from their children and are not authored by hand.

use serde::{Deserialize, Serialize};

use crate::attribution::{Agent, Author};
use crate::declared::{DeclaredChange, Drift};
use crate::id::{hashed, ArtifactId, PayloadRef, TaskId, VersionId};
use crate::Timestamp;

/// Where the objective truth of a version came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeSource {
    /// git is present: identified by commit SHA.
    Git { commit_sha: String },
    /// No git: identified by a checkpoint produced by the observer's checkpoint engine.
    Checkpoint { checkpoint_id: String },
}

impl ChangeSource {
    /// A stable string identifying this source, used in version-id derivation.
    #[must_use]
    pub fn ref_str(&self) -> &str {
        match self {
            ChangeSource::Git { commit_sha } => commit_sha,
            ChangeSource::Checkpoint { checkpoint_id } => checkpoint_id,
        }
    }
}

/// The kind of change a version represents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Same identity, new name (path changed within the same container).
    Renamed {
        from: String,
    },
    /// Same identity, moved to a different container/file.
    Moved {
        from: String,
    },
}

/// A dated version of an artifact node. Carries enough to reconstruct attribution, the
/// structural fingerprint at that point in time (for identity/rename tracking), and a
/// reference to the full diff in the payload store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactVersion {
    pub id: VersionId,
    pub artifact_id: ArtifactId,
    pub timestamp: Timestamp,
    pub author: Author,
    pub agent: Agent,
    pub source: ChangeSource,
    /// Fully-qualified path *at this version* (may differ from earlier versions on rename).
    pub qualified_path: String,
    /// Structural AST fingerprint hash at this version (empty for aggregate levels).
    pub fingerprint: String,
    pub change_kind: ChangeKind,
    pub lines_added: u32,
    pub lines_removed: u32,
    /// Reference to the full diff in the payload store (hydrated on demand).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ref: Option<PayloadRef>,
}

/// A dated version of a task node. Accumulates the prompt, the decision summary, the
/// agent's declared changes, and any detected drift, as reported incrementally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskVersion {
    pub id: VersionId,
    pub task_id: TaskId,
    pub timestamp: Timestamp,
    /// Reference to the full prompt in the payload store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ref: Option<PayloadRef>,
    /// Reference to the decision summary in the payload store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_summary_ref: Option<PayloadRef>,
    /// What the agent declared it changed in this version.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub declared: Vec<DeclaredChange>,
    /// Drift between declared and done, if computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drift: Option<Drift>,
    /// Files the agent **read** during this turn (repo-relative when inside the project, absolute
    /// when outside — the audit-relevant case). Metadata only: paths, never content. Lets an
    /// auditor see which files were loaded into the (possibly remote) model's context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<String>,
    /// Reads whose CONTENT contained secrets (detected at ingest by the secret scanner). Records
    /// only the secret KINDS per path — never the value or the content — so an auditor sees that a
    /// secret reached the model. Subset of `reads`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_secrets: Vec<ReadSecret>,
}

/// A read whose content held one or more secrets (kinds only, never the value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadSecret {
    /// The read path (same normalization as `reads`).
    pub path: String,
    /// Detected secret kinds (e.g. `aws_access_key`, `private_key`).
    pub kinds: Vec<String>,
}

impl VersionId {
    /// Deterministic id for an artifact version. Distinct authors at the same instant
    /// produce distinct versions (author is part of the key); the same observed change
    /// reported twice converges on one version.
    #[must_use]
    pub fn for_artifact(
        artifact_id: &ArtifactId,
        timestamp: &Timestamp,
        author: &Author,
        source: &ChangeSource,
    ) -> Self {
        let raw = hashed(
            "brain0.version.artifact.v1",
            &[
                artifact_id.as_str().as_bytes(),
                timestamp.to_rfc3339().as_bytes(),
                author.key().as_bytes(),
                source.ref_str().as_bytes(),
            ],
        );
        Self::new(format!("ver_{raw}"))
    }

    /// Deterministic id for a task version.
    #[must_use]
    pub fn for_task(task_id: &TaskId, timestamp: &Timestamp, ordinal: u64) -> Self {
        let raw = hashed(
            "brain0.version.task.v1",
            &[
                task_id.as_str().as_bytes(),
                timestamp.to_rfc3339().as_bytes(),
                &ordinal.to_be_bytes(),
            ],
        );
        Self::new(format!("ver_{raw}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> Timestamp {
        chrono::Utc.with_ymd_and_hms(2026, 6, 6, 12, 0, 0).unwrap()
    }

    #[test]
    fn artifact_version_id_distinguishes_authors() {
        let aid = ArtifactId::new("art_x");
        let src = ChangeSource::Git {
            commit_sha: "abc".into(),
        };
        let v1 = VersionId::for_artifact(&aid, &ts(), &Author::new("Ada"), &src);
        let v2 = VersionId::for_artifact(&aid, &ts(), &Author::new("Linus"), &src);
        let v1b = VersionId::for_artifact(&aid, &ts(), &Author::new("Ada"), &src);
        assert_ne!(v1, v2);
        assert_eq!(v1, v1b); // same inputs converge
    }

    #[test]
    fn change_source_ref() {
        assert_eq!(
            ChangeSource::Checkpoint {
                checkpoint_id: "cp1".into()
            }
            .ref_str(),
            "cp1"
        );
    }

    #[test]
    fn artifact_version_roundtrips() {
        let v = ArtifactVersion {
            id: VersionId::new("ver_1"),
            artifact_id: ArtifactId::new("art_1"),
            timestamp: ts(),
            author: Author::new("Ada"),
            agent: Agent::new("claude-code"),
            source: ChangeSource::Git {
                commit_sha: "abc".into(),
            },
            qualified_path: "m::f".into(),
            fingerprint: "fp".into(),
            change_kind: ChangeKind::Modified,
            lines_added: 3,
            lines_removed: 1,
            diff_ref: Some(PayloadRef::new("blob://1")),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(v, serde_json::from_str(&json).unwrap());
    }
}
