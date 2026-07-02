//! `brain0 report` — the accountability report for a repo.
//!
//! Leads with the numbers only brain0 can produce (they are the product's point):
//! ① declared-vs-done **drift** incidents, ② **sensitive reads** (sessions whose context
//! included secrets — kinds only, never values), ③ **top risk** artifacts incl. gold signals
//! (looked safe, later proved dangerous), and only last ④ the agent-vs-human footprint
//! (vendor dashboards already show bare percentages; the causal detail is ours).
//!
//! Pure collect/render over the abstract [`Storage`] — no I/O beyond the store, unit-tested.

use std::collections::HashSet;

use brain0_model::{Edge, EdgeKind, Level};
use brain0_storage::{Result, Storage};

/// One declared-vs-done drift incident on an agent task version.
pub struct DriftIncident {
    pub agent: String,
    pub when: String,
    pub score: f32,
    pub undeclared: Vec<String>,
    pub phantom: Vec<String>,
}

/// One read whose content held secrets (kinds only — the value is never stored anywhere).
pub struct SensitiveRead {
    pub agent: String,
    pub when: String,
    pub path: String,
    pub kinds: Vec<String>,
}

/// A file ranked by fused risk.
pub struct RiskyFile {
    pub path: String,
    pub fused: f32,
    pub gold: bool,
}

/// Everything the report prints.
pub struct ReportData {
    pub repo: String,
    pub agent_tasks: usize,
    pub commit_tasks: usize,
    pub drift: Vec<DriftIncident>,
    pub sensitive_reads: Vec<SensitiveRead>,
    pub out_of_repo_reads: usize,
    pub risky: Vec<RiskyFile>,
    pub gold_signals: usize,
    pub agent_touched_files: usize,
    pub total_files: usize,
    pub agent_lines_added: u64,
    pub agent_lines_removed: u64,
}

/// Collect the report for `repo`, keeping the `top` riskiest files.
pub fn collect(storage: &dyn Storage, repo: &str, top: usize) -> Result<ReportData> {
    // Classify tasks: an agent task carries its source adapter; a commit/observer task does not.
    let mut agent_ids: HashSet<String> = HashSet::new();
    let mut agent_tasks = 0usize;
    let mut commit_tasks = 0usize;
    let mut drift = Vec::new();
    let mut sensitive_reads = Vec::new();
    let mut out_of_repo_reads = 0usize;

    for task_id in storage.all_task_ids()? {
        let Some(task) = storage.get_task_node(&task_id)? else {
            continue;
        };
        if task.source_adapter.is_none() {
            commit_tasks += 1;
            continue;
        }
        agent_tasks += 1;
        agent_ids.insert(task_id.as_str().to_owned());
        // Drift is session-cumulative (declared-so-far vs observed-over-the-session), so the
        // LATEST version carries the session's verdict — one incident per session, not per turn.
        let mut verdict: Option<DriftIncident> = None;
        for v in storage.task_versions(&task_id)? {
            let when = v.timestamp.to_rfc3339();
            if let Some(d) = &v.drift {
                verdict =
                    (!d.undeclared.is_empty() || !d.phantom.is_empty()).then(|| DriftIncident {
                        agent: task.agent.name.clone(),
                        when: when.clone(),
                        score: d.score,
                        undeclared: d.undeclared.clone(),
                        phantom: d.phantom.clone(),
                    });
            }
            out_of_repo_reads += v.reads.iter().filter(|p| p.starts_with('/')).count();
            for rs in &v.read_secrets {
                sensitive_reads.push(SensitiveRead {
                    agent: task.agent.name.clone(),
                    when: when.clone(),
                    path: rs.path.clone(),
                    kinds: rs.kinds.clone(),
                });
            }
        }
        drift.extend(verdict);
    }
    drift.sort_by(|a, b| b.score.total_cmp(&a.score));

    // Files: risk ranking + gold signals + the agent footprint via intent edges.
    let files = storage.list_artifacts(repo, Level::File)?;
    let total_files = files.len();
    let mut gold_signals = 0usize;
    let mut risky: Vec<RiskyFile> = Vec::new();
    let mut agent_touched_files = 0usize;
    let mut agent_lines_added = 0u64;
    let mut agent_lines_removed = 0u64;

    for f in &files {
        let gold = f.risk.is_gold_signal();
        if gold {
            gold_signals += 1;
        }
        risky.push(RiskyFile {
            path: f.qualified_path.clone(),
            fused: f.risk.fused(),
            gold,
        });

        let mut touched = false;
        for edge in storage.in_edges(EdgeKind::TaskModifiesArtifact, f.id.as_str())? {
            if let Edge::TaskModifiesArtifact {
                task,
                lines_added,
                lines_removed,
                ..
            } = edge
            {
                if agent_ids.contains(task.as_str()) {
                    touched = true;
                    agent_lines_added += u64::from(lines_added);
                    agent_lines_removed += u64::from(lines_removed);
                }
            }
        }
        if touched {
            agent_touched_files += 1;
        }
    }
    risky.sort_by(|a, b| b.fused.total_cmp(&a.fused));
    risky.truncate(top);
    risky.retain(|r| r.fused > 0.0);

    Ok(ReportData {
        repo: repo.to_owned(),
        agent_tasks,
        commit_tasks,
        drift,
        sensitive_reads,
        out_of_repo_reads,
        risky,
        gold_signals,
        agent_touched_files,
        total_files,
        agent_lines_added,
        agent_lines_removed,
    })
}

