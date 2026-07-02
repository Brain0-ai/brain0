//! Normalized event/turn model shared by every adapter.
//!
//! Adapters parse their agent-specific transcript format into these neutral types. A
//! **turn** is the unit of work — a user prompt and everything (assistant text + tool
//! calls) until the next prompt — and is what the ingest pipeline summarizes and embeds.

use std::path::{Path, PathBuf};

use brain0_model::Timestamp;
use serde::{Deserialize, Serialize};

/// Where a normalized event came from — for audit and cursor management.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub adapter: String,
    pub file: String,
    pub byte_offset: u64,
}

/// A tool call the agent made during a turn (the *declared* changes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool name (e.g. `Edit`, `Write`, `Bash`, `apply_patch`, `exec_command`).
    pub name: String,
    /// Repo-relative or absolute file paths the call declares it *wrote*.
    pub declared_paths: Vec<String>,
    /// File paths the call *read* (e.g. the `Read` tool) — the audit trail of what was loaded
    /// into the model's context. Distinct from `declared_paths` (writes).
    #[serde(default)]
    pub read_paths: Vec<String>,
    /// Shell command, if this was a shell tool.
    pub command: Option<String>,
}

/// The content a Read tool returned (the file content the model saw). Held transiently on a
/// [`Turn`] so the driver can secret-scan it at ingest — it is NEVER persisted.
#[derive(Debug, Clone)]
pub struct CapturedRead {
    pub path: String,
    pub content: String,
}

/// One turn of a session.
#[derive(Debug, Clone)]
pub struct Turn {
    pub session_id: String,
    pub cwd: PathBuf,
    pub timestamp: Timestamp,
    /// 0-based turn index within the session.
    pub ordinal: u64,
    /// The user prompt that opened the turn, if any.
    pub prompt: Option<String>,
    /// Concatenated assistant text for the turn.
    pub assistant_text: String,
    /// The model that produced the turn (e.g. `claude-sonnet-4-6`), when the transcript records it.
    pub model: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub provenance: Provenance,
    /// Read results captured this turn (path + content). Transient: scanned for secrets at ingest,
    /// never written to storage.
    pub read_contents: Vec<CapturedRead>,
}

impl Turn {
    /// Deduplicated declared file paths across the turn's tool calls, each normalized to
    /// repo-relative against the session `cwd`. This is the single point every adapter funnels
    /// through, so the normalization fixes Claude Code (absolute tool_use paths) and Codex alike.
    #[must_use]
    pub fn declared_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .tool_calls
            .iter()
            .flat_map(|tc| tc.declared_paths.iter())
            .map(|p| normalize_declared(p, &self.cwd))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Deduplicated file paths the turn **read**, normalized repo-relative against the session
    /// `cwd` (paths outside the project stay absolute — the audit-relevant case). This is the
    /// metadata trail of which files reached the model's context.
    #[must_use]
    pub fn read_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .tool_calls
            .iter()
            .flat_map(|tc| tc.read_paths.iter())
            .map(|p| normalize_declared(p, &self.cwd))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Captured read results paired with their repo-relative-normalized path (out-of-repo paths
    /// stay absolute), for secret-scanning at ingest. Content is borrowed; never stored.
    #[must_use]
    pub fn reads_with_content(&self) -> Vec<(String, &str)> {
        self.read_contents
            .iter()
            .map(|c| (normalize_declared(&c.path, &self.cwd), c.content.as_str()))
            .collect()
    }

    /// The text used for the decision summary / embedding: prompt + assistant text.
    #[must_use]
    pub fn summary_source(&self) -> String {
        let mut parts = Vec::new();
        if let Some(prompt) = &self.prompt {
            parts.push(prompt.clone());
        }
        if !self.assistant_text.is_empty() {
            parts.push(self.assistant_text.clone());
        }
        parts.join("\n")
    }
}

/// Normalize a declared path to repo-relative against the session `cwd`. Agents like Claude Code
/// record absolute tool_use paths (`/home/me/proj/src/x.ts`) while the observer records git paths
/// repo-relative (`src/x.ts`); without this, reconciliation never matches them and reports every
/// declared file as spurious drift (both phantom and undeclared). A path already relative, or
/// absolute but outside the project, is returned unchanged.
fn normalize_declared(path: &str, cwd: &Path) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        if let Ok(rel) = p.strip_prefix(cwd) {
            return rel.to_string_lossy().into_owned();
        }
    }
    path.to_owned()
}

/// A discovered session file with its resolved working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub session_id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
}

/// The result of an incremental read: new turns plus the advanced byte offset.
#[derive(Debug, Clone)]
pub struct IncrementalRead {
    pub turns: Vec<Turn>,
    pub new_offset: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn normalize_declared_makes_absolute_in_project_repo_relative() {
        let cwd = Path::new("/home/nicola/progetti/brain0");
        // Absolute path inside the project → repo-relative (matches the observer's git paths).
        assert_eq!(
            normalize_declared("/home/nicola/progetti/brain0/packages/gui/src/main.ts", cwd),
            "packages/gui/src/main.ts"
        );
        // Already relative → unchanged.
        assert_eq!(normalize_declared("src/parse.py", cwd), "src/parse.py");
        // Absolute but outside the project → unchanged (cannot be made repo-relative).
        assert_eq!(normalize_declared("/etc/passwd", cwd), "/etc/passwd");
    }

    #[test]
    fn declared_paths_normalizes_then_dedups_across_adapters() {
        use chrono::TimeZone;
        let ts = chrono::Utc.timestamp_opt(0, 0).single().unwrap();
        let turn = Turn {
            session_id: "s".to_owned(),
            cwd: PathBuf::from("/home/nicola/progetti/brain0"),
            timestamp: ts,
            ordinal: 0,
            prompt: None,
            assistant_text: String::new(),
            tool_calls: vec![
                ToolCall {
                    name: "Read".to_owned(),
                    declared_paths: vec![],
                    // An in-project absolute read and an OUT-of-project read (the audit case).
                    read_paths: vec![
                        "/home/nicola/progetti/brain0/packages/shared/src/store.ts".to_owned(),
                        "/home/dev/.aws/credentials".to_owned(),
                    ],
                    command: None,
                },
                ToolCall {
                    name: "Edit".to_owned(),
                    declared_paths: vec![
                        "/home/nicola/progetti/brain0/packages/gui/src/main.ts".to_owned()
                    ],
                    read_paths: vec![],
                    command: None,
                },
                ToolCall {
                    // Same file via a relative path → must dedup with the normalized absolute one.
                    name: "Write".to_owned(),
                    declared_paths: vec!["packages/gui/src/main.ts".to_owned()],
                    read_paths: vec![],
                    command: None,
                },
            ],
            model: None,
            provenance: Provenance {
                adapter: "claude-code".to_owned(),
                file: "f".to_owned(),
                byte_offset: 0,
            },
            read_contents: Vec::new(),
        };
        assert_eq!(
            turn.declared_paths(),
            vec!["packages/gui/src/main.ts".to_owned()]
        );
        // Reads: in-project absolute → repo-relative; out-of-project stays absolute (flagged).
        assert_eq!(
            turn.read_paths(),
            vec![
                "/home/dev/.aws/credentials".to_owned(),
                "packages/shared/src/store.ts".to_owned(),
            ]
        );
    }
}
