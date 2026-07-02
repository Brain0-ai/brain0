//! Declared ↔ done reconciliation.
//!
//! A coding agent *declares* what it changed (via MCP). The observer records what
//! *actually* changed. This crate compares the two and performs both required functions:
//!
//! * **Gap-filling** — everything that actually changed is linked to the agent's task even
//!   if the agent never mentioned it, so the graph is complete.
//! * **Drift detection** — when declared and done diverge (e.g. "I only touched X" but 12
//!   files changed), the discrepancy is recorded as a first-class [`Drift`] signal on the
//!   task and feeds the a-priori risk.

use std::collections::BTreeSet;

use brain0_model::{ArtifactId, PayloadRef};
use brain0_model::{
    ChangeKind, DeclaredChange, Drift, Edge, ReadSecret, TaskId, TaskVersion, Timestamp, VersionId,
};
use brain0_storage::{Storage, StorageError};
use thiserror::Error;

/// Errors produced during reconciliation.
#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, ReconcileError>;

/// One actually-observed change, as recorded by the observer, to be reconciled against the
/// agent's declarations.
#[derive(Debug, Clone)]
pub struct ActualChange {
    pub artifact_id: ArtifactId,
    /// Repo-relative file path that changed.
    pub path: String,
    pub version: VersionId,
    pub change_kind: ChangeKind,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Compute the drift between what was declared and what actually changed.
///
/// Comparison is by file path. The score is the size of the symmetric difference over the
/// union of paths, in `0.0..=1.0` (0 = perfect match, 1 = fully disjoint).
#[must_use]
pub fn compute_drift(declared: &[DeclaredChange], actual_paths: &[String]) -> Drift {
    let declared_set: BTreeSet<&str> = declared.iter().map(|d| d.path.as_str()).collect();
    let actual_set: BTreeSet<&str> = actual_paths.iter().map(String::as_str).collect();

    let undeclared: Vec<String> = actual_set
        .difference(&declared_set)
        .map(|s| (*s).to_owned())
        .collect();
    let phantom: Vec<String> = declared_set
        .difference(&actual_set)
        .map(|s| (*s).to_owned())
        .collect();

    let union = declared_set.union(&actual_set).count();
    let score = if union == 0 {
        0.0
    } else {
        (undeclared.len() + phantom.len()) as f32 / union as f32
    };
    Drift::new(score, undeclared, phantom)
}

/// Inputs for persisting a reconciliation onto a task.
#[derive(Debug, Clone)]
pub struct ReconcileInput<'a> {
    pub task_id: &'a TaskId,
    pub timestamp: Timestamp,
    /// Ordinal of the task version being appended (incremental within the session).
    pub ordinal: u64,
    /// What THIS turn declared — persisted verbatim on the version (the per-turn record).
    pub declared: &'a [DeclaredChange],
    /// The whole session's declarations so far (this turn included) — the set drift is
    /// computed against. Agents declare incrementally while commits land in batches at the
    /// session's end; comparing a single turn against a commit-sized actual set produces
    /// false phantom (declared, committed later) and mass undeclared (sibling turns' files).
    /// Cumulative-vs-cumulative converges: the latest version's drift is the session verdict.
    pub cumulative_declared: &'a [DeclaredChange],
    pub actual: &'a [ActualChange],
    /// Optional payload refs for the prompt / decision summary at this point.
    pub prompt_ref: Option<PayloadRef>,
    pub decision_summary_ref: Option<PayloadRef>,
    /// Files the agent read this turn (audit trail of what reached the model).
    pub reads: &'a [String],
    /// Reads whose content held secrets (kinds only) — DLP signal.
    pub read_secrets: &'a [ReadSecret],
}

