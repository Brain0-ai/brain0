//! Passive observer for brain0.
//!
//! The observer never writes to the user's repo. It turns a [`Snapshot`] — a dated set of
//! file states with attribution — into append-only graph updates, **identically** whether
//! the snapshot came from git (commits) or from the checkpoint engine (no-git fallback).
//!
//! [`ingest`] is the mode-agnostic core: it parses each changed file, resolves symbol and
//! file identity across snapshots (so renames/moves stay the same node), appends dated
//! versions, maintains the `repo → module → file → symbol` containment hierarchy, links
//! the version timeline, and attaches every change to a per-snapshot intent task (so the
//! graph is complete even for agents that report nothing). Aggregate nodes (repo, module)
//! are derived containers; their risk is computed by `brain0-risk` (Phase 7).

mod checkpoint;
mod git;

pub use checkpoint::{full_tree, snapshot_directory, CheckpointEngine};
pub use git::GitReader;

use std::collections::BTreeSet;

use brain0_identity::{resolve, ChangeStatus, IdentityConfig, KnownSymbol, ObservedSymbol};
use brain0_model::{
    Agent, ArtifactId, ArtifactNode, ArtifactVersion, Author, ChangeKind, ChangeSource, Edge, Lang,
    Level, SessionId, TaskId, TaskNode, TaskVersion, Timestamp, VersionId,
};
use brain0_parser::{parse_source, Fingerprint, ParserError};
use brain0_storage::{PayloadStore, Storage, StorageError};
use thiserror::Error;

/// Errors produced while observing.
#[derive(Debug, Error)]
pub enum ObserverError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Parser(#[from] ParserError),
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, ObserverError>;

/// The state of one file within a snapshot.
#[derive(Debug, Clone)]
pub struct FileState {
    /// Repo-relative, normalized path.
    pub rel_path: String,
    /// File content, or `None` if the file was deleted in this snapshot.
    pub content: Option<String>,
    /// Lines added at the file level (best-effort; 0 if unknown).
    pub lines_added: u32,
    /// Lines removed at the file level (best-effort; 0 if unknown).
    pub lines_removed: u32,
    /// Full unified diff/patch text for the file, stored in the payload store if present.
    pub diff: Option<String>,
}

impl FileState {
    /// A modified/added file with given content.
    pub fn modified(rel_path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            rel_path: rel_path.into(),
            content: Some(content.into()),
            lines_added: 0,
            lines_removed: 0,
            diff: None,
        }
    }

    /// A deleted file.
    pub fn deleted(rel_path: impl Into<String>) -> Self {
        Self {
            rel_path: rel_path.into(),
            content: None,
            lines_added: 0,
            lines_removed: 0,
            diff: None,
        }
    }
}

/// A dated, attributed set of file changes — the unit the observer ingests.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Canonical repository identifier.
    pub repo: String,
    pub timestamp: Timestamp,
    pub author: Author,
    pub agent: Agent,
    pub source: ChangeSource,
    /// Commit message / checkpoint note → used as the intent task's decision summary.
    pub message: Option<String>,
    /// The files that changed in this snapshot (present or deleted).
    pub files: Vec<FileState>,
}

/// Summary of what an [`ingest`] applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub task_id: Option<String>,
    pub changed_artifacts: usize,
    pub added: usize,
    pub modified: usize,
    pub renamed: usize,
    pub moved: usize,
    pub removed: usize,
}

/// Extract human reviewers from a commit message's git trailers (`Reviewed-by:` / `Acked-by:`),
/// the offline review-status convention (§2.3). Case-insensitive keys; values trimmed; a
/// `Co-authored-by:` is NOT a review. Returns the reviewer strings in order, deduped-by-presence
/// left to the caller.
#[must_use]
pub fn parse_review_trailers(message: &str) -> Vec<String> {
    let mut reviewers = Vec::new();
    for line in message.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if matches!(key.as_str(), "reviewed-by" | "acked-by") {
            let value = value.trim();
            if !value.is_empty() && !reviewers.iter().any(|r| r == value) {
                reviewers.push(value.to_owned());
            }
        }
    }
    reviewers
}

/// Module path (parent directory) of a repo-relative file path.
fn module_path(rel_path: &str) -> String {
    match rel_path.rfind('/') {
        Some(idx) => rel_path[..idx].to_owned(),
        None => ".".to_owned(),
    }
}

