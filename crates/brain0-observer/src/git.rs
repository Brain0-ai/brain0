//! Git reader mode: git is the source of truth.
//!
//! Strictly **read-only** — we only ever invoke read commands (`log`, `show`); never
//! `commit`, `checkout`, or anything that mutates the repo. Each commit becomes a
//! [`Snapshot`] with its real author, timestamp, message, and per-file changes. Renames
//! are intentionally surfaced as delete+add (`--no-renames`) so brain0's own symbol
//! identity layer decides what is truly the same node.

use std::path::{Path, PathBuf};
use std::process::Command;

use brain0_model::{Agent, Author, ChangeSource};

use crate::{FileState, ObserverError, Result, Snapshot};

/// Reads commits from a git working tree, read-only.
#[derive(Debug, Clone)]
pub struct GitReader {
    repo_path: PathBuf,
    repo_id: String,
}

const FIELD_SEP: char = '\u{1f}';

fn run_git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(ObserverError::Io)?;
    if !output.status.success() {
        return Err(ObserverError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl GitReader {
    /// Open a git working tree. `repo_id` is the canonical repository identifier used for
    /// cross-machine identity (e.g. the remote URL or `org/name`).
    pub fn open(repo_path: impl Into<PathBuf>, repo_id: impl Into<String>) -> Result<Self> {
        let repo_path = repo_path.into();
        let inside = run_git(&repo_path, &["rev-parse", "--is-inside-work-tree"])?;
        if inside.trim() != "true" {
            return Err(ObserverError::Git("not a git work tree".to_owned()));
        }
        Ok(Self {
            repo_path,
            repo_id: repo_id.into(),
        })
    }

    /// Commit SHAs in chronological order (oldest first). If `since` is given, only commits
    /// after it are returned.
    pub fn commits(&self, since: Option<&str>) -> Result<Vec<String>> {
        let range = since.map(|sha| format!("{sha}..HEAD"));
        let mut args = vec!["log", "--reverse", "--format=%H"];
        if let Some(range) = &range {
            args.push(range);
        }
        let out = run_git(&self.repo_path, &args)?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// The current HEAD commit SHA.
    pub fn head(&self) -> Result<String> {
        Ok(run_git(&self.repo_path, &["rev-parse", "HEAD"])?
            .trim()
            .to_owned())
    }

    /// Build a [`Snapshot`] for a single commit.
    pub fn snapshot_for(&self, sha: &str) -> Result<Snapshot> {
        let meta = run_git(
            &self.repo_path,
            &[
                "show",
                "-s",
                &format!("--format=%an{FIELD_SEP}%ae{FIELD_SEP}%aI{FIELD_SEP}%B"),
                sha,
            ],
        )?;
        let mut parts = meta.splitn(4, FIELD_SEP);
        let author_name = parts.next().unwrap_or("").trim().to_owned();
        let author_email = parts.next().unwrap_or("").trim().to_owned();
        let iso = parts.next().unwrap_or("").trim().to_owned();
        let message = parts.next().unwrap_or("").trim().to_owned();

        let timestamp = chrono::DateTime::parse_from_rfc3339(&iso)
            .map_err(|err| ObserverError::Git(format!("bad commit date '{iso}': {err}")))?
            .with_timezone(&chrono::Utc);

        let author = if author_email.is_empty() {
            Author::new(author_name)
        } else {
            Author::with_email(author_name, author_email)
        };

        // Per-file line stats.
        let numstat = run_git(
            &self.repo_path,
            &["show", "--numstat", "--no-renames", "--format=", sha],
        )?;
        let mut stats: std::collections::HashMap<String, (u32, u32)> =
            std::collections::HashMap::new();
        for line in numstat.lines().filter(|l| !l.is_empty()) {
            let mut cols = line.split('\t');
            let added = cols.next().unwrap_or("0").parse().unwrap_or(0);
            let removed = cols.next().unwrap_or("0").parse().unwrap_or(0);
            if let Some(path) = cols.next() {
                stats.insert(path.to_owned(), (added, removed));
            }
        }

        // Per-file change status.
        let name_status = run_git(
            &self.repo_path,
            &["show", "--name-status", "--no-renames", "--format=", sha],
        )?;
        let mut files = Vec::new();
        for line in name_status.lines().filter(|l| !l.is_empty()) {
            let mut cols = line.split('\t');
            let status = cols.next().unwrap_or("");
            let Some(path) = cols.next() else { continue };
            let (lines_added, lines_removed) = stats.get(path).copied().unwrap_or((0, 0));

            if status.starts_with('D') {
                files.push(FileState::deleted(path));
            } else {
                // Added or Modified: fetch the content at this commit.
                let content = run_git(&self.repo_path, &["show", &format!("{sha}:{path}")])?;
                let diff = run_git(
                    &self.repo_path,
                    &["show", "--format=", "--no-renames", sha, "--", path],
                )
                .ok();
                files.push(FileState {
                    rel_path: path.to_owned(),
                    content: Some(content),
                    lines_added,
                    lines_removed,
                    diff,
                });
            }
        }

        Ok(Snapshot {
            repo: self.repo_id.clone(),
            timestamp,
            author,
            agent: Agent::human(),
            source: ChangeSource::Git {
                commit_sha: sha.to_owned(),
            },
            message: if message.is_empty() {
                None
            } else {
                Some(message)
            },
            files,
        })
    }

    /// Build snapshots for all commits after `since` (or all commits), oldest first.
    pub fn snapshots_since(&self, since: Option<&str>) -> Result<Vec<Snapshot>> {
        self.commits(since)?
            .iter()
            .map(|sha| self.snapshot_for(sha))
            .collect()
    }
}
