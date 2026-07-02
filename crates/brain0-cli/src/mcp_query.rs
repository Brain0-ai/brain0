//! brain0's MCP **query** channel: exposes debug/audit — and the provenance
//! context tools — as MCP tools an external agent can call, answered by
//! reference (node ids + concise metadata; the payload text is never pushed through MCP).
//!
//! `brain0_context` and `brain0_blame` are the "why layer" a coding agent queries before
//! touching code: who/what changed an artifact, with which intent and model, with what drift.

use brain0_mcp::{ToolDef, ToolOutcome, ToolProvider};
use brain0_model::{Edge, EdgeKind, Level};
use brain0_models::EmbeddingProvider;
use brain0_storage::Storage;
use serde_json::{json, Value};
use std::str::FromStr as _;

use crate::query;

/// MCP tool provider backed by the brain0 index.
pub struct QueryTools<'a> {
    storage: &'a dyn Storage,
    embedder: &'a dyn EmbeddingProvider,
    default_repo: Option<String>,
}

impl<'a> QueryTools<'a> {
    pub fn new(
        storage: &'a dyn Storage,
        embedder: &'a dyn EmbeddingProvider,
        default_repo: Option<String>,
    ) -> Self {
        Self {
            storage,
            embedder,
            default_repo,
        }
    }
}

impl QueryTools<'_> {
    /// Resolve the repo: explicit argument → configured default → the index's only repo.
    fn resolve_repo(&self, arguments: &Value) -> Result<String, String> {
        if let Some(r) = arguments.get("repo").and_then(Value::as_str) {
            return Ok(r.to_owned());
        }
        if let Some(r) = &self.default_repo {
            return Ok(r.clone());
        }
        match self.storage.repos().map_err(|e| e.to_string())?.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err("no repo in the index".into()),
            many => Err(format!(
                "several repos — pass repo, one of: {}",
                many.join(", ")
            )),
        }
    }

    /// The provenance context of a file (`src/x.rs`) or symbol (`src/x.rs::f`), by reference:
    /// risk, recent version history, and the agent intents (agent/model/when/drift) behind it.
    fn artifact_context(&self, repo: &str, path: &str) -> Result<Value, String> {
        let level = if path.contains("::") {
            Level::Symbol
        } else {
            Level::File
        };
        let artifact = self
            .storage
            .find_artifact_by_path(repo, level, path)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no {level:?} artifact at {path:?} in repo {repo:?}"))?;

        let versions: Vec<Value> = self
            .storage
            .artifact_versions(&artifact.id)
            .map_err(|e| e.to_string())?
            .iter()
            .rev()
            .take(5)
            .map(|v| {
                json!({
                    "when": v.timestamp.to_rfc3339(),
                    "committer": v.author.name,
                    "changeKind": format!("{:?}", v.change_kind).to_lowercase(),
                    "linesAdded": v.lines_added,
                    "linesRemoved": v.lines_removed,
                    "source": v.source.ref_str().chars().take(10).collect::<String>(),
                })
            })
            .collect();

        let mut intents = Vec::new();
        for edge in self
            .storage
            .in_edges(EdgeKind::TaskModifiesArtifact, artifact.id.as_str())
            .map_err(|e| e.to_string())?
        {
            let Edge::TaskModifiesArtifact { task, .. } = edge else {
                continue;
            };
            let Some(node) = self
                .storage
                .get_task_node(&task)
                .map_err(|e| e.to_string())?
            else {
                continue;
            };
            if node.source_adapter.is_none() {
                continue; // commits are in `versions` already; intents = the agent side
            }
            let drift = self
                .storage
                .task_versions(&task)
                .map_err(|e| e.to_string())?
                .iter()
                .filter_map(|v| v.drift.as_ref().map(|d| d.score))
                .fold(0f32, f32::max);
            intents.push(json!({
                "taskId": task.as_str(),
                "agent": node.agent.name,
                "model": node.model,
                "when": node.created_at.to_rfc3339(),
                "maxDriftScore": drift,
            }));
        }

        Ok(json!({
            "path": artifact.qualified_path,
            "level": format!("{level:?}").to_lowercase(),
            "risk": {"fused": artifact.risk.fused(), "goldSignal": artifact.risk.is_gold_signal()},
            "versions": versions,
            "intents": intents,
        }))
    }
}

