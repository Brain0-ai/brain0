//! RAG-on-graph query for the MCP query channel: the same logic as the
//! internal agent, offered to an external agent. It operates **by reference** — it returns
//! the relevant node ids plus a concise textual summary, never a dump of the dataset, and
//! makes no LLM call (the calling agent does the reasoning).

use brain0_model::{Edge, EdgeKind, Level, Timestamp};
use brain0_models::EmbeddingProvider;
use brain0_storage::Storage;

use anyhow::Result;

const HALF_LIFE_DAYS: f64 = 30.0;
const W_SEMANTIC: f32 = 0.6;
const W_RECENCY: f32 = 0.3;
const W_RISK: f32 = 0.1;

/// Root-cause debug result — references plus an explanation. Reference lists are CAPPED
/// (`MAX_REF_ARTIFACTS`/`MAX_REF_VERSIONS`): a gap-filled commit can link hundreds of
/// artifacts, and an MCP consumer pays tokens for every id. `artifacts_total` reports the
/// uncapped count so truncation is never silent.
#[derive(Debug, Clone)]
pub struct DebugResult {
    pub tasks: Vec<String>,
    /// Top artifacts by fused risk (capped).
    pub artifacts: Vec<String>,
    /// Uncapped number of distinct artifacts the returned tasks touched.
    pub artifacts_total: usize,
    /// Most recent version ids of the top artifacts (capped).
    pub versions: Vec<String>,
    pub explanation: String,
}

/// Caps for the by-reference lists in [`DebugResult`] (token budget for MCP consumers).
const MAX_REF_ARTIFACTS: usize = 20;
const MAX_REF_VERSIONS: usize = 20;
/// Per-task artifact lines shown in the explanation (top by risk, then "+N more").
const MAX_TOUCHED_LINES: usize = 8;
/// Recent versions kept per top artifact.
const VERSIONS_PER_ARTIFACT: usize = 3;

/// Audit result — risk distribution and the riskiest nodes, by reference.
#[derive(Debug, Clone)]
pub struct AuditResult {
    pub repo: String,
    pub level: String,
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
    pub gold_signals: Vec<String>,
    pub top_risky: Vec<(String, String, f32)>,
    pub explanation: String,
}

/// Max fused risk across the artifacts a task modified.
fn task_risk(storage: &dyn Storage, task_id: &str) -> Result<f32> {
    let mut max = 0.0f32;
    for edge in storage.out_edges(EdgeKind::TaskModifiesArtifact, task_id)? {
        if let Edge::TaskModifiesArtifact { artifact, .. } = edge {
            if let Some(node) = storage.get_artifact_node(&artifact)? {
                max = max.max(node.risk.fused());
            }
        }
    }
    Ok(max)
}

fn recency(created_at: &Timestamp, now: &Timestamp) -> f32 {
    let age_days = ((*now - *created_at).num_seconds().max(0) as f64) / 86_400.0;
    0.5f64.powf(age_days / HALF_LIFE_DAYS) as f32
}

/// Recency-aware, risk-weighted root-cause debug. Hydration is bounded to `k` nodes. Uses
/// the configured embedder for the query vector (same model/space as ingest —).
pub fn debug(
    storage: &dyn Storage,
    embedder: &dyn EmbeddingProvider,
    query: &str,
    k: usize,
    now: &Timestamp,
) -> Result<DebugResult> {
    let qvec = embedder.embed(query)?;
    let pool = storage.search_tasks_by_vector(&qvec, (k * 3).max(k))?;

    let mut scored: Vec<(String, f32)> = Vec::new();
    for hit in &pool {
        let risk = task_risk(storage, hit.task_id.as_str())?;
        let rec = recency(&hit.created_at, now);
        let score = W_SEMANTIC * hit.cosine + W_RECENCY * rec + W_RISK * risk;
        scored.push((hit.task_id.as_str().to_owned(), score));
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);

    // Collect every touched artifact once (id → fused risk), and per task the top lines.
    let mut artifact_risk: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();
    let mut lines = Vec::new();
    for (task_id, score) in &scored {
        let mut touched: Vec<(String, f32)> = Vec::new();
        for edge in storage.out_edges(EdgeKind::TaskModifiesArtifact, task_id)? {
            if let Edge::TaskModifiesArtifact { artifact, .. } = edge {
                let aid = artifact.as_str().to_owned();
                if let Some(node) = storage.get_artifact_node(&artifact)? {
                    let fused = node.risk.fused();
                    artifact_risk.entry(aid).or_insert(fused);
                    touched.push((node.qualified_path.clone(), fused));
                } else {
                    artifact_risk.entry(aid).or_insert(0.0);
                }
            }
        }
        // The explanation shows the riskiest few, never the whole gap-filled list.
        touched.sort_by(|a, b| b.1.total_cmp(&a.1));
        let extra = touched.len().saturating_sub(MAX_TOUCHED_LINES);
        let mut shown: Vec<String> = touched
            .iter()
            .take(MAX_TOUCHED_LINES)
            .map(|(path, fused)| format!("{path} (risk {fused:.2})"))
            .collect();
        if extra > 0 {
            shown.push(format!("… +{extra} more"));
        }
        lines.push(format!(
            "- task {task_id} (score {score:.2}) → {}",
            if shown.is_empty() {
                "(no linked code)".to_owned()
            } else {
                shown.join(", ")
            }
        ));
    }

    // References, by risk: the top artifacts, and the most recent versions of each.
    let artifacts_total = artifact_risk.len();
    let mut ranked: Vec<(String, f32)> = artifact_risk.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(MAX_REF_ARTIFACTS);
    let artifacts: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();

    let mut versions = Vec::new();
    'outer: for (aid, _) in &ranked {
        let chain = storage.artifact_versions(&brain0_model::ArtifactId::new(aid.clone()))?;
        for v in chain.iter().rev().take(VERSIONS_PER_ARTIFACT) {
            versions.push(v.id.as_str().to_owned());
            if versions.len() >= MAX_REF_VERSIONS {
                break 'outer;
            }
        }
    }

    let explanation = if lines.is_empty() {
        format!("No relevant intents found for: {query}")
    } else {
        format!(
            "Most likely related intents for \"{query}\" (most relevant first):\n{}",
            lines.join("\n")
        )
    };

    Ok(DebugResult {
        tasks: scored.into_iter().map(|(t, _)| t).collect(),
        artifacts,
        artifacts_total,
        versions,
        explanation,
    })
}