/// The container (file portion) of a symbol's qualified path.
fn container_of(qualified_path: &str) -> &str {
    qualified_path.split("::").next().unwrap_or(qualified_path)
}

/// Shared context threaded through the recording helpers.
struct Ctx<'a> {
    storage: &'a dyn Storage,
    payload: &'a dyn PayloadStore,
    repo: &'a str,
    timestamp: Timestamp,
    author: Author,
    agent: Agent,
    source: ChangeSource,
    task_id: TaskId,
}

impl Ctx<'_> {
    fn version_id(&self, artifact_id: &ArtifactId) -> VersionId {
        VersionId::for_artifact(artifact_id, &self.timestamp, &self.author, &self.source)
    }

    /// Ensure a derived aggregate node (repo/module) exists and is contained by `parent`.
    fn ensure_aggregate(
        &self,
        level: Level,
        qualified_path: &str,
        parent: Option<&ArtifactId>,
    ) -> Result<ArtifactId> {
        let id = ArtifactId::derive(self.repo, level, qualified_path, "");
        if self.storage.get_artifact_node(&id)?.is_none() {
            // Aggregates are derived: current_version is a stable label, not a real row.
            let label = VersionId::new(format!("ver_agg_{}", id.as_str()));
            self.storage.put_artifact_node(&ArtifactNode {
                id: id.clone(),
                level,
                repo: self.repo.to_owned(),
                qualified_path: qualified_path.to_owned(),
                lang: None,
                parent_id: parent.cloned(),
                current_version: label,
                risk: brain0_model::RiskState::default(),
            })?;
        }
        if let Some(parent) = parent {
            self.storage.put_edge(&Edge::ArtifactContains {
                parent: parent.clone(),
                child: id.clone(),
            })?;
        }
        Ok(id)
    }

    /// Append a new version of a leaf artifact (file or symbol) and update its node + edges.
    /// `Unchanged` only refreshes containment. Returns nothing; the id is the caller's.
    #[allow(clippy::too_many_arguments)]
    fn record_version(
        &self,
        artifact_id: &ArtifactId,
        level: Level,
        qualified_path: &str,
        lang: Option<&str>,
        parent: Option<&ArtifactId>,
        fingerprint: &Fingerprint,
        status: &ChangeStatus,
        lines_added: u32,
        lines_removed: u32,
        diff: Option<&str>,
        report: &mut IngestReport,
    ) -> Result<()> {
        let change_kind = match status {
            ChangeStatus::Unchanged => {
                if let Some(parent) = parent {
                    self.storage.put_edge(&Edge::ArtifactContains {
                        parent: parent.clone(),
                        child: artifact_id.clone(),
                    })?;
                }
                return Ok(());
            }
            ChangeStatus::Added => {
                report.added += 1;
                ChangeKind::Added
            }
            ChangeStatus::Modified => {
                report.modified += 1;
                ChangeKind::Modified
            }
            ChangeStatus::Renamed { from } => {
                report.renamed += 1;
                ChangeKind::Renamed { from: from.clone() }
            }
            ChangeStatus::Moved { from } => {
                report.moved += 1;
                ChangeKind::Moved { from: from.clone() }
            }
        };
        report.changed_artifacts += 1;

        let existing = self.storage.get_artifact_node(artifact_id)?;
        let prev_version = existing.as_ref().map(|node| node.current_version.clone());
        let risk = existing.map(|node| node.risk).unwrap_or_default();

        let version_id = self.version_id(artifact_id);
        let diff_ref = match diff {
            Some(text) => Some(self.payload.put_str(text)?),
            None => None,
        };

        self.storage.append_artifact_version(&ArtifactVersion {
            id: version_id.clone(),
            artifact_id: artifact_id.clone(),
            timestamp: self.timestamp,
            author: self.author.clone(),
            agent: self.agent.clone(),
            source: self.source.clone(),
            qualified_path: qualified_path.to_owned(),
            fingerprint: fingerprint.hash.clone(),
            change_kind: change_kind.clone(),
            lines_added,
            lines_removed,
            diff_ref,
        })?;

        self.storage.put_artifact_node(&ArtifactNode {
            id: artifact_id.clone(),
            level,
            repo: self.repo.to_owned(),
            qualified_path: qualified_path.to_owned(),
            lang: lang.map(Lang::new),
            parent_id: parent.cloned(),
            current_version: version_id.clone(),
            risk,
        })?;
        self.storage.put_artifact_fingerprint(
            artifact_id,
            &fingerprint.hash,
            &fingerprint.shingles,
        )?;

        if let Some(prev) = prev_version {
            if prev != version_id {
                self.storage.put_edge(&Edge::ArtifactVersionSucceeds {
                    predecessor: prev,
                    successor: version_id.clone(),
                })?;
            }
        }
        if let Some(parent) = parent {
            self.storage.put_edge(&Edge::ArtifactContains {
                parent: parent.clone(),
                child: artifact_id.clone(),
            })?;
        }
        self.storage.put_edge(&Edge::TaskModifiesArtifact {
            task: self.task_id.clone(),
            artifact: artifact_id.clone(),
            version: version_id,
            change_kind,
            lines_added,
            lines_removed,
        })?;
        Ok(())
    }

    /// Append a `Deleted` version for an artifact that disappeared.
    fn record_deletion(&self, artifact_id: &ArtifactId, report: &mut IngestReport) -> Result<()> {
        let Some(mut node) = self.storage.get_artifact_node(artifact_id)? else {
            return Ok(());
        };
        report.removed += 1;
        report.changed_artifacts += 1;

        let prev_version = node.current_version.clone();
        let fingerprint_hash = self
            .storage
            .get_artifact_fingerprint(artifact_id)?
            .map(|(hash, _)| hash)
            .unwrap_or_default();
        let version_id = self.version_id(artifact_id);

        self.storage.append_artifact_version(&ArtifactVersion {
            id: version_id.clone(),
            artifact_id: artifact_id.clone(),
            timestamp: self.timestamp,
            author: self.author.clone(),
            agent: self.agent.clone(),
            source: self.source.clone(),
            qualified_path: node.qualified_path.clone(),
            fingerprint: fingerprint_hash,
            change_kind: ChangeKind::Deleted,
            lines_added: 0,
            lines_removed: 0,
            diff_ref: None,
        })?;

        node.current_version = version_id.clone();
        self.storage.put_artifact_node(&node)?;

        if prev_version != version_id {
            self.storage.put_edge(&Edge::ArtifactVersionSucceeds {
                predecessor: prev_version,
                successor: version_id.clone(),
            })?;
        }
        self.storage.put_edge(&Edge::TaskModifiesArtifact {
            task: self.task_id.clone(),
            artifact: artifact_id.clone(),
            version: version_id,
            change_kind: ChangeKind::Deleted,
            lines_added: 0,
            lines_removed: 0,
        })?;
        Ok(())
    }
}

