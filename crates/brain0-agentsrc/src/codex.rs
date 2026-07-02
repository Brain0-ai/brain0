//! Codex adapter: `~/.codex/sessions/<Y>/<M>/<D>/rollout-*.jsonl` + `~/.codex/memories`.
//! See `docs/agent-artifacts.md` for the observed schema.

use std::path::{Path, PathBuf};

use brain0_model::Timestamp;
use serde_json::Value;
use walkdir::WalkDir;

use crate::event::{IncrementalRead, Provenance, SessionFile, ToolCall, Turn};
use crate::jsonl::read_complete_lines;
use crate::scope::ProjectScope;
use crate::source::AgentArtifactSource;
use crate::Result;

const NAME: &str = "codex";

#[derive(Debug)]
pub struct CodexSource {
    roots: Vec<PathBuf>,
}

impl CodexSource {
    /// `root` is the `.codex` directory.
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

/// Extract `*** Add/Update/Delete File: <path>` headers from an apply_patch body.
fn paths_from_patch(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            for prefix in [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    return Some(rest.trim().to_owned());
                }
            }
            None
        })
        .collect()
}

fn parse_function_call(payload: &Value) -> Option<ToolCall> {
    let name = payload.get("name")?.as_str()?.to_owned();
    let args_str = payload
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);

    let mut declared_paths = Vec::new();
    let mut command = None;
    match name.as_str() {
        "apply_patch" => {
            let patch = args
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or(args_str);
            declared_paths = paths_from_patch(patch);
        }
        "exec_command" | "shell" | "bash" | "local_shell" => {
            command = args
                .get("command")
                .map(|c| c.to_string())
                .or_else(|| args.get("cmd").map(|c| c.to_string()));
        }
        _ => {}
    }
    Some(ToolCall {
        name,
        declared_paths,
        // Codex reads files via shell (cat/sed), which is not reliably parseable, so reads are not
        // captured here yet; the field stays empty rather than guessing.
        read_paths: Vec::new(),
        command,
    })
}

/// Read the session id + cwd from the `session_meta` record (first lines).
fn peek_meta(path: &Path) -> Option<(String, PathBuf)> {
    let (lines, _) = read_complete_lines(path, 0).ok()?;
    for line in lines.iter().take(50) {
        let v: Value = serde_json::from_str(&line.text).ok()?;
        if v.get("type").and_then(Value::as_str) == Some("session_meta") {
            let p = v.get("payload")?;
            let id = p.get("id")?.as_str()?.to_owned();
            let cwd = p.get("cwd")?.as_str()?.to_owned();
            return Some((id, PathBuf::from(cwd)));
        }
    }
    None
}

impl AgentArtifactSource for CodexSource {
    fn name(&self) -> &str {
        NAME
    }

    fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn sessions(&self, scope: &ProjectScope) -> Result<Vec<SessionFile>> {
        let mut out = Vec::new();
        for root in &self.roots {
            let sessions_dir = root.join("sessions");
            if !sessions_dir.exists() {
                continue;
            }
            for entry in WalkDir::new(&sessions_dir)
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
        let mut last_ts: Option<Timestamp> = None;
        let file = session.path.to_string_lossy().to_string();

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
            if let Some(ts) = parse_ts(&v) {
                last_ts = Some(ts);
            }
            if v.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let Some(payload) = v.get("payload") else {
                continue;
            };
            match payload.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                    let text = collect_text(payload.get("content"));
                    if role == "user" {
                        flush(&mut turns, &mut current);
                        current = Some(Turn {
                            session_id: session.session_id.clone(),
                            cwd: session.cwd.clone(),
                            timestamp: last_ts.unwrap_or_else(default_ts),
                            ordinal: 0,
                            prompt: Some(text),
                            assistant_text: String::new(),
                            model: None, // codex model capture deferred
                            tool_calls: Vec::new(),
                            provenance: Provenance {
                                adapter: NAME.to_owned(),
                                file: file.clone(),
                                byte_offset: line.offset,
                            },
                            // Codex reads files via shell; no clean tool_result to capture yet.
                            read_contents: Vec::new(),
                        });
                    } else if role == "assistant" {
                        if let Some(turn) = current.as_mut() {
                            if !turn.assistant_text.is_empty() {
                                turn.assistant_text.push('\n');
                            }
                            turn.assistant_text.push_str(&text);
                        }
                    }
                }
                Some("function_call") => {
                    if let (Some(turn), Some(tc)) = (current.as_mut(), parse_function_call(payload))
                    {
                        turn.tool_calls.push(tc);
                    }
                }
                _ => {}
            }
        }
        flush(&mut turns, &mut current);
        Ok(IncrementalRead { turns, new_offset })
    }

    fn memory_files(&self, _scope: &ProjectScope) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for root in &self.roots {
            let agents = root.join("AGENTS.md");
            if agents.exists() {
                out.push(agents);
            }
            let mem = root.join("memories");
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
        Ok(out)
    }
}

fn collect_text(content: Option<&Value>) -> String {
    let Some(arr) = content.and_then(Value::as_array) else {
        return content.and_then(Value::as_str).unwrap_or("").to_owned();
    };
    arr.iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn default_ts() -> Timestamp {
    use chrono::TimeZone;
    chrono::Utc.timestamp_opt(0, 0).single().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let sessions = dir.join("sessions/2026/06/06");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn parses_turns_cwd_and_declared_changes() {
        let dir = std::env::temp_dir().join(format!("brain0-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A relative apply_patch path (the common case) and an absolute one — both must end up
        // repo-relative so reconciliation matches the observer's git paths (shared normalization).
        let patch = "*** Begin Patch\\n*** Update File: src/lex.py\\n*** Update File: /home/nicola/progetti/demo/src/abs.py\\n@@\\n-x\\n+y\\n*** End Patch";
        write_session(
            &dir,
            "rollout-2026-06-06T10-00-00-abc.jsonl",
            &[
                r#"{"timestamp":"2026-06-06T10:00:00.000Z","type":"session_meta","payload":{"id":"sess-abc","cwd":"/home/nicola/progetti/demo"}}"#,
                r#"{"timestamp":"2026-06-06T10:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the lexer"}]}}"#,
                r#"{"timestamp":"2026-06-06T10:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"editing lexer"}]}}"#,
                &format!(
                    r#"{{"timestamp":"2026-06-06T10:00:03.000Z","type":"response_item","payload":{{"type":"function_call","name":"apply_patch","arguments":"{{\"input\":\"{patch}\"}}"}}}}"#
                ),
            ],
        );

        let source = CodexSource::new(&dir);
        let scope = ProjectScope::project(Path::new("/home/nicola/progetti/demo"));
        let sessions = source.sessions(&scope).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "sess-abc");

        let read = source.read_incremental(&sessions[0], 0).unwrap();
        assert_eq!(read.turns.len(), 1);
        let turn = &read.turns[0];
        assert_eq!(turn.prompt.as_deref(), Some("fix the lexer"));
        assert_eq!(turn.assistant_text, "editing lexer");
        assert_eq!(
            turn.declared_paths(),
            vec!["src/abs.py".to_owned(), "src/lex.py".to_owned()]
        );
        assert_eq!(turn.provenance.adapter, "codex");

        // Out-of-scope project is excluded.
        let other = ProjectScope::project(Path::new("/home/nicola/progetti/other"));
        assert!(source.sessions(&other).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