/// Big-picture audit over a repo: risk distribution + gold signals + riskiest nodes.
pub fn audit(storage: &dyn Storage, repo: &str, level: Level, k: usize) -> Result<AuditResult> {
    let artifacts = storage.list_artifacts(repo, level)?;
    let mut green = 0;
    let mut yellow = 0;
    let mut red = 0;
    let mut gold_signals = Vec::new();
    let mut scored: Vec<(String, String, f32)> = Vec::new();
    for node in &artifacts {
        let fused = node.risk.fused();
        if fused < 0.34 {
            green += 1;
        } else if fused < 0.66 {
            yellow += 1;
        } else {
            red += 1;
        }
        if node.risk.is_gold_signal() {
            gold_signals.push(node.id.as_str().to_owned());
        }
        scored.push((
            node.id.as_str().to_owned(),
            node.qualified_path.clone(),
            fused,
        ));
    }
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let top_risky: Vec<_> = scored.into_iter().take(k).collect();

    let explanation = format!(
        "Repo {repo}: {} {level:?}(s). Risk green={green} yellow={yellow} red={red}. Gold-signal (looked safe → proved dangerous): {}.",
        artifacts.len(),
        gold_signals.len()
    );

    Ok(AuditResult {
        repo: repo.to_owned(),
        level: format!("{level:?}").to_lowercase(),
        green,
        yellow,
        red,
        gold_signals,
        top_risky,
        explanation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::{
        Agent, ArtifactId, ArtifactNode, RiskState, SessionId, TaskId, TaskNode, VersionId,
    };
    use brain0_models::LocalEmbeddingProvider;
    use brain0_storage::{local_embed, SqliteStorage, LOCAL_EMBED_DIM};
    use chrono::TimeZone;

    fn now() -> Timestamp {
        chrono::Utc.with_ymd_and_hms(2026, 6, 7, 0, 0, 0).unwrap()
    }

    fn seed_task(store: &SqliteStorage, id: &str, created: Timestamp, text: &str) {
        store
            .put_task_node(&TaskNode {
                id: TaskId::new(id),
                session_id: SessionId::new("s"),
                agent: Agent::new("codex"),
                author: brain0_model::Author::new("agent"),
                created_at: created,
                current_version: VersionId::new("v"),
                source_adapter: Some("codex".into()),
                session_cwd: Some("/p".into()),
                model: None,
                reviewers: Vec::new(),
            })
            .unwrap();
        store
            .put_task_embedding(&TaskId::new(id), &local_embed(text, LOCAL_EMBED_DIM))
            .unwrap();
    }

    #[test]
    fn debug_surfaces_relevant_recent_task() {
        let store = SqliteStorage::open_in_memory().unwrap();
        seed_task(
            &store,
            "tsk_parser",
            now() - chrono::Duration::days(1),
            "rewrite the parser tokenizer",
        );
        seed_task(
            &store,
            "tsk_docs",
            now() - chrono::Duration::days(200),
            "update the readme docs",
        );

        let result = debug(
            &store,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            "parser bug",
            1,
            &now(),
        )
        .unwrap();
        assert_eq!(result.tasks, vec!["tsk_parser".to_owned()]);
        assert!(!result.explanation.is_empty());
    }

    #[test]
    fn audit_distribution_and_gold_signals() {
        let store = SqliteStorage::open_in_memory().unwrap();
        for (id, risk) in [
            ("art_a", RiskState::new(0.0, 0.0)),
            ("art_b", RiskState::new(0.5, 0.0)),
            ("art_c", RiskState::new(0.1, 0.9)),
        ] {
            store
                .put_artifact_node(&ArtifactNode {
                    id: ArtifactId::new(id),
                    level: Level::Symbol,
                    repo: "repo".into(),
                    qualified_path: format!("m.py::{id}"),
                    lang: None,
                    parent_id: None,
                    current_version: VersionId::new("v"),
                    risk,
                })
                .unwrap();
        }
        let result = audit(&store, "repo", Level::Symbol, 5).unwrap();
        assert_eq!((result.green, result.yellow, result.red), (1, 1, 1));
        assert_eq!(result.gold_signals, vec!["art_c".to_owned()]);
        assert_eq!(result.top_risky[0].0, "art_c");
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    use brain0_model::{
        Agent, ArtifactId, ArtifactNode, ArtifactVersion, Author, ChangeKind, ChangeSource,
        RiskState, SessionId, TaskId, TaskNode, VersionId,
    };
    use brain0_models::LocalEmbeddingProvider;
    use brain0_storage::{local_embed, SqliteStorage, Storage, LOCAL_EMBED_DIM};
    use chrono::TimeZone;

    /// A gap-filled commit can link hundreds of artifacts; the by-reference result must stay
    /// bounded (MCP token budget) while reporting the uncapped total.
    #[test]
    fn debug_caps_reference_lists_and_reports_totals() {
        let store = SqliteStorage::open_in_memory().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 7, 2, 0, 0, 0).unwrap();
        let tid = TaskId::new("tsk_big");
        store
            .put_task_node(&TaskNode {
                id: tid.clone(),
                session_id: SessionId::new("s"),
                agent: Agent::new("codex"),
                author: Author::new("agent"),
                created_at: now,
                current_version: VersionId::new("v"),
                source_adapter: Some("codex".into()),
                session_cwd: None,
                model: None,
                reviewers: Vec::new(),
            })
            .unwrap();
        store
            .put_task_embedding(
                &tid,
                &local_embed("huge refactor of everything", LOCAL_EMBED_DIM),
            )
            .unwrap();
        for i in 0..25 {
            let aid = ArtifactId::new(format!("art_{i:02}"));
            store
                .put_artifact_node(&ArtifactNode {
                    id: aid.clone(),
                    level: Level::File,
                    repo: "r".into(),
                    qualified_path: format!("src/f{i:02}.rs"),
                    lang: None,
                    parent_id: None,
                    current_version: VersionId::new(format!("v{i:02}")),
                    risk: RiskState::new(i as f32 / 25.0, 0.0),
                })
                .unwrap();
            store
                .append_artifact_version(&ArtifactVersion {
                    id: VersionId::new(format!("v{i:02}")),
                    artifact_id: aid.clone(),
                    timestamp: now,
                    author: Author::new("dev"),
                    agent: Agent::human(),
                    source: ChangeSource::Git {
                        commit_sha: "c".into(),
                    },
                    qualified_path: format!("src/f{i:02}.rs"),
                    fingerprint: String::new(),
                    change_kind: ChangeKind::Modified,
                    lines_added: 1,
                    lines_removed: 0,
                    diff_ref: None,
                })
                .unwrap();
            store
                .put_edge(&Edge::TaskModifiesArtifact {
                    task: tid.clone(),
                    artifact: aid,
                    version: VersionId::new(format!("v{i:02}")),
                    change_kind: ChangeKind::Modified,
                    lines_added: 1,
                    lines_removed: 0,
                })
                .unwrap();
        }

        let r = debug(
            &store,
            &LocalEmbeddingProvider::new(LOCAL_EMBED_DIM),
            "huge refactor",
            1,
            &now,
        )
        .unwrap();
        assert_eq!(r.artifacts_total, 25);
        assert!(
            r.artifacts.len() <= 20,
            "artifacts capped: {}",
            r.artifacts.len()
        );
        assert!(
            r.versions.len() <= 20,
            "versions capped: {}",
            r.versions.len()
        );
        // The riskiest artifact survives the cap; the least risky does not.
        assert!(r.artifacts.contains(&"art_24".to_owned()));
        assert!(!r.artifacts.contains(&"art_00".to_owned()));
        // The explanation shows the top lines plus an explicit "+N more".
        assert!(r.explanation.contains("+17 more"), "{}", r.explanation);
    }
}