/// The narrowest symbol containing `line` in `source` (parsed as `rel_path`), if any.
pub fn symbol_at_line(rel_path: &str, source: &str, line: usize) -> Option<String> {
    let parsed = brain0_parser::parse_source(rel_path, source).ok()?;
    parsed
        .symbols
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
        .map(|s| s.qualified_path.clone())
}

impl ToolProvider for QueryTools<'_> {
    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "brain0_debug".into(),
                description: "Root-cause debug: find the intents/code most likely related to an issue, by reference.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {"type": "string"},
                        "k": {"type": "integer", "description": "max nodes to return (default 8)"}
                    }
                }),
            },
            ToolDef {
                name: "brain0_audit".into(),
                description: "Audit a repo: risk distribution, gold signals, and the riskiest nodes.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "repo": {"type": "string"},
                        "level": {"type": "string", "enum": ["repo", "module", "file", "symbol"]}
                    }
                }),
            },
            ToolDef {
                name: "brain0_context".into(),
                description: "Provenance context for a file or symbol BEFORE touching it: risk, recent history, and the agent intents (agent/model/when/drift) behind it. Path is repo-relative; symbols as `path::Symbol`.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string"},
                        "repo": {"type": "string"}
                    }
                }),
            },
            ToolDef {
                name: "brain0_blame".into(),
                description: "Which intent wrote this code: resolve a file (and optionally a 1-based line, mapped to its symbol) to the agent intents and history behind it.".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["file"],
                    "properties": {
                        "file": {"type": "string"},
                        "line": {"type": "integer"},
                        "repo": {"type": "string"}
                    }
                }),
            },
        ]
    }

    fn call(&self, name: &str, arguments: &Value) -> ToolOutcome {
        match name {
            "brain0_context" => {
                let Some(path) = arguments.get("path").and_then(Value::as_str) else {
                    return ToolOutcome::error("path is required");
                };
                let repo = match self.resolve_repo(arguments) {
                    Ok(r) => r,
                    Err(e) => return ToolOutcome::error(e),
                };
                match self.artifact_context(&repo, path) {
                    Ok(v) => ToolOutcome::ok(v.to_string()),
                    Err(e) => ToolOutcome::error(e),
                }
            }
            "brain0_blame" => {
                let Some(file) = arguments.get("file").and_then(Value::as_str) else {
                    return ToolOutcome::error("file is required");
                };
                let repo = match self.resolve_repo(arguments) {
                    Ok(r) => r,
                    Err(e) => return ToolOutcome::error(e),
                };
                // With a line, narrow to the containing symbol by parsing the CURRENT file from
                // the working tree (the server runs from the repo root). Fall back to file level.
                let mut target = file.to_owned();
                let mut note = None;
                if let Some(line) = arguments.get("line").and_then(Value::as_u64) {
                    match std::fs::read_to_string(file) {
                        Ok(src) => match symbol_at_line(file, &src, line as usize) {
                            Some(sym) => target = sym,
                            None => note = Some("no symbol spans that line — file-level context"),
                        },
                        Err(_) => note = Some("file unreadable from cwd — file-level context"),
                    }
                }
                match self.artifact_context(&repo, &target) {
                    Ok(mut v) => {
                        if let (Some(n), Some(obj)) = (note, v.as_object_mut()) {
                            obj.insert("note".into(), json!(n));
                        }
                        ToolOutcome::ok(v.to_string())
                    }
                    Err(e) => ToolOutcome::error(e),
                }
            }
            "brain0_debug" => {
                let query = arguments.get("query").and_then(Value::as_str).unwrap_or("");
                let k = arguments
                    .get("k")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(8);
                match query::debug(self.storage, self.embedder, query, k, &chrono::Utc::now()) {
                    Ok(r) => ToolOutcome::ok(
                        json!({
                            "tasks": r.tasks,
                            "artifacts": r.artifacts,
                            "artifactsTotal": r.artifacts_total,
                            "versions": r.versions,
                            "explanation": r.explanation
                        })
                        .to_string(),
                    ),
                    Err(e) => ToolOutcome::error(e.to_string()),
                }
            }
            "brain0_audit" => {
                let repo = arguments
                    .get("repo")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.default_repo.clone());
                let Some(repo) = repo else {
                    return ToolOutcome::error("no repo given and no default configured");
                };
                let level = arguments
                    .get("level")
                    .and_then(Value::as_str)
                    .and_then(|s| Level::from_str(s).ok())
                    .unwrap_or(Level::Symbol);
                match query::audit(self.storage, &repo, level, 10) {
                    Ok(r) => ToolOutcome::ok(
                        json!({
                            "repo": r.repo,
                            "level": r.level,
                            "distribution": {"green": r.green, "yellow": r.yellow, "red": r.red},
                            "goldSignals": r.gold_signals,
                            "topRisky": r.top_risky,
                            "explanation": r.explanation
                        })
                        .to_string(),
                    ),
                    Err(e) => ToolOutcome::error(e.to_string()),
                }
            }
            other => ToolOutcome::error(format!("unknown tool: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::{
        Agent, ArtifactId, ArtifactNode, Author, ChangeKind, ChangeSource, RiskState, SessionId,
        TaskId, TaskNode, TaskVersion, VersionId,
    };
    use brain0_models::LocalEmbeddingProvider;
    use brain0_storage::SqliteStorage;
    use chrono::TimeZone;

    #[test]
    fn symbol_at_line_finds_the_narrowest_symbol() {
        let src = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        assert_eq!(
            symbol_at_line("m.py", src, 3).as_deref(),
            Some("m.py::outer.inner")
        );
        assert_eq!(
            symbol_at_line("m.py", src, 4).as_deref(),
            Some("m.py::outer")
        );
        assert_eq!(symbol_at_line("m.py", src, 99), None);
    }

    #[test]
    fn context_tool_returns_intents_and_history_by_reference() {
        let ts = brain0_model::chrono::Utc
            .with_ymd_and_hms(2026, 7, 1, 10, 0, 0)
            .unwrap();
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_artifact_node(&ArtifactNode {
            id: ArtifactId::new("art_f"),
            level: Level::File,
            repo: "r".into(),
            qualified_path: "src/m.py".into(),
            lang: None,
            parent_id: None,
            current_version: VersionId::new("v1"),
            risk: RiskState {
                apriori: 0.2,
                aposteriori: 0.0,
            },
        })
        .unwrap();
        s.append_artifact_version(&brain0_model::ArtifactVersion {
            id: VersionId::new("v1"),
            artifact_id: ArtifactId::new("art_f"),
            timestamp: ts,
            author: Author::new("Nicola"),
            agent: Agent::human(),
            source: ChangeSource::Git {
                commit_sha: "abcdef123456".into(),
            },
            qualified_path: "src/m.py".into(),
            fingerprint: "fp".into(),
            change_kind: ChangeKind::Modified,
            lines_added: 3,
            lines_removed: 1,
            diff_ref: None,
        })
        .unwrap();
        let tid = TaskId::new("tsk_a");
        s.put_task_node(&TaskNode {
            id: tid.clone(),
            session_id: SessionId::new("s"),
            agent: Agent::new("claude-code"),
            author: Author::new("agent"),
            created_at: ts,
            current_version: VersionId::new("tv"),
            source_adapter: Some("claude-code".into()),
            session_cwd: None,
            model: Some("claude-fable-5".into()),
            reviewers: Vec::new(),
        })
        .unwrap();
        s.append_task_version(&TaskVersion {
            id: VersionId::new("tv"),
            task_id: tid.clone(),
            timestamp: ts,
            prompt_ref: None,
            decision_summary_ref: None,
            declared: Vec::new(),
            drift: Some(brain0_model::Drift::new(
                0.4,
                vec!["src/other.py".into()],
                vec![],
            )),
            reads: Vec::new(),
            read_secrets: Vec::new(),
        })
        .unwrap();
        s.put_edge(&Edge::TaskModifiesArtifact {
            task: tid,
            artifact: ArtifactId::new("art_f"),
            version: VersionId::new("v1"),
            change_kind: ChangeKind::Modified,
            lines_added: 3,
            lines_removed: 1,
        })
        .unwrap();

        let embedder = LocalEmbeddingProvider::new(8);
        let tools = QueryTools::new(&s, &embedder, Some("r".into()));
        let out = tools.call("brain0_context", &json!({"path": "src/m.py"}));
        assert!(!out.is_error, "{:?}", out.text);
        let v: Value = serde_json::from_str(&out.text).unwrap();
        assert_eq!(v["level"], "file");
        assert_eq!(v["intents"][0]["agent"], "claude-code");
        assert_eq!(v["intents"][0]["model"], "claude-fable-5");
        assert!(v["intents"][0]["maxDriftScore"].as_f64().unwrap() > 0.3);
        assert_eq!(v["versions"][0]["committer"], "Nicola");
        // Blame without a line resolves the same file-level context.
        let blame = tools.call("brain0_blame", &json!({"file": "src/m.py"}));
        assert!(!blame.is_error);
    }
}
