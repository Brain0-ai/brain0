//! The ingest driver: discovered + scoped sources → turns → graph.
//!
//! For each scoped session it reads only new turns (cursor), summarizes each turn once
//! (cached), writes append-only task nodes/versions carrying the agent provenance, embeds
//! the session locally, and rewires reconciliation — declared changes now come from the
//! transcript's tool calls, compared against the observed git/checkpoint versions.

use brain0_model::{
    Agent, Author, DeclaredChange, Level, PayloadRef, ReadSecret, SessionId, TaskId, TaskNode,
    TaskVersion, Timestamp, VersionId,
};
use brain0_models::EmbeddingProvider;
use brain0_reconcile::{reconcile, ActualChange, ReconcileInput};
use brain0_storage::{PayloadStore, Storage};

use crate::event::Turn;
use crate::redact::Redactor;
use crate::scope::ProjectScope;
use crate::source::SourceRegistry;
use crate::summarize::{session_summary, summarize_cached, PersistentSummaryCache, TurnSummarizer};
use crate::Result;

/// Correlation window between a declared turn and observed objective changes.
const RECONCILE_WINDOW_SECS: i64 = 30 * 60;

/// What an ingest run produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestStats {
    pub sessions: usize,
    pub turns: usize,
    pub tasks: usize,
    pub embeddings: usize,
}

/// Progress callbacks for a long ingest pass (`observe`/`ingest`). Every method has a default
/// no-op, so callers that don't care are unaffected. The CLI implements this to render a
/// percentage + ETA while per-turn summarization (the slow, model-bound step) runs.
pub trait IngestProgress {
    /// Called once after planning, before any turn is processed: how many sessions and new turns
    /// will be ingested this pass (turns already past the cursor are not counted).
    fn on_plan(&self, sessions: usize, turns: usize) {
        let _ = (sessions, turns);
    }
    /// Called immediately **before** a turn is processed (`starting` is 1-based of `total`). This
    /// is the heartbeat before the slow, model-bound summarization, so the CLI is never silent
    /// while a single turn is being summarized.
    fn on_turn_start(&self, starting: usize, total: usize) {
        let _ = (starting, total);
    }
    /// Called after each turn is ingested (`done` of `total`).
    fn on_turn(&self, done: usize, total: usize) {
        let _ = (done, total);
    }
    /// Called once when the whole pass has finished.
    fn on_done(&self) {}
}

/// A progress sink that does nothing (the default used by [`run_ingest`]).
#[derive(Debug, Clone, Copy)]
pub struct NoProgress;
impl IngestProgress for NoProgress {}

/// Observed file versions inside `[from - W, to + W]` — the session horizon. Drift compares
/// the session's cumulative declarations against everything observed over the same span, so a
/// commit landing at the session's end still matches declarations made turns earlier.
fn actual_changes(
    storage: &dyn Storage,
    repo: &str,
    from: &Timestamp,
    to: &Timestamp,
) -> Result<Vec<ActualChange>> {
    let lo = *from - chrono::Duration::seconds(RECONCILE_WINDOW_SECS);
    let hi = *to + chrono::Duration::seconds(RECONCILE_WINDOW_SECS);
    let mut out = Vec::new();
    for file in storage.list_artifacts(repo, Level::File)? {
        for version in storage.artifact_versions(&file.id)? {
            if version.timestamp >= lo && version.timestamp <= hi {
                out.push(ActualChange {
                    artifact_id: file.id.clone(),
                    path: file.qualified_path.clone(),
                    version: version.id,
                    change_kind: version.change_kind,
                    lines_added: version.lines_added,
                    lines_removed: version.lines_removed,
                });
            }
        }
    }
    Ok(out)
}

