//! Claude Code adapter: `~/.claude/projects/<ENCODED_CWD>/<sessionId>.jsonl`, the subagent
//! transcripts under `<sessionId>/subagents/`, and the per-project `memory/`. See
//! `docs/agent-artifacts.md` for the observed schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use brain0_model::Timestamp;
use serde_json::Value;
use walkdir::WalkDir;

use crate::event::{CapturedRead, IncrementalRead, Provenance, SessionFile, ToolCall, Turn};
use crate::jsonl::read_complete_lines;
use crate::scope::ProjectScope;
use crate::shell::read_paths_from_command;
use crate::source::AgentArtifactSource;
use crate::Result;

const NAME: &str = "claude-code";

/// Upper bound on a spilled tool result (`<sessionId>/tool-results/<id>.txt`) we will load back
/// for secret scanning. Untrusted input; larger files are scanned only through their preview.
const MAX_SPILLED_RESULT_BYTES: u64 = 16 * 1024 * 1024;

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
        // A shell read (`cat .env`, `sed -n …`, `grep -rn … src/`) puts file content in front
        // of the model exactly like `Read` does; the command line is parsed best-effort.
        "Bash" => {
            command = input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(c) = &command {
                read_paths = read_paths_from_command(c);
            }
        }
        // `Grep` in content mode returns matching lines: a (partial) read of everything under
        // its target, the repo root when no path is given.
        "Grep" => {
            if let Some(p) = grep_content_target(&input) {
                read_paths.push(p);
            }
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

/// The target of a `Grep` tool call whose result carries file content (`output_mode:
/// "content"`); `files_with_matches` / `count` modes expose paths only, not content.
fn grep_content_target(input: &Value) -> Option<String> {
    if input.get("output_mode").and_then(Value::as_str) != Some("content") {
        return None;
    }
    Some(
        input
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .unwrap_or(".")
            .to_owned(),
    )
}

/// The paths a tool_use reads (for matching its later tool_result, whose content is what the
/// model saw). A shell command may read several files; its single output is attributed to each.
fn read_paths_of(part: &Value) -> Vec<String> {
    parse_tool_use(part)
        .map(|tc| tc.read_paths)
        .unwrap_or_default()
}

/// The full text of a tool result as the model saw it. Large results are spilled to
/// `<sessionId>/tool-results/<id>.txt` with only a preview inline; the record's top-level
/// `toolUseResult` (stdout / file content) still carries the full text, and the spilled file
/// is loaded as a fallback. Everything here is transient: scanned for secrets, never stored.
fn tool_result_text(record: &Value, part: &Value) -> String {
    let mut text = text_of(part.get("content"));
    if let Some(full) = record.get("toolUseResult") {
        for field in [
            full.get("stdout"),
            full.get("stderr"),
            full.get("file").and_then(|f| f.get("content")),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        {
            text.push('\n');
            text.push_str(field);
        }
    }
    if let Some(path) = spilled_result_path(&text) {
        if std::fs::metadata(&path)
            .is_ok_and(|m| m.is_file() && m.len() <= MAX_SPILLED_RESULT_BYTES)
        {
            if let Ok(full) = std::fs::read_to_string(&path) {
                text.push('\n');
                text.push_str(&full);
            }
        }
    }
    text
}

/// The `Full output saved to: <path>` reference inside a `<persisted-output>` preview.
fn spilled_result_path(text: &str) -> Option<PathBuf> {
    let marker = "Full output saved to: ";
    let start = text.find("<persisted-output>")?;
    let rest = &text[start..];
    let path = rest.split_once(marker)?.1.lines().next()?.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
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
            // Depth 2: `<project>/<sessionId>.jsonl`. Depth 4: the subagent transcripts
            // `<project>/<sessionId>/subagents/agent-<id>.jsonl`, whose reads reach a model just
            // the same (they carry the parent `sessionId` + `cwd`, so they merge into its task).
            for entry in WalkDir::new(root)
                .max_depth(4)
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
        // Reading tool_use id → paths, so the later tool_result (the file content the model saw)
        // can be attached to the turn for secret-scanning. Reset per turn.
        let mut read_calls: HashMap<String, Vec<String>> = HashMap::new();

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
                            if let Some(paths) = read_calls.get(id) {
                                let result = tool_result_text(&v, part);
                                if !result.is_empty() {
                                    for path in paths {
                                        turn.read_contents.push(CapturedRead {
                                            path: path.clone(),
                                            content: result.clone(),
                                        });
                                    }
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
                                        // Remember reading calls by id → paths for tool_result
                                        // matching (Read, NotebookRead, Grep, shell reads).
                                        let paths = read_paths_of(part);
                                        if let (Some(id), false) = (
                                            part.get("id").and_then(Value::as_str),
                                            paths.is_empty(),
                                        ) {
                                            read_calls.insert(id.to_owned(), paths);
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

    #[test]
    fn shell_and_grep_reads_are_captured_with_the_content_the_model_saw() {
        let projects =
            std::env::temp_dir().join(format!("brain0-claude-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(&projects).unwrap();
        write_session(
            &projects,
            "-home-nicola-progetti-demo",
            "sess2.jsonl",
            &[
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess2","timestamp":"2026-06-06T10:00:00.000Z","message":{"role":"user","content":"check the config"}}"#,
                // Reads through the shell and through Grep (content mode) reach the model like
                // `Read` does; `ls` is not a read; `Grep` in files_with_matches mode returns no content.
                r#"{"type":"assistant","cwd":"/home/nicola/progetti/demo","sessionId":"sess2","timestamp":"2026-06-06T10:00:01.000Z","message":{"role":"assistant","model":"claude-sonnet-4-6","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cat .env | head -3"}},{"type":"tool_use","id":"b2","name":"Bash","input":{"command":"ls -la"}},{"type":"tool_use","id":"g1","name":"Grep","input":{"pattern":"KEY","path":"/home/nicola/progetti/demo/config","output_mode":"content"}},{"type":"tool_use","id":"g2","name":"Grep","input":{"pattern":"KEY"}}]}}"#,
                // The inline content is only a preview; the harness keeps the full stdout in
                // `toolUseResult`, which is where the secret actually is.
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess2","timestamp":"2026-06-06T10:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b1","content":"DB_HOST=localhost"}]},"toolUseResult":{"stdout":"DB_HOST=localhost\nAWS_KEY=AKIAIOSFODNN7EXAMPLE","stderr":""}}"#,
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess2","timestamp":"2026-06-06T10:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"g1","content":"config/app.py:3:KEY = 'x'"}]}}"#,
            ],
        );

        let source = ClaudeSource::new(&projects);
        let scope = ProjectScope::project(Path::new("/home/nicola/progetti/demo"));
        let sessions = source.sessions(&scope).unwrap();
        let read = source.read_incremental(&sessions[0], 0).unwrap();
        let turn = &read.turns[0];
        assert_eq!(
            turn.read_paths(),
            vec![".env".to_owned(), "config".to_owned()]
        );
        assert!(turn
            .tool_calls
            .iter()
            .any(|tc| tc.name == "Bash" && tc.read_paths.is_empty()));

        let contents = turn.reads_with_content();
        let env = contents
            .iter()
            .find(|(p, _)| p == ".env")
            .expect("shell read content");
        assert!(
            env.1.contains("AKIAIOSFODNN7EXAMPLE"),
            "full stdout from toolUseResult"
        );
        let cfg = contents
            .iter()
            .find(|(p, _)| p == "config")
            .expect("grep content");
        assert!(cfg.1.contains("KEY = 'x'"));
        let _ = std::fs::remove_dir_all(&projects);
    }

    #[test]
    fn spilled_tool_results_are_loaded_from_disk_for_scanning() {
        let projects =
            std::env::temp_dir().join(format!("brain0-claude-spill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        let results = projects.join("-home-nicola-progetti-demo/sess3/tool-results");
        std::fs::create_dir_all(&results).unwrap();
        let spilled = results.join("abc.txt");
        std::fs::write(&spilled, "line1\nTOKEN=AKIAIOSFODNN7EXAMPLE\n").unwrap();
        let preview = format!(
            "<persisted-output>\nOutput too large (30KB). Full output saved to: {}\n\nPreview (first 2KB):\nline1",
            spilled.display()
        );
        let record = serde_json::json!({
            "type": "user", "cwd": "/home/nicola/progetti/demo", "sessionId": "sess3",
            "timestamp": "2026-06-06T10:00:02.000Z",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b1", "content": preview}]}
        })
        .to_string();
        write_session(
            &projects,
            "-home-nicola-progetti-demo",
            "sess3.jsonl",
            &[
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess3","timestamp":"2026-06-06T10:00:00.000Z","message":{"role":"user","content":"dump"}}"#,
                r#"{"type":"assistant","cwd":"/home/nicola/progetti/demo","sessionId":"sess3","timestamp":"2026-06-06T10:00:01.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cat big.log"}}]}}"#,
                &record,
            ],
        );
        let source = ClaudeSource::new(&projects);
        let scope = ProjectScope::project(Path::new("/home/nicola/progetti/demo"));
        let sessions = source.sessions(&scope).unwrap();
        let session = sessions.iter().find(|s| s.session_id == "sess3").unwrap();
        let read = source.read_incremental(session, 0).unwrap();
        let contents = read.turns[0].reads_with_content();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].0, "big.log");
        assert!(
            contents[0].1.contains("AKIAIOSFODNN7EXAMPLE"),
            "spilled content loaded"
        );
        let _ = std::fs::remove_dir_all(&projects);
    }

    #[test]
    fn subagent_transcripts_are_discovered_under_the_session_directory() {
        let projects =
            std::env::temp_dir().join(format!("brain0-claude-sub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&projects);
        std::fs::create_dir_all(&projects).unwrap();
        write_session(
            &projects,
            "-home-nicola-progetti-demo",
            "sess4.jsonl",
            &[
                r#"{"type":"user","cwd":"/home/nicola/progetti/demo","sessionId":"sess4","timestamp":"2026-06-06T10:00:00.000Z","message":{"role":"user","content":"explore"}}"#,
            ],
        );
        write_session(
            &projects,
            "-home-nicola-progetti-demo/sess4/subagents",
            "agent-a1b2.jsonl",
            &[
                r#"{"type":"user","isSidechain":true,"agentId":"a1b2","cwd":"/home/nicola/progetti/demo","sessionId":"sess4","timestamp":"2026-06-06T10:00:05.000Z","message":{"role":"user","content":"find the db config"}}"#,
                r#"{"type":"assistant","isSidechain":true,"agentId":"a1b2","cwd":"/home/nicola/progetti/demo","sessionId":"sess4","timestamp":"2026-06-06T10:00:06.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/home/nicola/progetti/demo/.env"}}]}}"#,
            ],
        );
        // Unrelated files under the session directory are not transcripts.
        let tr = projects.join("-home-nicola-progetti-demo/sess4/tool-results");
        std::fs::create_dir_all(&tr).unwrap();
        std::fs::write(tr.join("x.txt"), "not a transcript").unwrap();

        let source = ClaudeSource::new(&projects);
        let scope = ProjectScope::project(Path::new("/home/nicola/progetti/demo"));
        let mut sessions = source.sessions(&scope).unwrap();
        sessions.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(sessions.len(), 2, "main session + its subagent transcript");
        // The subagent carries the parent session id, so its turns merge into the same task.
        assert!(sessions.iter().all(|s| s.session_id == "sess4"));
        let sub = sessions
            .iter()
            .find(|s| s.path.ends_with("agent-a1b2.jsonl"))
            .unwrap();
        let read = source.read_incremental(sub, 0).unwrap();
        assert_eq!(read.turns[0].read_paths(), vec![".env".to_owned()]);
        let _ = std::fs::remove_dir_all(&projects);
    }
}
