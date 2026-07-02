//! `brain0 today` — the morning triage: what agents and humans did in the
//! last window (default 24h), ordered by what needs attention first — sensitive reads and
//! drift above everything, then commits, then clean sessions. Metadata only (no payload
//! hydration): fast, and safe to run anywhere.

use brain0_model::Timestamp;
use brain0_storage::{Result, Storage};

/// One agent session's activity inside the window.
pub struct SessionDigest {
    pub agent: String,
    pub model: Option<String>,
    pub turns: usize,
    pub last: String,
    pub drift_max: f32,
    pub drift_paths: Vec<String>,
    pub secret_reads: Vec<String>, // "path [kind, kind]"
}

/// One observed commit inside the window.
pub struct CommitDigest {
    pub author: String,
    pub reference: String,
    pub when: String,
}

/// The triage: attention-worthy sessions first, then commits, then clean sessions.
pub struct TodayData {
    pub hours: i64,
    pub attention: Vec<SessionDigest>,
    pub quiet: Vec<SessionDigest>,
    pub commits: Vec<CommitDigest>,
}

/// Parse a `--since` duration: `24h`, `7d`, `90m` (defaults unit = hours).
pub fn parse_since(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, _unit) = s.split_at(s.len() - s.chars().last().map_or(0, |c| c.len_utf8()));
    match s.chars().last()? {
        'h' => Some(chrono::Duration::hours(num.parse().ok()?)),
        'd' => Some(chrono::Duration::days(num.parse().ok()?)),
        'm' => Some(chrono::Duration::minutes(num.parse().ok()?)),
        _ => Some(chrono::Duration::hours(s.parse().ok()?)),
    }
    .filter(|d| *d > chrono::Duration::zero())
}

/// Collect everything that happened after `cutoff`.
pub fn collect(storage: &dyn Storage, cutoff: Timestamp, now: Timestamp) -> Result<TodayData> {
    let mut attention = Vec::new();
    let mut quiet = Vec::new();
    let mut commits = Vec::new();

    for task_id in storage.all_task_ids()? {
        let Some(task) = storage.get_task_node(&task_id)? else {
            continue;
        };
        let versions: Vec<_> = storage
            .task_versions(&task_id)?
            .into_iter()
            .filter(|v| v.timestamp >= cutoff)
            .collect();
        if versions.is_empty() {
            continue;
        }

        if task.source_adapter.is_none() {
            commits.push(CommitDigest {
                author: task.author.name.clone(),
                reference: task.session_id.as_str().chars().take(10).collect(),
                when: task.created_at.to_rfc3339(),
            });
            continue;
        }

        // Drift is session-cumulative: the LATEST drifted version is the verdict so far.
        let mut drift_max = 0f32;
        let mut drift_paths = Vec::new();
        let mut secret_reads = Vec::new();
        let mut last = cutoff;
        for v in &versions {
            if v.timestamp > last {
                last = v.timestamp;
            }
            if let Some(d) = &v.drift {
                drift_max = d.score;
                drift_paths = d.undeclared.clone();
            }
            for rs in &v.read_secrets {
                secret_reads.push(format!("{} [{}]", rs.path, rs.kinds.join(", ")));
            }
        }
        let digest = SessionDigest {
            agent: task.agent.name.clone(),
            model: task.model.clone(),
            turns: versions.len(),
            last: last.to_rfc3339(),
            drift_max,
            drift_paths,
            secret_reads,
        };
        if digest.drift_max > 0.0 || !digest.secret_reads.is_empty() {
            attention.push(digest);
        } else {
            quiet.push(digest);
        }
    }

    // Most alarming first: secret reads outrank drift; higher drift outranks lower.
    attention.sort_by(|a, b| {
        (b.secret_reads.len(), b.drift_max.to_bits())
            .cmp(&(a.secret_reads.len(), a.drift_max.to_bits()))
    });
    // The same commit can be indexed under several repo namespaces (deterministic per-repo task
    // ids); it is still one event — dedupe by (sha, when).
    commits.sort_by(|a, b| b.when.cmp(&a.when));
    commits.dedup_by(|a, b| a.reference == b.reference && a.when == b.when);

    let hours = (now - cutoff).num_hours().max(1);
    Ok(TodayData {
        hours,
        attention,
        quiet,
        commits,
    })
}