/// Ingest one turn → an append-only task version (declared changes + drift). Returns the
/// turn's decision summary, for the session-level aggregate embedding.
#[allow(clippy::too_many_arguments)]
fn ingest_turn(
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    summarizer: &dyn TurnSummarizer,
    cache: &PersistentSummaryCache,
    adapter: &str,
    repo: Option<&str>,
    turn: &Turn,
    redactor: &Redactor,
) -> Result<String> {
    let task_id = TaskId::for_source(adapter, &turn.session_id);
    let prior_versions = storage.task_versions(&task_id)?;
    let ordinal = prior_versions.len() as u64;
    let version_id = VersionId::for_task(&task_id, &turn.timestamp, ordinal);
    // Session start (for the reconcile horizon): the first recorded turn, else this one.
    let session_start = prior_versions
        .first()
        .map_or(turn.timestamp, |v| v.timestamp);

    let existing = storage.get_task_node(&task_id)?;
    let created_at = existing.as_ref().map_or(turn.timestamp, |t| t.created_at);
    // Keep the model recorded for the session: this turn's, else whatever an earlier turn set.
    let model = turn
        .model
        .clone()
        .or_else(|| existing.as_ref().and_then(|t| t.model.clone()));
    storage.put_task_node(&TaskNode {
        id: task_id.clone(),
        session_id: SessionId::new(turn.session_id.clone()),
        agent: Agent::new(adapter),
        author: Author::new("agent"),
        created_at,
        current_version: version_id.clone(),
        source_adapter: Some(adapter.to_owned()),
        session_cwd: Some(turn.cwd.to_string_lossy().into_owned()),
        model,
        reviewers: Vec::new(), // review status is a commit-trailer concept, not a session one
    })?;

    // The full prompt is intentionally NOT persisted: only the
    // model-generated summary is kept. The summary is still computed from the in-memory turn
    // below (on already-redacted text), so dropping the prompt payload loses no downstream signal
    // while shrinking the at-rest footprint and the amount of raw user text on disk.
    let prompt_ref: Option<PayloadRef> = None;
    let summary = summarize_cached(summarizer, turn, cache);
    let summary_ref = Some(payload.put_str(&summary)?);
    let declared: Vec<DeclaredChange> = turn
        .declared_paths()
        .into_iter()
        .map(DeclaredChange::new)
        .collect();
    // Audit trail: files the agent read this turn (metadata only — paths, never content), so an
    // auditor can see what reached the (possibly remote) model. Recorded regardless of exclude.
    let reads = turn.read_paths();
    // DLP: secret-scan the CONTENT of the read results (what the model saw); record only the
    // detected secret KINDS per path — never the value or the content.
    let read_secrets: Vec<ReadSecret> = turn
        .reads_with_content()
        .into_iter()
        .filter_map(|(path, content)| {
            let kinds = redactor.scan_kinds(content);
            (!kinds.is_empty()).then_some(ReadSecret { path, kinds })
        })
        .collect();

    // The session's cumulative declarations (every prior turn + this one), deduped by path —
    // the set drift is measured against (see ReconcileInput::cumulative_declared). Absolute
    // paths are declarations OUTSIDE the repo (agent memory files, scratch dirs): the observer
    // can never see them change, so including them would manufacture permanent phantom drift.
    // They stay on the per-turn `declared` record (audit), just not in the drift comparison.
    let mut seen = std::collections::BTreeSet::new();
    let mut cumulative_declared: Vec<DeclaredChange> = Vec::new();
    for d in prior_versions
        .iter()
        .flat_map(|v| v.declared.iter())
        .chain(declared.iter())
    {
        if !d.path.starts_with('/') && seen.insert(d.path.clone()) {
            cumulative_declared.push(d.clone());
        }
    }

    let actual = match repo {
        Some(repo) => actual_changes(storage, repo, &session_start, &turn.timestamp)?,
        None => Vec::new(),
    };

    if actual.is_empty() {
        // No objective changes to reconcile against → record declarations without drift.
        storage.append_task_version(&TaskVersion {
            id: version_id,
            task_id,
            timestamp: turn.timestamp,
            prompt_ref,
            decision_summary_ref: summary_ref,
            declared,
            drift: None,
            reads,
            read_secrets,
        })?;
    } else {
        reconcile(
            storage,
            &ReconcileInput {
                task_id: &task_id,
                timestamp: turn.timestamp,
                ordinal,
                declared: &declared,
                cumulative_declared: &cumulative_declared,
                actual: &actual,
                prompt_ref,
                decision_summary_ref: summary_ref,
                reads: &reads,
                read_secrets: &read_secrets,
            },
        )?;
    }
    Ok(summary)
}