/// Reconcile an agent's declared changes against the observed actual changes:
///
/// 1. **Gap-fill**: link every actual change to the agent's task via
///    `TASK_MODIFIES_ARTIFACT` (even undeclared ones).
/// 2. **Drift**: compute [`Drift`] and append a [`TaskVersion`] carrying both the
///    declarations and the drift signal.
///
/// Returns the computed drift.
pub fn reconcile(storage: &dyn Storage, input: &ReconcileInput) -> Result<Drift> {
    // 1. Gap-filling: every observed change is attached to the task.
    for change in input.actual {
        storage.put_edge(&Edge::TaskModifiesArtifact {
            task: input.task_id.clone(),
            artifact: change.artifact_id.clone(),
            version: change.version.clone(),
            change_kind: change.change_kind.clone(),
            lines_added: change.lines_added,
            lines_removed: change.lines_removed,
        })?;
    }

    // 2. Drift detection — against the SESSION's cumulative declarations, not just this turn's.
    let actual_paths: Vec<String> = input.actual.iter().map(|c| c.path.clone()).collect();
    let drift = compute_drift(input.cumulative_declared, &actual_paths);

    storage.append_task_version(&TaskVersion {
        id: VersionId::for_task(input.task_id, &input.timestamp, input.ordinal),
        task_id: input.task_id.clone(),
        timestamp: input.timestamp,
        prompt_ref: input.prompt_ref.clone(),
        decision_summary_ref: input.decision_summary_ref.clone(),
        declared: input.declared.to_vec(),
        drift: Some(drift.clone()),
        reads: input.reads.to_vec(),
        read_secrets: input.read_secrets.to_vec(),
    })?;

    Ok(drift)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::chrono::{self, TimeZone};
    use brain0_storage::{SqliteStorage, Storage};

    fn declared(paths: &[&str]) -> Vec<DeclaredChange> {
        paths.iter().map(|p| DeclaredChange::new(*p)).collect()
    }

    #[test]
    fn perfect_match_has_no_drift() {
        let d = compute_drift(
            &declared(&["a.py", "b.py"]),
            &["a.py".into(), "b.py".into()],
        );
        assert_eq!(d.score, 0.0);
        assert!(!d.is_present());
    }

    #[test]
    fn undeclared_changes_are_detected() {
        // Agent said "only a.py" but b.py and c.py also changed.
        let d = compute_drift(
            &declared(&["a.py"]),
            &["a.py".into(), "b.py".into(), "c.py".into()],
        );
        assert!(d.is_present());
        assert_eq!(d.undeclared, vec!["b.py".to_owned(), "c.py".to_owned()]);
        assert!(d.phantom.is_empty());
        // 2 undeclared / 3 union.
        assert!((d.score - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn phantom_declarations_are_detected() {
        // Agent claimed b.py but it never actually changed.
        let d = compute_drift(&declared(&["a.py", "b.py"]), &["a.py".into()]);
        assert_eq!(d.phantom, vec!["b.py".to_owned()]);
        assert!(d.undeclared.is_empty());
    }

    #[test]
    fn empty_is_no_drift() {
        assert_eq!(compute_drift(&[], &[]).score, 0.0);
    }

    #[test]
    fn drift_uses_the_cumulative_declarations_not_the_single_turn() {
        // Turn 1 declared a.py, turn 2 declares b.py; the commit lands both. Against the
        // single turn (b.py only) a.py would read as undeclared — against the session's
        // cumulative set there is no drift at all.
        let store = SqliteStorage::open_in_memory().unwrap();
        let task = TaskId::new("tsk_cum");
        let actual = vec![
            ActualChange {
                artifact_id: ArtifactId::new("art_a"),
                path: "a.py".into(),
                version: VersionId::new("ver_a2"),
                change_kind: ChangeKind::Modified,
                lines_added: 1,
                lines_removed: 0,
            },
            ActualChange {
                artifact_id: ArtifactId::new("art_b"),
                path: "b.py".into(),
                version: VersionId::new("ver_b2"),
                change_kind: ChangeKind::Modified,
                lines_added: 1,
                lines_removed: 0,
            },
        ];
        let this_turn = declared(&["b.py"]);
        let cumulative = declared(&["a.py", "b.py"]);
        let drift = reconcile(
            &store,
            &ReconcileInput {
                task_id: &task,
                timestamp: chrono::Utc.timestamp_opt(200, 0).single().unwrap(),
                ordinal: 2,
                declared: &this_turn,
                cumulative_declared: &cumulative,
                actual: &actual,
                prompt_ref: None,
                decision_summary_ref: None,
                reads: &[],
                read_secrets: &[],
            },
        )
        .unwrap();
        assert!(!drift.is_present(), "cumulative match must yield no drift");
        // The per-turn record still carries only what THIS turn declared.
        let versions = store.task_versions(&task).unwrap();
        assert_eq!(versions[0].declared.len(), 1);
        assert_eq!(versions[0].declared[0].path, "b.py");
    }

    #[test]
    fn reconcile_gap_fills_and_persists_drift() {
        let store = SqliteStorage::open_in_memory().unwrap();
        let task = TaskId::new("tsk_agent");
        let actual = vec![
            ActualChange {
                artifact_id: ArtifactId::new("art_a"),
                path: "a.py".into(),
                version: VersionId::new("ver_a"),
                change_kind: ChangeKind::Modified,
                lines_added: 3,
                lines_removed: 1,
            },
            ActualChange {
                artifact_id: ArtifactId::new("art_b"),
                path: "b.py".into(),
                version: VersionId::new("ver_b"),
                change_kind: ChangeKind::Added,
                lines_added: 10,
                lines_removed: 0,
            },
        ];
        let decl = declared(&["a.py"]); // b.py undeclared
        let drift = reconcile(
            &store,
            &ReconcileInput {
                task_id: &task,
                timestamp: chrono::Utc.timestamp_opt(100, 0).single().unwrap(),
                ordinal: 1,
                declared: &decl,
                cumulative_declared: &decl,
                actual: &actual,
                prompt_ref: None,
                decision_summary_ref: None,
                reads: &[],
                read_secrets: &[],
            },
        )
        .unwrap();

        // Drift detected b.py as undeclared.
        assert_eq!(drift.undeclared, vec!["b.py".to_owned()]);

        // Gap-filling: both artifacts linked to the agent's task.
        use brain0_model::EdgeKind;
        let edges = store
            .out_edges(EdgeKind::TaskModifiesArtifact, "tsk_agent")
            .unwrap();
        assert_eq!(edges.len(), 2);

        // Drift persisted on the task version.
        let versions = store.task_versions(&task).unwrap();
        assert_eq!(versions.len(), 1);
        assert!(versions[0].drift.as_ref().unwrap().is_present());
    }
}
