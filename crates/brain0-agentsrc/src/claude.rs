//! Claude Code adapter: `~/.claude/projects/<ENCODED_CWD>/<sessionId>.jsonl` + per-project
//! `memory/`. See `docs/agent-artifacts.md` for the observed schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use brain0_model::Timestamp;
use serde_json::Value;
use walkdir::WalkDir;

use crate::event::{CapturedRead, IncrementalRead, Provenance, SessionFile, ToolCall, Turn};
use crate::jsonl::read_complete_lines;
use crate::scope::ProjectScope;
use crate::source::AgentArtifactSource;
use crate::Result;

const NAME: &str = "claude-code";

#[derive(Debug)]
pub struct ClaudeSource {
    roots: Vec<PathBuf>,
}

impl ClaudeSource {
    /// `root` is the `.claude/projects` directory.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            roots: vec![root.into()],
        }
    }
}

fn parse_ts(value: &Value) -> Option<Timestamp> {
    let s = value.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Text of a message `content` (string, or the concatenation of its `text` parts).
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn parse_tool_use(part: &Value) -> Option<ToolCall> {
    let name = part.get("name")?.as_str()?.to_owned();
    let input = part.get("input").cloned().unwrap_or(Value::Null);
    let mut declared_paths = Vec::new();
    let mut read_paths = Vec::new();
    let mut command = None;
    match name.as_str() {
        "Edit" | "Write" | "MultiEdit" => {
            if let Some(p) = input.get("file_path").and_then(Value::as_str) {
                declared_paths.push(p.to_owned());
            }
        }
        "NotebookEdit" => {
            if let Some(p) = input.get("notebook_path").and_then(Value::as_str) {
                declared_paths.push(p.to_owned());
            }
        }
        // Reads: what was loaded into the model's context (audit trail). Explicit reads only —
        // edited files are already listed as changes; this surfaces files merely consulted
        // (e.g. a secret read but not modified).
        "Read" => {
            if let Some(p) = input.get("file_path").and_then(Value::as_str) {
                read_paths.push(p.to_owned());
            }
        }
        "NotebookRead" => {
            if let Some(p) = input.get("notebook_path").and_then(Value::as_str) {
                read_paths.push(p.to_owned());
            }
        }
        "Bash" => {
            command = input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        _ => {}
    }
    Some(ToolCall {
        name,
        declared_paths,
        read_paths,
        command,
    })
}

/// The path a Read/NotebookRead tool_use targets (for matching its later tool_result).
fn read_path_of(part: &Value) -> Option<String> {
    let input = part.get("input")?;
    match part.get("name")?.as_str()? {
        "Read" => input
            .get("file_path")
            .and_then(Value::as_str)
            .map(str::to_owned),
        "NotebookRead" => input
            .get("notebook_path")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Read the cwd + session id from the first records of a session file.
fn peek_meta(path: &Path) -> Option<(String, PathBuf)> {
    let (lines, _) = read_complete_lines(path, 0).ok()?;
    for line in lines.iter().take(50) {
        let Ok(v) = serde_json::from_str::<Value>(&line.text) else {
            continue;
        };
        let cwd = v.get("cwd").and_then(Value::as_str);
        let sid = v.get("sessionId").and_then(Value::as_str);
        if let (Some(cwd), Some(sid)) = (cwd, sid) {
            return Some((sid.to_owned(), PathBuf::from(cwd)));
        }
    }
    None
}

impl AgentArtifactSource for ClaudeSource {
    fn name(&self) -> &str {
        NAME
    }

    fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn sessions(&self, scope: &ProjectScope) -> Result<Vec<SessionFile>> {
        let mut out = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            for entry in WalkDir::new(root)
                .max_depth(2)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                if entry.file_type().is_file()
                    && entry.path().extension().is_some_and(|e| e == "jsonl")
                {
                    if let Some((session_id, cwd)) = peek_meta(entry.path()) {
                        if scope.includes(&cwd) {
                            out.push(SessionFile {
                                session_id,
                                path: entry.path().to_path_buf(),
                                cwd,
                            });
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn read_incremental(&self, session: &SessionFile, from_offset: u64) -> Result<IncrementalRead> {
        let (lines, new_offset) = read_complete_lines(&session.path, from_offset)?;
        let mut turns = Vec::new();
        let mut current: Option<Turn> = None;
        let file = session.path.to_string_lossy().to_string();
        // Read tool_use id → path, so the later tool_result (the file content the model saw) can be
        // attached to the turn for secret-scanning. Reset per turn.
        let mut read_calls: HashMap<String, String> = HashMap::new();

        let flush = |turns: &mut Vec<Turn>, current: &mut Option<Turn>| {
            if let Some(mut turn) = current.take() {
                turn.ordinal = turns.len() as u64;
                turns.push(turn);
            }
        };

        for line in &lines {
            let Ok(v) = serde_json::from_str::<Value>(&line.text) else {
                continue;
            };
            let ts = parse_ts(&v).unwrap_or_else(default_ts);
            let message = v.get("message");
            let content = message.and_then(|m| m.get("content"));
            match v.get("type").and_then(Value::as_str) {
                Some("user") => {
                    // First, attach any Read tool_result content (matched by tool_use id) to the
                    // current turn — these messages carry no user text, so they don't start a turn.
                    if let (Some(turn), Some(parts)) =
                        (current.as_mut(), content.and_then(Value::as_array))
                    {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                                continue;
                            }
                            let Some(id) = part.get("tool_use_id").and_then(Value::as_str) else {
                                continue;
                            };
                            if let Some(path) = read_calls.get(id) {
                                let result = text_of(part.get("content"));
                                if !result.is_empty() {
                                    turn.read_contents.push(CapturedRead {
                                        path: path.clone(),
                                        content: result,
                                    });
                                }
                            }
                        }
                    }
                    let text = text_of(content);
                    if !text.trim().is_empty() {
                        flush(&mut turns, &mut current);
                        read_calls.clear();
                        current = Some(Turn {
                            session_id: session.session_id.clone(),
                            cwd: session.cwd.clone(),
                            timestamp: ts,
                            ordinal: 0,
                            prompt: Some(text),
                            assistant_text: String::new(),
                            model: None,
                            tool_calls: Vec::new(),
                            provenance: Provenance {
                                adapter: NAME.to_owned(),
                                file: file.clone(),
                                byte_offset: line.offset,
                            },
                            read_contents: Vec::new(),
                        });
                    }
                }
                Some("assistant") => {
                    if let Some(turn) = current.as_mut() {
                        // The model is recorded on the assistant message; keep the first seen.
                        if turn.model.is_none() {
                            turn.model = message
                                .and_then(|m| m.get("model"))
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                        }
                        if let Some(parts) = content.and_then(Value::as_array) {
                            for part in parts {
                                match part.get("type").and_then(Value::as_str) {
                                    Some("text") => {
                                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                                            if !turn.assistant_text.is_empty() {
                                                turn.assistant_text.push('\n');
                                            }
                                            turn.assistant_text.push_str(t);
                                        }
                                    }
                                    Some("tool_use") => {
                                        if let Some(tc) = parse_tool_use(part) {
                                            turn.tool_calls.push(tc);
                                        }
                                        // Remember Read calls by id → path for tool_result matching.
                                        if let (Some(id), Some(path)) = (
                                            part.get("id").and_then(Value::as_str),
                                            read_path_of(part),
                                        ) {
                                            read_calls.insert(id.to_owned(), path);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        flush(&mut turns, &mut current);
        Ok(IncrementalRead { turns, new_offset })
    }

    fn memory_files(&self, scope: &ProjectScope) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            for project in std::fs::read_dir(root)?.filter_map(std::result::Result::ok) {
                let dir = project.path();
                if !dir.is_dir() {
                    continue;
                }
                // Determine the project's cwd from one of its sessions to honor the scope.
                let cwd = std::fs::read_dir(&dir)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(std::result::Result::ok)
                    .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                    .and_then(|e| peek_meta(&e.path()))
                    .map(|(_, cwd)| cwd);
                let in_scope = cwd.as_deref().map(|c| scope.includes(c)).unwrap_or(false);
                if !in_scope {
                    continue;
                }
                let mem = dir.join("memory");
                if mem.exists() {
                    for entry in WalkDir::new(&mem)
                        .into_iter()
                        .filter_map(std::result::Result::ok)
                    {
                        if entry.file_type().is_file() {
                            out.push(entry.path().to_path_buf());
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

fn default_ts() -> Timestamp {
    use chrono::TimeZone;
    chrono::Utc.timestamp_opt(0, 0).single().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(projects: &Path, enc: &str, name: &str, lines: &[&str]) {
        let dir = projects.join(enc);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn parses_turns_cwd_and_declared_changes() {
        let projects = std::env::temp_dir().join(format!("brain0-claude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(&projects).unwrap();
        write_session(
            &projects,
            "-home-nicola-progetti-demo",
            "sess1.jsonl",
            &[
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess1","timestamp":"2026-06-06T10:00:00.000Z","message":{"role":"user","content":"refactor the parser"}}"#,
                // Claude Code records ABSOLUTE tool_use paths; they must be normalized to
                // repo-relative so reconciliation matches the observer's git paths.
                r#"{"type":"assistant","cwd":"/home/nicola/progetti/demo","sessionId":"sess1","timestamp":"2026-06-06T10:00:01.000Z","message":{"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"text","text":"editing"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/home/nicola/progetti/demo/src/parse.py"}},{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/home/nicola/progetti/demo/src/config.py"}}]}}"#,
            ],
        );

        let source = ClaudeSource::new(&projects);
        let scope = ProjectScope::project(Path::new("/home/nicola/progetti/demo"));
        let sessions = source.sessions(&scope).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess1");

        let read = source.read_incremental(&sessions[0], 0).unwrap();
        assert_eq!(read.turns.len(), 1);
        let turn = &read.turns[0];
        assert_eq!(turn.prompt.as_deref(), Some("refactor the parser"));
        assert_eq!(turn.assistant_text, "editing");
        assert_eq!(turn.declared_paths(), vec!["src/parse.py".to_owned()]);
        // The Read tool is captured as a read (audit trail), normalized repo-relative.
        assert_eq!(turn.read_paths(), vec!["src/config.py".to_owned()]);
        // The model is captured from the assistant message (DLP + provenance).
        assert_eq!(turn.model.as_deref(), Some("claude-sonnet-4-6"));

        let other = ProjectScope::project(Path::new("/home/nicola/progetti/elsewhere"));
        assert!(source.sessions(&other).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&projects);
    }
}