/// Run a full passive ingest pass over all registered sources, scoped to `scope`. If `repo`
/// (the observed repository id) is provided, declared changes are reconciled against the
/// objective git/checkpoint versions for drift.
///
/// Convenience wrapper over [`run_ingest_reporting`] with no progress reporting.
#[allow(clippy::too_many_arguments)]
pub fn run_ingest(
    registry: &SourceRegistry,
    scope: &ProjectScope,
    repo: Option<&str>,
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    summarizer: &dyn TurnSummarizer,
    embedder: &dyn EmbeddingProvider,
    redactor: &Redactor,
) -> Result<IngestStats> {
    run_ingest_reporting(
        registry,
        scope,
        repo,
        storage,
        payload,
        summarizer,
        embedder,
        redactor,
        &NoProgress,
    )
}

/// Like [`run_ingest`] but reports progress through `progress` (`on_plan`, then `on_turn` per
/// ingested turn, then `on_done`). Runs in two phases: it first **plans** — reading each scoped
/// session's new turns past its cursor — so the total turn count (and therefore a percentage and
/// ETA) is known before the slow, model-bound per-turn summarization begins.
#[allow(clippy::too_many_arguments)]
pub fn run_ingest_reporting(
    registry: &SourceRegistry,
    scope: &ProjectScope,
    repo: Option<&str>,
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    summarizer: &dyn TurnSummarizer,
    embedder: &dyn EmbeddingProvider,
    redactor: &Redactor,
    progress: &dyn IngestProgress,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    let cache = PersistentSummaryCache::from_env();
    // Fix the store's embedding model + dimension on first use.
    if storage.get_embedding_meta()?.is_none() {
        storage.set_embedding_meta(embedder.model_id(), embedder.dim())?;
    }

    // Phase 1 — plan: read each scoped, non-excluded session's new turns so we know the total
    // amount of work up front (for percent + ETA). Parsing is cheap; the model calls are not.
    let mut planned: Vec<(
        &str,
        crate::event::SessionFile,
        crate::event::IncrementalRead,
    )> = Vec::new();
    for source in registry.sources() {
        let adapter = source.name();
        for session in source.sessions(scope)? {
            let file_key = session.path.to_string_lossy().into_owned();
            // Privacy: skip excluded sessions before reading any content (§9).
            if redactor.is_excluded(&session.cwd.to_string_lossy())
                || redactor.is_excluded(&file_key)
            {
                continue;
            }
            let cursor = storage.get_cursor(adapter, &file_key)?.unwrap_or(0);
            let read = source.read_incremental(&session, cursor)?;
            planned.push((adapter, session, read));
        }
    }
    let total_turns: usize = planned.iter().map(|(_, _, read)| read.turns.len()).sum();
    progress.on_plan(planned.len(), total_turns);

    // Phase 2 — process each planned session.
    let mut done_turns = 0usize;
    for (adapter, session, read) in &planned {
        let adapter = *adapter;
        let file_key = session.path.to_string_lossy().into_owned();
        stats.sessions += 1;

        let mut summaries = Vec::new();
        let mut redactions = 0usize;
        for turn in &read.turns {
            progress.on_turn_start(done_turns + 1, total_turns);
            let (redacted, events) = redactor.redact_turn_audited(turn);
            redactions += events.len();
            summaries.push(ingest_turn(
                storage, payload, summarizer, &cache, adapter, repo, &redacted, redactor,
            )?);
            stats.turns += 1;
            done_turns += 1;
            progress.on_turn(done_turns, total_turns);
        }
        storage.set_cursor(adapter, &file_key, read.new_offset)?;
        if redactions > 0 {
            // Audit the fact that secrets were redacted — kinds/counts only, never values.
            storage.append_audit(
                "redaction",
                &format!(
                    "adapter={adapter} session={} redactions={redactions}",
                    session.session_id
                ),
                chrono::Utc::now(),
            )?;
        }

        if !read.turns.is_empty() {
            let task_id = TaskId::for_source(adapter, &session.session_id);
            stats.tasks += 1;
            let aggregate = session_summary(&summaries);
            // Embed the redacted session summary. Fail-safe: a missing embedder must not
            // block ingest — the task is recorded and can be (re-)embedded later.
            match embedder.embed(&aggregate) {
                Ok(vector) => {
                    storage.put_task_embedding(&task_id, &vector)?;
                    stats.embeddings += 1;
                }
                Err(err) => {
                    storage.append_audit(
                        "embed_skipped",
                        &format!("task={task_id} reason={err}"),
                        chrono::Utc::now(),
                    )?;
                }
            }
        }
    }
    progress.on_done();
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexSource;
    use crate::summarize::DeterministicSummarizer;
    use brain0_model::{
        ArtifactId, ArtifactNode, ArtifactVersion, ChangeKind, ChangeSource, RiskState,
    };
    use brain0_models::LocalEmbeddingProvider;
    use brain0_storage::{local_embed, InMemoryPayloadStore, SqliteStorage, LOCAL_EMBED_DIM};
    use chrono::TimeZone;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn codex_fixture(tag: &str, prompt: &str, patch_path: &str) -> (PathBuf, CodexSource) {
        let dir = std::env::temp_dir().join(format!("brain0-driver-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions/2026/06/06");
        std::fs::create_dir_all(&sessions).unwrap();
        let patch = format!("*** Begin Patch\\n*** Update File: {patch_path}\\n*** End Patch");
        let lines = [
            r#"{"timestamp":"2026-06-06T10:00:00.000Z","type":"session_meta","payload":{"id":"sess-1","cwd":"/home/nicola/demo"}}"#.to_string(),
            format!(r#"{{"timestamp":"2026-06-06T10:00:01.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{prompt}"}}]}}}}"#),
            format!(r#"{{"timestamp":"2026-06-06T10:00:02.000Z","type":"response_item","payload":{{"type":"function_call","name":"apply_patch","arguments":"{{\"input\":\"{patch}\"}}"}}}}"#),
        ];
        let mut f =
            std::fs::File::create(sessions.join("rollout-2026-06-06T10-00-00-x.jsonl")).unwrap();
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }
        (dir.clone(), CodexSource::new(dir))
    }

    #[test]
    fn ingest_creates_tasks_with_provenance_and_is_incremental() {
        let (dir, source) = codex_fixture("a", "fix the lexer", "src/lex.py");
        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let mut registry = SourceRegistry::new();
        registry.register(Box::new(source));
        let scope = ProjectScope::project(Path::new("/home/nicola/demo"));

        let stats = run_ingest(
            &registry,
            &scope,
            None,
            &store,
            &payload,
            &DeterministicSummarizer,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            &Redactor::empty(),
        )
        .unwrap();
        assert_eq!(
            stats,
            IngestStats {
                sessions: 1,
                turns: 1,
                tasks: 1,
                embeddings: 1
            }
        );

        let task_id = TaskId::for_source("codex", "sess-1");
        let task = store
            .get_task_node(&task_id)
            .unwrap()
            .expect("task created");
        assert_eq!(task.source_adapter.as_deref(), Some("codex"));
        assert_eq!(task.session_cwd.as_deref(), Some("/home/nicola/demo"));

        let versions = store.task_versions(&task_id).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].declared[0].path, "src/lex.py");
        // The full prompt is never persisted — only the decision summary is.
        assert!(versions[0].prompt_ref.is_none() && versions[0].decision_summary_ref.is_some());

        // The session is searchable by its embedding.
        let hits = store
            .search_tasks_by_vector(&local_embed("fix the lexer", LOCAL_EMBED_DIM), 5)
            .unwrap();
        assert!(hits.iter().any(|h| h.task_id == task_id));

        // Re-running ingests nothing new (cursor advanced).
        let again = run_ingest(
            &registry,
            &scope,
            None,
            &store,
            &payload,
            &DeterministicSummarizer,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            &Redactor::empty(),
        )
        .unwrap();
        assert_eq!(again.turns, 0);
        assert_eq!(store.task_versions(&task_id).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secrets_are_redacted_before_payload_and_summary() {
        use brain0_storage::PayloadStore as _;
        let (dir, source) =
            codex_fixture("sec", "my aws key AKIAIOSFODNN7EXAMPLE fix it", "src/x.py");
        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let mut registry = SourceRegistry::new();
        registry.register(Box::new(source));
        let scope = ProjectScope::project(Path::new("/home/nicola/demo"));

        // Secure default redactor (built-in secret scanning ON).
        let redactor = Redactor::new(&crate::redact::RedactionConfig::default()).unwrap();
        run_ingest(
            &registry,
            &scope,
            None,
            &store,
            &payload,
            &DeterministicSummarizer,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            &redactor,
        )
        .unwrap();

        let task_id = TaskId::for_source("codex", "sess-1");
        let versions = store.task_versions(&task_id).unwrap();
        let v = &versions[0];
        // The prompt is never persisted, so it cannot leak. The summary is the
        // only persisted task text and is computed from already-redacted input — assert the secret
        // is absent there.
        assert!(
            v.prompt_ref.is_none(),
            "prompt payload must not be persisted"
        );
        let summary = payload
            .get_str(v.decision_summary_ref.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            !summary.contains("AKIAIOSFODNN7EXAMPLE"),
            "secret leaked into summary"
        );

        // The redaction is recorded in the audit log (kind/count only, no value).
        let audit = store.list_audit(10).unwrap();
        assert!(audit.iter().any(|e| e.event_type == "redaction"));
        assert!(!audit
            .iter()
            .any(|e| e.detail.contains("AKIAIOSFODNN7EXAMPLE")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_detects_drift_against_observed_changes() {
        let (dir, source) = codex_fixture("b", "only touch lexer", "src/lex.py");
        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();

        // Seed an OBSERVED change at the same time on a DIFFERENT (undeclared) file.
        let repo = "home/nicola/demo";
        let ts = chrono::Utc.with_ymd_and_hms(2026, 6, 6, 10, 0, 1).unwrap();
        store
            .put_artifact_node(&ArtifactNode {
                id: ArtifactId::new("art_other"),
                level: Level::File,
                repo: repo.into(),
                qualified_path: "src/other.py".into(),
                lang: None,
                parent_id: None,
                current_version: VersionId::new("ver_o"),
                risk: RiskState::default(),
            })
            .unwrap();
        store
            .append_artifact_version(&ArtifactVersion {
                id: VersionId::new("ver_o"),
                artifact_id: ArtifactId::new("art_other"),
                timestamp: ts,
                author: Author::new("dev"),
                agent: Agent::human(),
                source: ChangeSource::Git {
                    commit_sha: "c".into(),
                },
                qualified_path: "src/other.py".into(),
                fingerprint: String::new(),
                change_kind: ChangeKind::Modified,
                lines_added: 1,
                lines_removed: 0,
                diff_ref: None,
            })
            .unwrap();

        let mut registry = SourceRegistry::new();
        registry.register(Box::new(source));
        let scope = ProjectScope::project(Path::new("/home/nicola/demo"));
        run_ingest(
            &registry,
            &scope,
            Some(repo),
            &store,
            &payload,
            &DeterministicSummarizer,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            &Redactor::empty(),
        )
        .unwrap();

        let task_id = TaskId::for_source("codex", "sess-1");
        let versions = store.task_versions(&task_id).unwrap();
        let drift = versions[0].drift.as_ref().expect("drift computed");
        assert!(drift.undeclared.contains(&"src/other.py".to_owned()));
        assert!(drift.phantom.contains(&"src/lex.py".to_owned()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The dogfood false positive: an agent declares a file in an EARLY turn and the commit
    /// containing it lands near a LATER turn. Per-turn semantics flagged the early file as
    /// phantom and every sibling turn's file as undeclared; the session-cumulative semantics
    /// must converge to zero drift on the latest version (the session verdict).
    #[test]
    fn declare_early_commit_late_yields_no_drift_on_the_session_verdict() {
        let dir = std::env::temp_dir().join(format!("brain0-driver-cum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions/2026/06/06");
        std::fs::create_dir_all(&sessions).unwrap();
        let patch_a = "*** Begin Patch\\n*** Update File: src/a.py\\n*** End Patch";
        let patch_b = "*** Begin Patch\\n*** Update File: src/b.py\\n*** End Patch";
        let lines = [
            r#"{"timestamp":"2026-06-06T10:00:00.000Z","type":"session_meta","payload":{"id":"sess-cum","cwd":"/home/nicola/demo"}}"#.to_string(),
            // Turn 1 (10:00): declares a.py.
            r#"{"timestamp":"2026-06-06T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"touch a"}]}}"#.to_string(),
            format!(r#"{{"timestamp":"2026-06-06T10:00:02.000Z","type":"response_item","payload":{{"type":"function_call","name":"apply_patch","arguments":"{{\"input\":\"{patch_a}\"}}"}}}}"#),
            // Turn 2 (10:20): declares b.py. The commit with BOTH files lands at 10:21.
            r#"{"timestamp":"2026-06-06T10:20:00.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"touch b"}]}}"#.to_string(),
            format!(r#"{{"timestamp":"2026-06-06T10:20:01.000Z","type":"response_item","payload":{{"type":"function_call","name":"apply_patch","arguments":"{{\"input\":\"{patch_b}\"}}"}}}}"#),
        ];
        let mut f =
            std::fs::File::create(sessions.join("rollout-2026-06-06T10-00-00-c.jsonl")).unwrap();
        for l in &lines {
            writeln!(f, "{l}").unwrap();
        }

        let store = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let repo = "home/nicola/demo";
        let commit_ts = chrono::Utc.with_ymd_and_hms(2026, 6, 6, 10, 21, 0).unwrap();
        for (art, path) in [("art_a", "src/a.py"), ("art_b", "src/b.py")] {
            store
                .put_artifact_node(&ArtifactNode {
                    id: ArtifactId::new(art),
                    level: Level::File,
                    repo: repo.into(),
                    qualified_path: path.into(),
                    lang: None,
                    parent_id: None,
                    current_version: VersionId::new(format!("ver_{art}")),
                    risk: RiskState::default(),
                })
                .unwrap();
            store
                .append_artifact_version(&ArtifactVersion {
                    id: VersionId::new(format!("ver_{art}")),
                    artifact_id: ArtifactId::new(art),
                    timestamp: commit_ts,
                    author: Author::new("dev"),
                    agent: Agent::human(),
                    source: ChangeSource::Git {
                        commit_sha: "c2".into(),
                    },
                    qualified_path: path.into(),
                    fingerprint: String::new(),
                    change_kind: ChangeKind::Modified,
                    lines_added: 1,
                    lines_removed: 0,
                    diff_ref: None,
                })
                .unwrap();
        }

        let mut registry = SourceRegistry::new();
        registry.register(Box::new(CodexSource::new(dir.clone())));
        let scope = ProjectScope::project(Path::new("/home/nicola/demo"));
        run_ingest(
            &registry,
            &scope,
            Some(repo),
            &store,
            &payload,
            &DeterministicSummarizer,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            &Redactor::empty(),
        )
        .unwrap();

        let task_id = TaskId::for_source("codex", "sess-cum");
        let versions = store.task_versions(&task_id).unwrap();
        assert_eq!(versions.len(), 2);
        let verdict = versions
            .last()
            .unwrap()
            .drift
            .as_ref()
            .expect("drift computed on the last turn");
        assert!(
            !verdict.is_present(),
            "session verdict must be clean; got undeclared={:?} phantom={:?}",
            verdict.undeclared,
            verdict.phantom
        );
        // The per-turn declared record is still turn-granular.
        assert_eq!(versions[0].declared[0].path, "src/a.py");
        assert_eq!(versions[1].declared[0].path, "src/b.py");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