fn build_known(
    storage: &dyn Storage,
    repo: &str,
    level: Level,
    in_scope: impl Fn(&str) -> bool,
) -> Result<Vec<KnownSymbol>> {
    let mut out = Vec::new();
    for node in storage.list_artifacts(repo, level)? {
        if !in_scope(&node.qualified_path) {
            continue;
        }
        let fingerprint = storage
            .get_artifact_fingerprint(&node.id)?
            .map(|(hash, shingles)| Fingerprint { hash, shingles })
            .unwrap_or(Fingerprint {
                hash: String::new(),
                shingles: Vec::new(),
            });
        out.push(KnownSymbol {
            id: node.id,
            qualified_path: node.qualified_path,
            fingerprint,
        });
    }
    Ok(out)
}

/// Ingest a snapshot into the graph. Idempotent: re-ingesting the same snapshot makes no
/// further changes (deterministic ids + append-only inserts).
pub fn ingest(
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    snapshot: &Snapshot,
    config: &IdentityConfig,
) -> Result<IngestReport> {
    let mut report = IngestReport::default();

    // --- intent task for this snapshot (so the graph is complete sans MCP) ---
    let source_ref = snapshot.source.ref_str().to_owned();
    let task_id = TaskId::for_source(&snapshot.repo, &source_ref);
    let task_version_id = VersionId::for_task(&task_id, &snapshot.timestamp, 0);
    let decision_summary_ref = match &snapshot.message {
        Some(message) => Some(payload.put_str(message)?),
        None => None,
    };
    let reviewers = snapshot
        .message
        .as_deref()
        .map(parse_review_trailers)
        .unwrap_or_default();
    storage.put_task_node(&TaskNode {
        id: task_id.clone(),
        session_id: SessionId::new(source_ref),
        agent: snapshot.agent.clone(),
        author: snapshot.author.clone(),
        created_at: snapshot.timestamp,
        current_version: task_version_id.clone(),
        // Observer-originated tasks (the fact side); the declared/intent side comes from
        // the passive agent-artifact ingest.
        source_adapter: None,
        session_cwd: None,
        model: None, // a git commit has no agent model
        reviewers,   // human review status from commit trailers (§2.3)
    })?;
    storage.append_task_version(&TaskVersion {
        id: task_version_id,
        task_id: task_id.clone(),
        timestamp: snapshot.timestamp,
        prompt_ref: None,
        decision_summary_ref,
        declared: Vec::new(),
        drift: None,
        reads: Vec::new(), // a git commit is the FACT side — reads come from the agent transcript
        read_secrets: Vec::new(),
    })?;
    report.task_id = Some(task_id.as_str().to_owned());

    let ctx = Ctx {
        storage,
        payload,
        repo: &snapshot.repo,
        timestamp: snapshot.timestamp,
        author: snapshot.author.clone(),
        agent: snapshot.agent.clone(),
        source: snapshot.source.clone(),
        task_id,
    };

    let repo_id = ctx.ensure_aggregate(Level::Repo, &snapshot.repo, None)?;

    let changed_paths: BTreeSet<&str> =
        snapshot.files.iter().map(|f| f.rel_path.as_str()).collect();

    // --- parse present files ---
    struct Parsed {
        rel_path: String,
        lang: Option<String>,
        file_fingerprint: Fingerprint,
        lines_added: u32,
        lines_removed: u32,
        diff: Option<String>,
        symbols: Vec<brain0_parser::ExtractedSymbol>,
    }
    let mut parsed_files: Vec<Parsed> = Vec::new();
    for file in &snapshot.files {
        if let Some(content) = &file.content {
            let parsed = parse_source(&file.rel_path, content)?;
            parsed_files.push(Parsed {
                rel_path: file.rel_path.clone(),
                lang: parsed.lang.as_ref().map(|l| l.as_str().to_owned()),
                file_fingerprint: parsed.file_fingerprint,
                lines_added: file.lines_added,
                lines_removed: file.lines_removed,
                diff: file.diff.clone(),
                symbols: parsed.symbols,
            });
        }
    }

    // --- FILE level: resolve identity, record versions, build rel_path -> file id map ---
    let known_files = build_known(storage, &snapshot.repo, Level::File, |path| {
        changed_paths.contains(path)
    })?;
    let observed_files: Vec<ObservedSymbol> = parsed_files
        .iter()
        .map(|p| ObservedSymbol {
            qualified_path: p.rel_path.clone(),
            fingerprint: p.file_fingerprint.clone(),
        })
        .collect();
    let file_outcome = resolve(
        &snapshot.repo,
        Level::File,
        &known_files,
        &observed_files,
        config,
    );

    let mut file_id_by_path: std::collections::HashMap<String, ArtifactId> =
        std::collections::HashMap::new();
    for (idx, parsed) in parsed_files.iter().enumerate() {
        let resolved = &file_outcome.resolved[idx];
        let module_id = ctx.ensure_aggregate(
            Level::Module,
            &module_path(&parsed.rel_path),
            Some(&repo_id),
        )?;
        ctx.record_version(
            &resolved.id,
            Level::File,
            &parsed.rel_path,
            parsed.lang.as_deref(),
            Some(&module_id),
            &parsed.file_fingerprint,
            &resolved.status,
            parsed.lines_added,
            parsed.lines_removed,
            parsed.diff.as_deref(),
            &mut report,
        )?;
        file_id_by_path.insert(parsed.rel_path.clone(), resolved.id.clone());
    }
    for removed_id in &file_outcome.removed {
        ctx.record_deletion(removed_id, &mut report)?;
    }

    // --- SYMBOL level: resolve across all changed containers ---
    struct SymMeta {
        container: String,
        lang: Option<String>,
        span_lines: u32,
    }
    let mut observed_symbols: Vec<ObservedSymbol> = Vec::new();
    let mut symbol_meta: Vec<SymMeta> = Vec::new();
    for parsed in &parsed_files {
        for symbol in &parsed.symbols {
            observed_symbols.push(ObservedSymbol {
                qualified_path: symbol.qualified_path.clone(),
                fingerprint: symbol.fingerprint.clone(),
            });
            symbol_meta.push(SymMeta {
                container: parsed.rel_path.clone(),
                lang: parsed.lang.clone(),
                span_lines: (symbol.end_line.saturating_sub(symbol.start_line) + 1) as u32,
            });
        }
    }
    let known_symbols = build_known(storage, &snapshot.repo, Level::Symbol, |path| {
        changed_paths.contains(container_of(path))
    })?;
    let symbol_outcome = resolve(
        &snapshot.repo,
        Level::Symbol,
        &known_symbols,
        &observed_symbols,
        config,
    );
    for (idx, observed) in observed_symbols.iter().enumerate() {
        let resolved = &symbol_outcome.resolved[idx];
        let meta = &symbol_meta[idx];
        let parent = file_id_by_path.get(&meta.container);
        let lines_added = if matches!(resolved.status, ChangeStatus::Added) {
            meta.span_lines
        } else {
            0
        };
        ctx.record_version(
            &resolved.id,
            Level::Symbol,
            &observed.qualified_path,
            meta.lang.as_deref(),
            parent,
            &observed.fingerprint,
            &resolved.status,
            lines_added,
            0,
            None,
            &mut report,
        )?;
    }
    for removed_id in &symbol_outcome.removed {
        ctx.record_deletion(removed_id, &mut report)?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::EdgeKind;
    use brain0_storage::{InMemoryPayloadStore, SqliteStorage, Storage};
    use chrono::TimeZone;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn ts(secs: i64) -> Timestamp {
        chrono::Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    #[test]
    fn review_trailers_parsed_case_insensitively() {
        let msg = "Fix parser\n\nReviewed-by: Grace <grace@x.io>\nacked-by: Bob\nCo-authored-by: AI <ai@x.io>\nReviewed-by: Grace <grace@x.io>";
        // Both review trailers, deduped; Co-authored-by is not a review.
        assert_eq!(
            parse_review_trailers(msg),
            vec!["Grace <grace@x.io>".to_owned(), "Bob".to_owned()]
        );
        assert!(parse_review_trailers("no trailers here").is_empty());
    }

    fn snapshot(repo: &str, sha: &str, secs: i64, files: Vec<FileState>) -> Snapshot {
        Snapshot {
            repo: repo.to_owned(),
            timestamp: ts(secs),
            author: Author::with_email("Ada", "ada@x.io"),
            agent: Agent::new("claude-code"),
            source: ChangeSource::Git {
                commit_sha: sha.to_owned(),
            },
            message: Some(format!("commit {sha}")),
            files,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("brain0-obs-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ingest_core_produces_hierarchy_versions_and_edges() {
        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let cfg = IdentityConfig::default();

        let s1 = snapshot(
            "repo",
            "c1",
            100,
            vec![FileState::modified("pkg/m.py", "def f(x):\n    return x\n")],
        );
        let r1 = ingest(&store, &payload, &s1, &cfg).unwrap();
        assert_eq!(r1.added, 2); // file + symbol

        // The full hierarchy exists.
        let sym = store
            .find_artifact_by_path("repo", Level::Symbol, "pkg/m.py::f")
            .unwrap()
            .expect("symbol node");
        let file = store
            .find_artifact_by_path("repo", Level::File, "pkg/m.py")
            .unwrap()
            .expect("file node");
        assert!(store
            .find_artifact_by_path("repo", Level::Module, "pkg")
            .unwrap()
            .is_some());
        assert!(store
            .find_artifact_by_path("repo", Level::Repo, "repo")
            .unwrap()
            .is_some());
        assert_eq!(sym.parent_id.as_ref(), Some(&file.id));

        // Containment edge file -> symbol.
        let contains = store
            .out_edges(EdgeKind::ArtifactContains, file.id.as_str())
            .unwrap();
        assert!(contains
            .iter()
            .any(|e| matches!(e, Edge::ArtifactContains { child, .. } if *child == sym.id)));

        // Task created and links the symbol.
        let task_modifies = store
            .in_edges(EdgeKind::TaskModifiesArtifact, sym.id.as_str())
            .unwrap();
        assert_eq!(task_modifies.len(), 1);

        // Second snapshot: structural change → new version on the same node, chain linked.
        let s2 = snapshot(
            "repo",
            "c2",
            200,
            vec![FileState::modified(
                "pkg/m.py",
                "def f(x):\n    if x:\n        return x\n    return 0\n",
            )],
        );
        let r2 = ingest(&store, &payload, &s2, &cfg).unwrap();
        assert_eq!(r2.modified, 2); // file + symbol modified

        let versions = store.artifact_versions(&sym.id).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].author.name, "Ada"); // attribution preserved
        let succ = store
            .out_edges(EdgeKind::ArtifactVersionSucceeds, versions[0].id.as_str())
            .unwrap();
        assert_eq!(succ.len(), 1);
    }

    #[test]
    fn rename_keeps_same_artifact_node() {
        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let cfg = IdentityConfig::default();

        ingest(
            &store,
            &payload,
            &snapshot(
                "repo",
                "c1",
                100,
                vec![FileState::modified(
                    "m.py",
                    "def old_name(x):\n    return x + 1\n",
                )],
            ),
            &cfg,
        )
        .unwrap();
        let before = store
            .find_artifact_by_path("repo", Level::Symbol, "m.py::old_name")
            .unwrap()
            .unwrap();

        ingest(
            &store,
            &payload,
            &snapshot(
                "repo",
                "c2",
                200,
                vec![FileState::modified(
                    "m.py",
                    "def new_name(x):\n    return x + 1\n",
                )],
            ),
            &cfg,
        )
        .unwrap();
        let after = store
            .find_artifact_by_path("repo", Level::Symbol, "m.py::new_name")
            .unwrap()
            .expect("renamed symbol found at new path");

        assert_eq!(before.id, after.id, "rename must preserve identity");
        let versions = store.artifact_versions(&after.id).unwrap();
        assert!(matches!(
            versions.last().unwrap().change_kind,
            ChangeKind::Renamed { .. }
        ));
    }

    #[test]
    fn checkpoint_mode_produces_checkpoint_sourced_versions() {
        let dir = tmpdir("cp");
        std::fs::write(dir.join("app.py"), "def g():\n    return 1\n").unwrap();

        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let cfg = IdentityConfig::default();
        let mut engine =
            CheckpointEngine::new(&dir, "repo", Author::new("Local Dev"), Agent::human());

        let cp1 = engine
            .checkpoint(ts(100))
            .unwrap()
            .expect("first checkpoint");
        assert!(matches!(cp1.source, ChangeSource::Checkpoint { .. }));
        ingest(&store, &payload, &cp1, &cfg).unwrap();

        // No change → no checkpoint.
        assert!(engine.checkpoint(ts(150)).unwrap().is_none());

        std::fs::write(
            dir.join("app.py"),
            "def g():\n    if True:\n        return 1\n    return 0\n",
        )
        .unwrap();
        let cp2 = engine
            .checkpoint(ts(200))
            .unwrap()
            .expect("second checkpoint");
        ingest(&store, &payload, &cp2, &cfg).unwrap();

        let sym = store
            .find_artifact_by_path("repo", Level::Symbol, "app.py::g")
            .unwrap()
            .unwrap();
        let versions = store.artifact_versions(&sym.id).unwrap();
        assert_eq!(versions.len(), 2);
        assert!(matches!(
            versions[0].source,
            ChangeSource::Checkpoint { .. }
        ));
        assert_eq!(versions[0].author.name, "Local Dev"); // fallback attribution

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_mode_reconstructs_versions_with_attribution() {
        let dir = tmpdir("git");
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Grace Hopper"]);
        run(&["config", "user.email", "grace@navy.mil"]);

        std::fs::write(dir.join("lib.py"), "def add(a, b):\n    return a + b\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "add"]);
        std::fs::write(
            dir.join("lib.py"),
            "def add(a, b):\n    if a:\n        return a + b\n    return b\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "guard"]);

        let reader = GitReader::open(&dir, "test-repo").unwrap();
        let snapshots = reader.snapshots_since(None).unwrap();
        assert_eq!(snapshots.len(), 2);

        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let cfg = IdentityConfig::default();
        for snap in &snapshots {
            ingest(&store, &payload, snap, &cfg).unwrap();
        }

        let sym = store
            .find_artifact_by_path("test-repo", Level::Symbol, "lib.py::add")
            .unwrap()
            .expect("symbol from git");
        let versions = store.artifact_versions(&sym.id).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].author.name, "Grace Hopper"); // attribution from git
        assert!(matches!(versions[0].source, ChangeSource::Git { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