/// First `keep` paths joined with a `+N more` tail — incident lists can span 100+ files.
fn short_list(paths: &[String], keep: usize) -> String {
    if paths.len() <= keep {
        return paths.join(", ");
    }
    format!(
        "{} … +{} more",
        paths[..keep].join(", "),
        paths.len() - keep
    )
}

/// Render the report as terminal text or markdown.
#[must_use]
pub fn render(d: &ReportData, md: bool) -> String {
    let mut s = String::new();
    let h = |s: &mut String, title: &str| {
        if md {
            s.push_str(&format!("\n## {title}\n\n"));
        } else {
            s.push_str(&format!(
                "\n{title}\n{}\n",
                "─".repeat(title.chars().count())
            ));
        }
    };
    let li = |s: &mut String, line: &str| {
        s.push_str(if md { "- " } else { "  · " });
        s.push_str(line);
        s.push('\n');
    };

    if md {
        s.push_str(&format!("# brain0 report — {}\n", d.repo));
    } else {
        s.push_str(&format!("brain0 report — {}\n", d.repo));
    }
    s.push_str(&format!(
        "{} agent session(s) · {} commit(s) observed\n",
        d.agent_tasks, d.commit_tasks
    ));

    h(
        &mut s,
        &format!("drift — declared vs done ({})", d.drift.len()),
    );
    if d.drift.is_empty() {
        li(
            &mut s,
            "none: every agent declaration matched the observed changes",
        );
    }
    for i in d.drift.iter().take(8) {
        let mut parts = Vec::new();
        if !i.undeclared.is_empty() {
            parts.push(format!(
                "changed but not declared: {}",
                short_list(&i.undeclared, 4)
            ));
        }
        if !i.phantom.is_empty() {
            parts.push(format!(
                "declared but not observed: {}",
                short_list(&i.phantom, 4)
            ));
        }
        li(
            &mut s,
            &format!(
                "[{:.2}] {} ({}) — {}",
                i.score,
                i.agent,
                i.when,
                parts.join(" · ")
            ),
        );
    }

    h(
        &mut s,
        &format!("sensitive reads — DLP ({})", d.sensitive_reads.len()),
    );
    if d.sensitive_reads.is_empty() {
        li(&mut s, "none: no agent session read secret-bearing content");
    }
    for r in d.sensitive_reads.iter().take(8) {
        li(
            &mut s,
            &format!(
                "{} read {} [{}] ({})",
                r.agent,
                r.path,
                r.kinds.join(", "),
                r.when
            ),
        );
    }
    if d.out_of_repo_reads > 0 {
        li(
            &mut s,
            &format!(
                "{} read(s) outside the repo reached the model's context",
                d.out_of_repo_reads
            ),
        );
    }

    h(
        &mut s,
        &format!("top risk ({} gold signal(s))", d.gold_signals),
    );
    if d.risky.is_empty() {
        li(
            &mut s,
            "all green: no file carries meaningful risk right now",
        );
    }
    for r in &d.risky {
        li(
            &mut s,
            &format!(
                "{:.2} {}{}",
                r.fused,
                r.path,
                if r.gold { "  ⚠ gold signal" } else { "" }
            ),
        );
    }

    h(&mut s, "agent footprint");
    li(
        &mut s,
        &format!(
            "{}/{} files touched by agent intents (+{} / -{} lines linked to agents)",
            d.agent_touched_files, d.total_files, d.agent_lines_added, d.agent_lines_removed
        ),
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::{
        Agent, ArtifactId, ArtifactNode, Author, ChangeKind, DeclaredChange, Drift, ReadSecret,
        RiskState, SessionId, TaskId, TaskNode, TaskVersion, VersionId,
    };
    use brain0_storage::SqliteStorage;
    use chrono::TimeZone;

    fn ts() -> brain0_model::Timestamp {
        brain0_model::chrono::Utc
            .with_ymd_and_hms(2026, 7, 1, 10, 0, 0)
            .unwrap()
    }

    fn seed() -> SqliteStorage {
        let s = SqliteStorage::open_in_memory().unwrap();
        // One risky file with a gold-signal profile (safe a-priori, dangerous a-posteriori).
        s.put_artifact_node(&ArtifactNode {
            id: ArtifactId::new("art_f"),
            level: Level::File,
            repo: "r".into(),
            qualified_path: "src/hot.rs".into(),
            lang: None,
            parent_id: None,
            current_version: VersionId::new("v1"),
            risk: RiskState {
                apriori: 0.1,
                aposteriori: 0.9,
            },
        })
        .unwrap();
        // An agent task with drift + a secret-bearing read + an out-of-repo read.
        let tid = TaskId::new("tsk_agent");
        s.put_task_node(&TaskNode {
            id: tid.clone(),
            session_id: SessionId::new("s1"),
            agent: Agent::new("claude-code"),
            author: Author::new("agent"),
            created_at: ts(),
            current_version: VersionId::new("tv1"),
            source_adapter: Some("claude-code".into()),
            session_cwd: None,
            model: Some("claude-fable-5".into()),
            reviewers: Vec::new(),
        })
        .unwrap();
        s.append_task_version(&TaskVersion {
            id: VersionId::new("tv1"),
            task_id: tid.clone(),
            timestamp: ts(),
            prompt_ref: None,
            decision_summary_ref: None,
            declared: vec![DeclaredChange::new("src/hot.rs")],
            drift: Some(Drift::new(0.5, vec!["src/other.rs".into()], vec![])),
            reads: vec!["/home/dev/.env".into(), "src/hot.rs".into()],
            read_secrets: vec![ReadSecret {
                path: "/home/dev/.env".into(),
                kinds: vec!["env_assignment".into()],
            }],
        })
        .unwrap();
        // A commit/observer task (no adapter) — counted, not drifted.
        let cid = TaskId::new("tsk_commit");
        s.put_task_node(&TaskNode {
            id: cid.clone(),
            session_id: SessionId::new("abc123"),
            agent: Agent::human(),
            author: Author::new("Nicola"),
            created_at: ts(),
            current_version: VersionId::new("cv1"),
            source_adapter: None,
            session_cwd: None,
            model: None,
            reviewers: Vec::new(),
        })
        .unwrap();
        // The agent intent modified the file (+10/-2).
        s.put_edge(&Edge::TaskModifiesArtifact {
            task: tid,
            artifact: ArtifactId::new("art_f"),
            version: VersionId::new("v1"),
            change_kind: ChangeKind::Modified,
            lines_added: 10,
            lines_removed: 2,
        })
        .unwrap();
        s
    }

    #[test]
    fn collect_reports_drift_reads_risk_and_footprint() {
        let s = seed();
        let d = collect(&s, "r", 10).unwrap();
        assert_eq!((d.agent_tasks, d.commit_tasks), (1, 1));
        assert_eq!(d.drift.len(), 1);
        assert_eq!(d.drift[0].undeclared, vec!["src/other.rs".to_string()]);
        assert_eq!(d.sensitive_reads.len(), 1);
        assert_eq!(
            d.sensitive_reads[0].kinds,
            vec!["env_assignment".to_string()]
        );
        assert_eq!(d.out_of_repo_reads, 1);
        assert_eq!(d.gold_signals, 1);
        assert_eq!(d.risky.len(), 1);
        assert!(d.risky[0].gold && d.risky[0].fused > 0.9);
        assert_eq!((d.agent_touched_files, d.total_files), (1, 1));
        assert_eq!((d.agent_lines_added, d.agent_lines_removed), (10, 2));
    }

    #[test]
    fn render_has_all_sections_in_both_modes() {
        let s = seed();
        let d = collect(&s, "r", 10).unwrap();
        for md in [false, true] {
            let out = render(&d, md);
            for needle in [
                "drift — declared vs done (1)",
                "sensitive reads — DLP (1)",
                "env_assignment",
                "gold signal",
                "agent footprint",
                "1/1 files",
            ] {
                assert!(out.contains(needle), "missing {needle:?} in md={md}: {out}");
            }
            // The value of the secret is never present anywhere — only the kind and path.
            assert!(!out.to_lowercase().contains("password"));
        }
    }

    #[test]
    fn sqlite_repos_lists_distinct_repos() {
        let s = seed();
        assert_eq!(s.repos().unwrap(), vec!["r".to_string()]);
    }
}