/// Render the triage for the terminal.
#[must_use]
pub fn render(d: &TodayData) -> String {
    let mut s = format!(
        "brain0 today — last {}h · {} session(s) need attention · {} commit(s) · {} quiet\n",
        d.hours,
        d.attention.len(),
        d.commits.len(),
        d.quiet.len()
    );
    let session_line = |x: &SessionDigest| {
        let model = x
            .model
            .as_deref()
            .map(|m| format!(" · {m}"))
            .unwrap_or_default();
        format!(
            "{}{} — {} turn(s), last {}",
            x.agent, model, x.turns, x.last
        )
    };

    if !d.attention.is_empty() {
        s.push_str("\nneeds attention\n");
        for x in &d.attention {
            s.push_str(&format!("  · {}\n", session_line(x)));
            for r in &x.secret_reads {
                s.push_str(&format!("      ⚠ read secrets: {r}\n"));
            }
            if x.drift_max > 0.0 {
                let sample = x
                    .drift_paths
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = x.drift_paths.len().saturating_sub(4);
                s.push_str(&format!(
                    "      drift {:.2}: {}{}\n",
                    x.drift_max,
                    sample,
                    if more > 0 {
                        format!(" … +{more} more")
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }
    if !d.commits.is_empty() {
        s.push_str("\ncommits\n");
        for c in &d.commits {
            s.push_str(&format!("  · {} {} ({})\n", c.author, c.reference, c.when));
        }
    }
    if !d.quiet.is_empty() {
        s.push_str("\nquiet (no drift, no sensitive reads)\n");
        for x in &d.quiet {
            s.push_str(&format!("  · {}\n", session_line(x)));
        }
    }
    if d.attention.is_empty() && d.commits.is_empty() && d.quiet.is_empty() {
        s.push_str("\n  nothing happened in this window\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::{
        Agent, Author, Drift, ReadSecret, SessionId, TaskId, TaskNode, TaskVersion, VersionId,
    };
    use brain0_storage::SqliteStorage;
    use chrono::TimeZone;

    fn at(h: u32) -> Timestamp {
        brain0_model::chrono::Utc
            .with_ymd_and_hms(2026, 7, 2, h, 0, 0)
            .unwrap()
    }

    fn seed_task(
        s: &SqliteStorage,
        id: &str,
        adapter: Option<&str>,
        when: Timestamp,
        drift: Option<Drift>,
        secrets: Vec<ReadSecret>,
    ) {
        let tid = TaskId::new(id);
        s.put_task_node(&TaskNode {
            id: tid.clone(),
            session_id: SessionId::new(format!("{id}-sess")),
            agent: Agent::new(if adapter.is_some() {
                "claude-code"
            } else {
                "human"
            }),
            author: Author::new("Nicola"),
            created_at: when,
            current_version: VersionId::new(format!("{id}-v")),
            source_adapter: adapter.map(str::to_owned),
            session_cwd: None,
            model: adapter.map(|_| "claude-fable-5".into()),
            reviewers: Vec::new(),
        })
        .unwrap();
        s.append_task_version(&TaskVersion {
            id: VersionId::new(format!("{id}-v")),
            task_id: tid,
            timestamp: when,
            prompt_ref: None,
            decision_summary_ref: None,
            declared: Vec::new(),
            drift,
            reads: Vec::new(),
            read_secrets: secrets,
        })
        .unwrap();
    }

    #[test]
    fn collect_windows_and_ranks_attention_first() {
        let s = SqliteStorage::open_in_memory().unwrap();
        // In-window: one drifted session, one clean, one commit. Out-of-window: an old session.
        seed_task(
            &s,
            "tsk_drift",
            Some("claude-code"),
            at(9),
            Some(Drift::new(0.7, vec!["src/x.rs".into()], vec![])),
            vec![ReadSecret {
                path: ".env".into(),
                kinds: vec!["env_assignment".into()],
            }],
        );
        seed_task(&s, "tsk_clean", Some("claude-code"), at(8), None, vec![]);
        seed_task(&s, "tsk_commit", None, at(7), None, vec![]);
        seed_task(&s, "tsk_old", Some("claude-code"), at(0), None, vec![]);

        let d = collect(&s, at(6), at(10)).unwrap();
        assert_eq!(d.hours, 4);
        assert_eq!(d.attention.len(), 1);
        assert_eq!(d.quiet.len(), 1); // tsk_old excluded by the window
        assert_eq!(d.commits.len(), 1);
        assert_eq!(d.attention[0].secret_reads.len(), 1);
        assert!(d.attention[0].drift_max > 0.6);

        let out = render(&d);
        for needle in [
            "needs attention",
            "read secrets: .env",
            "drift 0.70",
            "commits",
            "quiet",
        ] {
            assert!(out.contains(needle), "missing {needle:?}:\n{out}");
        }
    }

    #[test]
    fn parse_since_understands_h_d_m() {
        assert_eq!(parse_since("24h"), Some(chrono::Duration::hours(24)));
        assert_eq!(parse_since("7d"), Some(chrono::Duration::days(7)));
        assert_eq!(parse_since("90m"), Some(chrono::Duration::minutes(90)));
        assert_eq!(parse_since("36"), Some(chrono::Duration::hours(36)));
        assert_eq!(parse_since("0h"), None);
        assert_eq!(parse_since("xh"), None);
    }
}
