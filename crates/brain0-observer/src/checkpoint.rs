//! Checkpoint engine: the no-git fallback.
//!
//! When a repo has no git, a filesystem watcher captures writes and the checkpoint engine
//! produces dated snapshots at iteration boundaries (or on a debounce), so the data model
//! always has discrete, dated versions. It is **not** a VCS — no branches, no merges — just
//! a shadow timeline that fills the gap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use brain0_model::{Agent, Author, ChangeSource, Timestamp};
use walkdir::WalkDir;

use crate::{FileState, ObserverError, Result, Snapshot};

/// Directories never scanned by the checkpoint engine.
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", "dist", ".brain0", ".venv"];

fn is_ignored(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|name| IGNORED_DIRS.contains(&name))
        .unwrap_or(false)
}

/// Read all (UTF-8) files under `root`, returning `rel_path -> content`. Non-UTF-8 files
/// are skipped (brain0 indexes source text).
fn read_tree(root: &Path) -> Result<HashMap<String, String>> {
    let mut files = HashMap::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
    {
        let entry = entry.map_err(|e| ObserverError::Io(e.into()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if let Ok(content) = std::fs::read_to_string(path) {
            files.insert(rel, content);
        }
    }
    Ok(files)
}

/// Multiset line delta between two file contents → (added, removed).
fn line_delta(old: &str, new: &str) -> (u32, u32) {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for line in new.lines() {
        *counts.entry(line).or_default() += 1;
    }
    for line in old.lines() {
        *counts.entry(line).or_default() -= 1;
    }
    let mut added = 0u32;
    let mut removed = 0u32;
    for delta in counts.values() {
        if *delta > 0 {
            added += *delta as u32;
        } else if *delta < 0 {
            removed += (-*delta) as u32;
        }
    }
    (added, removed)
}

fn checkpoint_id(timestamp: &Timestamp, files: &HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, String)> = files
        .iter()
        .map(|(path, content)| (path, blake3::hash(content.as_bytes()).to_hex().to_string()))
        .collect();
    entries.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(timestamp.to_rfc3339().as_bytes());
    for (path, hash) in entries {
        hasher.update(path.as_bytes());
        hasher.update(hash.as_bytes());
    }
    format!("cp_{}", &hasher.finalize().to_hex().as_str()[..16])
}

/// The full current tree under `root` as `(rel_path, content)` pairs, sorted by path — the
/// manifest `brain0 rewind` persists per checkpoint so any recorded state can be restored.
pub fn full_tree(root: impl AsRef<Path>) -> Result<Vec<(String, String)>> {
    let mut files: Vec<(String, String)> = read_tree(root.as_ref())?.into_iter().collect();
    files.sort();
    Ok(files)
}

/// Build a full snapshot of a directory's current state (every file as modified). Useful
/// for the first checkpoint or a one-shot scan.
pub fn snapshot_directory(
    root: impl AsRef<Path>,
    repo: impl Into<String>,
    author: Author,
    agent: Agent,
    timestamp: Timestamp,
) -> Result<Snapshot> {
    let files_map = read_tree(root.as_ref())?;
    let id = checkpoint_id(&timestamp, &files_map);
    let files = files_map
        .into_iter()
        .map(|(rel_path, content)| {
            let lines = content.lines().count() as u32;
            FileState {
                rel_path,
                content: Some(content),
                lines_added: lines,
                lines_removed: 0,
                diff: None,
            }
        })
        .collect();
    Ok(Snapshot {
        repo: repo.into(),
        timestamp,
        author,
        agent,
        source: ChangeSource::Checkpoint { checkpoint_id: id },
        message: Some("checkpoint".to_owned()),
        files,
    })
}

/// Stateful checkpoint engine: tracks the previous tree so each checkpoint contains only
/// the files that actually changed, with line deltas.
#[derive(Debug)]
pub struct CheckpointEngine {
    root: PathBuf,
    repo: String,
    author: Author,
    agent: Agent,
    previous: HashMap<String, String>,
}

impl CheckpointEngine {
    /// Create an engine for `root`. The first [`checkpoint`](Self::checkpoint) reports every
    /// file as added.
    pub fn new(
        root: impl Into<PathBuf>,
        repo: impl Into<String>,
        author: Author,
        agent: Agent,
    ) -> Self {
        Self {
            root: root.into(),
            repo: repo.into(),
            author,
            agent,
            previous: HashMap::new(),
        }
    }

    /// Scan the tree and produce a snapshot of the files that changed since the last
    /// checkpoint (added/modified/deleted), then update the engine's baseline. Returns
    /// `None` when nothing changed.
    pub fn checkpoint(&mut self, timestamp: Timestamp) -> Result<Option<Snapshot>> {
        let current = read_tree(&self.root)?;
        let mut files = Vec::new();

        for (rel_path, content) in &current {
            match self.previous.get(rel_path) {
                Some(prev) if prev == content => {}
                Some(prev) => {
                    let (added, removed) = line_delta(prev, content);
                    files.push(FileState {
                        rel_path: rel_path.clone(),
                        content: Some(content.clone()),
                        lines_added: added,
                        lines_removed: removed,
                        diff: None,
                    });
                }
                None => {
                    files.push(FileState {
                        rel_path: rel_path.clone(),
                        content: Some(content.clone()),
                        lines_added: content.lines().count() as u32,
                        lines_removed: 0,
                        diff: None,
                    });
                }
            }
        }
        for rel_path in self.previous.keys() {
            if !current.contains_key(rel_path) {
                files.push(FileState::deleted(rel_path.clone()));
            }
        }

        self.previous = current.clone();
        if files.is_empty() {
            return Ok(None);
        }

        let id = checkpoint_id(&timestamp, &current);
        Ok(Some(Snapshot {
            repo: self.repo.clone(),
            timestamp,
            author: self.author.clone(),
            agent: self.agent.clone(),
            source: ChangeSource::Checkpoint { checkpoint_id: id },
            message: Some("checkpoint".to_owned()),
            files,
        }))
    }

    /// Watch the tree and invoke `on_checkpoint` with each debounced snapshot. Blocks until
    /// the watcher errors or is dropped. `debounce` groups bursts of writes into a single
    /// checkpoint at an iteration boundary.
    pub fn watch<F>(&mut self, debounce: Duration, mut on_checkpoint: F) -> Result<()>
    where
        F: FnMut(&Snapshot),
    {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| ObserverError::Git(format!("watcher: {e}")))?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| ObserverError::Git(format!("watch: {e}")))?;

        while rx.recv().is_ok() {
            // Drain the burst within the debounce window.
            while rx.recv_timeout(debounce).is_ok() {}
            if let Some(snapshot) = self.checkpoint(chrono::Utc::now())? {
                on_checkpoint(&snapshot);
            }
        }
        Ok(())
    }
}
