//! Two-score risk engine.
//!
//! Risk is modelled as two *independent* scores that are fused into one green→red color
//! (the fusion + color live in [`brain0_model::RiskState`]):
//!
//! * **a-priori** — a write-time pure function of cheap structural signals: centrality,
//!   blast radius, churn, test gap, diff size, and declared↔done drift.
//! * **a-posteriori** — a retroactive, event-driven score from later evidence: the change
//!   was reverted, an immediate fix landed, tests broke, or an issue was linked.
//!
//! Aggregate nodes (file/module/repo) derive their risk from their children. The gold
//! debugging signal — *looked safe, proved dangerous* — is preserved by keeping the two
//! scores separate (see [`brain0_model::RiskTransition`]).

use brain0_model::risk::clamp_unit;
use brain0_model::{AposterioriFactors, AprioriFactors, ArtifactId, EdgeKind, RiskState};
use brain0_storage::{Storage, StorageError};
use std::collections::{HashSet, VecDeque};
use thiserror::Error;

/// Errors produced by the risk engine.
#[derive(Debug, Error)]
pub enum RiskError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("artifact not found: {0}")]
    NotFound(String),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, RiskError>;

// --- pure scoring --------------------------------------------------------------------

/// Per-signal weights = each factor's maximum contribution to the a-priori score when it is at
/// 1.0. These feed a probabilistic-OR (not a mean), so a factor that is absent (0.0) — e.g.
/// centrality/blast before a dependency graph exists — contributes nothing AND does not dilute
/// the others. A mean over all six factors instead let the always-zero signals cap the score
/// near the floor (every node stayed green); the OR lets any genuinely strong signal surface.
struct AprioriWeights {
    centrality: f32,
    blast_radius: f32,
    churn: f32,
    test_gap: f32,
    diff_size: f32,
    drift: f32,
}

const APRIORI_WEIGHTS: AprioriWeights = AprioriWeights {
    centrality: 0.5,
    blast_radius: 0.7,
    churn: 0.5,
    test_gap: 0.4,
    diff_size: 0.45,
    drift: 0.6,
};

/// Combine a-priori factors into a single `0.0..=1.0` score via a weighted probabilistic-OR:
/// `1 - ∏(1 - factor·weight)`. Absent factors leave the product unchanged, so missing signals
/// never drag the score down, while one strong signal (or several mild ones) can push it high.
#[must_use]
pub fn apriori_score(f: &AprioriFactors) -> f32 {
    let w = &APRIORI_WEIGHTS;
    let terms = [
        (f.centrality, w.centrality),
        (f.blast_radius, w.blast_radius),
        (f.churn, w.churn),
        (f.test_gap, w.test_gap),
        (f.diff_size, w.diff_size),
        (f.drift, w.drift),
    ];
    let mut survival = 1.0f32;
    for (value, weight) in terms {
        survival *= 1.0 - clamp_unit(value) * weight;
    }
    clamp_unit(1.0 - survival)
}

/// Combine a-posteriori factors via a weighted probabilistic-OR: strong evidence (a full
/// revert) alone drives the score high, and multiple signals reinforce each other.
#[must_use]
pub fn aposteriori_score(f: &AposterioriFactors) -> f32 {
    let terms = [
        (f.reverted, 1.0f32),
        (f.tests_broken, 0.9),
        (f.immediate_fix, 0.6),
        (f.linked_issue, 0.5),
    ];
    let mut survival = 1.0f32;
    for (value, weight) in terms {
        survival *= 1.0 - clamp_unit(value) * weight;
    }
    clamp_unit(1.0 - survival)
}

/// Normalize a non-negative count into `0.0..=1.0` with a soft knee at `half`
/// (`count == half` → 0.5).
fn saturate(count: usize, half: f32) -> f32 {
    saturate_f(count as f32, half)
}

/// [`saturate`] over a fractional magnitude (e.g. a recency-decayed change count).
fn saturate_f(x: f32, half: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    x / (x + half)
}

/// Churn half-life: a change this many days old counts half as much as one made now. Lifetime
/// version counts made every long-lived file permanently "orange"; recency-decayed churn means
/// *hot now* scores high and mature, stable files cool back toward green.
const CHURN_HALF_LIFE_DAYS: f32 = 14.0;

/// Recency-decayed churn magnitude: `Σ 0.5^(age_days / half_life)` over the artifact's
/// versions, with ages measured against `now` (the repo's latest observed timestamp — passed
/// in so the score is deterministic for a given index, never wall-clock dependent).
fn decayed_churn(versions: &[brain0_model::ArtifactVersion], now: &brain0_model::Timestamp) -> f32 {
    versions
        .iter()
        .map(|v| {
            let age_days = (*now - v.timestamp).num_seconds().max(0) as f32 / 86_400.0;
            0.5f32.powf(age_days / CHURN_HALF_LIFE_DAYS)
        })
        .sum()
}

// --- a-priori derivation from the graph ----------------------------------------------

/// Signals that the engine cannot derive structurally and must be supplied by the caller.
#[derive(Debug, Clone, Copy)]
pub struct AprioriContext {
    /// Normalized diff size for the latest change (`0.0..=1.0`).
    pub diff_size: f32,
    /// Test gap (1.0 = untested, 0.0 = well covered).
    pub test_gap: f32,
    /// Declared↔done drift attached to the producing task (`0.0..=1.0`).
    pub drift: f32,
    /// "Now" for churn recency-decay: the repo's latest observed timestamp (NOT wall clock),
    /// so a given index always derives the same scores.
    pub now: brain0_model::Timestamp,
}

impl Default for AprioriContext {
    fn default() -> Self {
        // Neutral defaults: unknown test coverage, no measured diff, no drift, epoch "now"
        // (callers derive the real horizon from the index).
        Self {
            diff_size: 0.0,
            test_gap: 0.5,
            drift: 0.0,
            now: brain0_model::chrono::DateTime::<brain0_model::chrono::Utc>::UNIX_EPOCH,
        }
    }
}

/// Derive the a-priori factors for an artifact from the graph plus the supplied context.
pub fn derive_apriori_factors(
    storage: &dyn Storage,
    artifact_id: &ArtifactId,
    ctx: &AprioriContext,
) -> Result<AprioriFactors> {
    let out_deps = storage
        .out_edges(EdgeKind::ArtifactDependsOn, artifact_id.as_str())?
        .len();
    let in_deps = storage
        .in_edges(EdgeKind::ArtifactDependsOn, artifact_id.as_str())?
        .len();
    let versions = storage.artifact_versions(artifact_id)?;
    let blast = blast_radius(storage, artifact_id)?;

    Ok(AprioriFactors {
        centrality: saturate(out_deps + in_deps, 5.0),
        blast_radius: saturate(blast, 8.0),
        // Recency-decayed: ~3 "current-equivalent" changes → 0.5. Mature files cool to green.
        churn: saturate_f(decayed_churn(&versions, &ctx.now), 3.0),
        test_gap: clamp_unit(ctx.test_gap),
        diff_size: clamp_unit(ctx.diff_size),
        drift: clamp_unit(ctx.drift),
    })
}

/// Count transitive dependents (who depends on this artifact, directly or indirectly) via
/// `ARTIFACT_DEPENDS_ON` edges.
fn blast_radius(storage: &dyn Storage, artifact_id: &ArtifactId) -> Result<usize> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(artifact_id.as_str().to_owned());
    while let Some(current) = queue.pop_front() {
        for edge in storage.in_edges(EdgeKind::ArtifactDependsOn, &current)? {
            if let brain0_model::Edge::ArtifactDependsOn { dependent, .. } = edge {
                let dep = dependent.as_str().to_owned();
                if dep != artifact_id.as_str() && seen.insert(dep.clone()) {
                    queue.push_back(dep);
                }
            }
        }
    }
    Ok(seen.len())
}

/// Recompute and persist the a-priori score for an artifact (leaves the a-posteriori score
/// untouched). Returns the new [`RiskState`].
pub fn recompute_apriori(
    storage: &dyn Storage,
    artifact_id: &ArtifactId,
    ctx: &AprioriContext,
) -> Result<RiskState> {
    let factors = derive_apriori_factors(storage, artifact_id, ctx)?;
    let node = storage
        .get_artifact_node(artifact_id)?
        .ok_or_else(|| RiskError::NotFound(artifact_id.as_str().to_owned()))?;
    let risk = RiskState::new(apriori_score(&factors), node.risk.aposteriori);
    storage.update_artifact_risk(artifact_id, risk)?;
    Ok(risk)
}

// --- a-posteriori derivation from history --------------------------------------------

/// Derive a-posteriori factors from an artifact's version history plus external evidence.
///
/// * `immediate_fix` fires when two versions land within `fix_window`.
/// * `reverted` fires when a version returns to an earlier structural fingerprint.
/// * `tests_broken` / `linked_issue` are external evidence supplied by the caller.
pub fn derive_aposteriori_factors(
    storage: &dyn Storage,
    artifact_id: &ArtifactId,
    fix_window: chrono::Duration,
    tests_broken: bool,
    linked_issue: bool,
) -> Result<AposterioriFactors> {
    let versions = storage.artifact_versions(artifact_id)?;

    // An "immediate fix" must be FIX-SHAPED, not merely fast: a small corrective follow-up
    // (≤ max(5 lines, ¼ of the prior change)) landing within the window. A bare time check
    // flagged every burst of similar-sized commits — a completely normal workflow — as fixes.
    let mut immediate_fix = false;
    for pair in versions.windows(2) {
        let quick = pair[1].timestamp - pair[0].timestamp <= fix_window;
        let first_total = pair[0].lines_added + pair[0].lines_removed;
        let second_total = pair[1].lines_added + pair[1].lines_removed;
        let fix_shaped = second_total > 0
            && second_total < first_total
            && second_total <= (first_total / 4).max(5);
        if quick && fix_shaped {
            immediate_fix = true;
            break;
        }
    }

    let mut reverted = false;
    let mut seen: HashSet<&str> = HashSet::new();
    for version in &versions {
        if version.fingerprint.is_empty() {
            continue;
        }
        if !seen.insert(version.fingerprint.as_str()) {
            reverted = true; // a structural state reappeared
            break;
        }
    }

    Ok(AposterioriFactors {
        reverted: if reverted { 1.0 } else { 0.0 },
        immediate_fix: if immediate_fix { 1.0 } else { 0.0 },
        tests_broken: if tests_broken { 1.0 } else { 0.0 },
        linked_issue: if linked_issue { 1.0 } else { 0.0 },
    })
}

/// Apply a-posteriori factors to an artifact (leaves the a-priori score untouched). This is
/// the event-driven update that can turn a green node red retroactively.
pub fn apply_aposteriori(
    storage: &dyn Storage,
    artifact_id: &ArtifactId,
    factors: &AposterioriFactors,
) -> Result<RiskState> {
    let node = storage
        .get_artifact_node(artifact_id)?
        .ok_or_else(|| RiskError::NotFound(artifact_id.as_str().to_owned()))?;
    let risk = RiskState::new(node.risk.apriori, aposteriori_score(factors));
    storage.update_artifact_risk(artifact_id, risk)?;
    Ok(risk)
}

// --- aggregate derivation ------------------------------------------------------------

/// Derive an aggregate node's risk from its children: the riskiest child dominates each
/// dimension, so hotspots surface at every zoom level.
#[must_use]
pub fn derive_aggregate(children: &[RiskState]) -> RiskState {
    let apriori = children.iter().map(|r| r.apriori).fold(0.0f32, f32::max);
    let aposteriori = children
        .iter()
        .map(|r| r.aposteriori)
        .fold(0.0f32, f32::max);
    RiskState::new(apriori, aposteriori)
}

/// Recompute aggregate risk for a whole repo, bottom-up: files from symbols, modules from
/// files, repo from modules. A **file** also carries its own directly-computed risk (churn,
/// diff size, drift on the file itself — set before this runs), so a file with no parsed symbols
/// (e.g. a Rust source file) is not forced to zero, and a hot file outranks its calmest symbol.
/// Modules and repos are pure aggregates of their children.
pub fn recompute_aggregates(storage: &dyn Storage, repo: &str) -> Result<()> {
    use brain0_model::Level;
    for level in [Level::File, Level::Module, Level::Repo] {
        for node in storage.list_artifacts(repo, level)? {
            let children = storage.children(&node.id)?;
            let mut risks: Vec<RiskState> = children.iter().map(|c| c.risk).collect();
            if level == Level::File {
                risks.push(node.risk); // fuse the file's own direct risk with its symbols'
            }
            let risk = derive_aggregate(&risks);
            storage.update_artifact_risk(&node.id, risk)?;
        }
    }
    Ok(())
}

// --- language-agnostic co-change coupling --------------------------------------------

/// Tuning for the co-change (logical coupling) graph derived from commit history.
#[derive(Debug, Clone, Copy)]
pub struct CoChangeConfig {
    /// Minimum number of commits in which two files must change together to be linked.
    pub min_support: usize,
    /// Commits touching more files than this are treated as bulk/mechanical (merges, reformats,
    /// vendored drops, the initial import) and ignored — they would otherwise couple everything
    /// to everything.
    pub max_commit_files: usize,
}

impl Default for CoChangeConfig {
    fn default() -> Self {
        Self {
            min_support: 2,
            max_commit_files: 50,
        }
    }
}

/// Derive a **language-agnostic** dependency graph from version history: files that repeatedly
/// change in the same commit are *logically coupled*. This needs no parser and no per-language
/// import resolver — only git, which every repo has — so centrality and blast radius work for
/// any language. Coupling is symmetric, so each pair is linked in both directions via
/// [`brain0_model::Edge::ArtifactDependsOn`]. The graph is append-only: support only accumulates
/// as history grows, and re-runs are idempotent (edges are keyed by endpoints). Returns the
/// number of edges written.
///
/// Honest limitation: this is a *correlation* (what tends to change together), not a proven
/// static dependency. It is the standard generalizable proxy and matches the product question
/// "what does changing this affect" directly; per-language static-import packs can refine it
/// later by writing the same edge kind.
pub fn recompute_cochange_coupling(
    storage: &dyn Storage,
    repo: &str,
    cfg: &CoChangeConfig,
) -> Result<usize> {
    use brain0_model::{Edge, Level};
    use std::collections::{BTreeSet, HashMap};

    // 1. commit ref → the set of file artifacts that changed in it.
    let mut commits: HashMap<String, BTreeSet<String>> = HashMap::new();
    for file in storage.list_artifacts(repo, Level::File)? {
        for version in storage.artifact_versions(&file.id)? {
            commits
                .entry(version.source.ref_str().to_owned())
                .or_default()
                .insert(file.id.as_str().to_owned());
        }
    }

    // 2. count, for each unordered file pair, in how many non-bulk commits they changed together.
    let mut pair_support: HashMap<(String, String), usize> = HashMap::new();
    for files in commits.values() {
        if files.len() < 2 || files.len() > cfg.max_commit_files {
            continue;
        }
        // BTreeSet iterates in sorted order, so (i, j) with i < j yields canonical pairs.
        let ordered: Vec<&String> = files.iter().collect();
        for i in 0..ordered.len() {
            for j in (i + 1)..ordered.len() {
                *pair_support
                    .entry((ordered[i].clone(), ordered[j].clone()))
                    .or_insert(0) += 1;
            }
        }
    }

    // 3. link pairs at or above the support threshold, both directions (coupling is symmetric).
    let mut written = 0usize;
    for ((a, b), support) in pair_support {
        if support < cfg.min_support {
            continue;
        }
        storage.put_edge(&Edge::ArtifactDependsOn {
            dependent: ArtifactId::new(a.clone()),
            dependency: ArtifactId::new(b.clone()),
        })?;
        storage.put_edge(&Edge::ArtifactDependsOn {
            dependent: ArtifactId::new(b),
            dependency: ArtifactId::new(a),
        })?;
        written += 2;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::chrono::{self, TimeZone};
    use brain0_model::{
        Agent, ArtifactNode, ArtifactVersion, Author, ChangeKind, ChangeSource, Lang, Level,
        RiskTransition, Timestamp, VersionId,
    };
    use brain0_storage::{SqliteStorage, Storage};

    fn ts(secs: i64) -> Timestamp {
        chrono::Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    #[test]
    fn apriori_each_factor_increases_score() {
        let base = AprioriFactors::default();
        let base_score = apriori_score(&base);
        let mut bumped = base;
        bumped.drift = 1.0;
        assert!(apriori_score(&bumped) > base_score);
        let mut bumped2 = base;
        bumped2.blast_radius = 1.0;
        assert!(apriori_score(&bumped2) > base_score);
    }

    #[test]
    fn apriori_strong_signal_not_capped_by_absent_ones() {
        // The old weighted-mean crushed a lone signal toward zero because the always-absent
        // factors (centrality/blast without a dep graph) stayed in the denominator. With the
        // probabilistic-OR, churn alone registers clearly and drift alone drives high.
        let churn_only = AprioriFactors {
            churn: 1.0,
            ..AprioriFactors::default()
        };
        assert!(
            apriori_score(&churn_only) >= 0.4,
            "churn alone → {}",
            apriori_score(&churn_only)
        );
        let drift_only = AprioriFactors {
            drift: 1.0,
            ..AprioriFactors::default()
        };
        assert!(
            apriori_score(&drift_only) >= 0.55,
            "drift alone → {}",
            apriori_score(&drift_only)
        );
        // Empty signals stay at zero.
        assert_eq!(apriori_score(&AprioriFactors::default()), 0.0);
    }

    #[test]
    fn aposteriori_revert_drives_high() {
        let reverted = AposterioriFactors {
            reverted: 1.0,
            ..AposterioriFactors::default()
        };
        assert!(aposteriori_score(&reverted) >= 0.99);
        assert_eq!(aposteriori_score(&AposterioriFactors::default()), 0.0);
    }

    fn node(id: &str, level: Level, risk: RiskState) -> ArtifactNode {
        ArtifactNode {
            id: ArtifactId::new(id),
            level,
            repo: "repo".into(),
            qualified_path: id.into(),
            lang: Some(Lang::new("python")),
            parent_id: None,
            current_version: VersionId::new("ver_cur"),
            risk,
        }
    }

    fn version(id: &str, artifact: &str, secs: i64, fp: &str) -> ArtifactVersion {
        version_sized(id, artifact, secs, fp, 1)
    }

    fn version_sized(id: &str, artifact: &str, secs: i64, fp: &str, added: u32) -> ArtifactVersion {
        ArtifactVersion {
            id: VersionId::new(id),
            artifact_id: ArtifactId::new(artifact),
            timestamp: ts(secs),
            author: Author::new("Ada"),
            agent: Agent::new("claude-code"),
            source: ChangeSource::Git {
                commit_sha: id.into(),
            },
            qualified_path: "m.py::f".into(),
            fingerprint: fp.into(),
            change_kind: ChangeKind::Modified,
            lines_added: added,
            lines_removed: 0,
            diff_ref: None,
        }
    }

    #[test]
    fn churn_raises_apriori_via_history() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let id = ArtifactId::new("art_f");
        s.put_artifact_node(&node("art_f", Level::Symbol, RiskState::default()))
            .unwrap();
        for i in 0..6 {
            s.append_artifact_version(&version(
                &format!("ver_{i}"),
                "art_f",
                100 + i,
                &format!("fp{i}"),
            ))
            .unwrap();
        }
        let factors = derive_apriori_factors(&s, &id, &AprioriContext::default()).unwrap();
        assert!(factors.churn > 0.5, "6 versions → churn {}", factors.churn);
    }

    #[test]
    fn aposteriori_detects_revert_and_immediate_fix() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let id = ArtifactId::new("art_f");
        // v1 (fpA, 80 lines) @100 → v2 (fpB, 3 lines) @130: a quick, FIX-SHAPED follow-up.
        // v3 (fpA again) later [revert].
        s.append_artifact_version(&version_sized("ver_1", "art_f", 100, "fpA", 80))
            .unwrap();
        s.append_artifact_version(&version_sized("ver_2", "art_f", 130, "fpB", 3))
            .unwrap();
        s.append_artifact_version(&version("ver_3", "art_f", 5000, "fpA"))
            .unwrap();
        let factors =
            derive_aposteriori_factors(&s, &id, chrono::Duration::seconds(60), false, false)
                .unwrap();
        assert_eq!(factors.immediate_fix, 1.0); // small corrective follow-up within 60s
        assert_eq!(factors.reverted, 1.0); // fpA reappears at v3
    }

    #[test]
    fn burst_commits_of_similar_size_are_not_immediate_fixes() {
        // Two same-sized commits 30s apart — a normal commit burst, not a fix. The old bare
        // time check flagged every such pair and painted whole repos orange.
        let s = SqliteStorage::open_in_memory().unwrap();
        let id = ArtifactId::new("art_b");
        s.append_artifact_version(&version_sized("ver_1", "art_b", 100, "fpA", 40))
            .unwrap();
        s.append_artifact_version(&version_sized("ver_2", "art_b", 130, "fpB", 35))
            .unwrap();
        let factors =
            derive_aposteriori_factors(&s, &id, chrono::Duration::seconds(60), false, false)
                .unwrap();
        assert_eq!(factors.immediate_fix, 0.0);
    }

    #[test]
    fn churn_decays_with_age_so_stale_files_cool_down() {
        let s = SqliteStorage::open_in_memory().unwrap();
        // Same 4-version history for both files; "hot" changed just now, "cold" months ago.
        for (art, base) in [("art_hot", 0i64), ("art_cold", 0i64)] {
            for i in 0..4 {
                s.append_artifact_version(&version(
                    &format!("{art}_v{i}"),
                    art,
                    base + i * 60,
                    &format!("fp{i}"),
                ))
                .unwrap();
            }
        }
        let now_hot = ts(4 * 60); // right after art_hot's last change
        let now_cold = ts(120 * 86_400); // 120 days later
        let hot = derive_apriori_factors(
            &s,
            &ArtifactId::new("art_hot"),
            &AprioriContext {
                now: now_hot,
                ..AprioriContext::default()
            },
        )
        .unwrap();
        let cold = derive_apriori_factors(
            &s,
            &ArtifactId::new("art_cold"),
            &AprioriContext {
                now: now_cold,
                ..AprioriContext::default()
            },
        )
        .unwrap();
        assert!(hot.churn > 0.5, "recent activity must score: {}", hot.churn);
        assert!(
            cold.churn < 0.05,
            "months-old activity must cool down: {}",
            cold.churn
        );
        assert!(hot.churn > cold.churn * 5.0);
    }

    #[test]
    fn gold_signal_after_aposteriori_update() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let id = ArtifactId::new("art_f");
        // Looked safe a-priori.
        s.put_artifact_node(&node("art_f", Level::Symbol, RiskState::new(0.1, 0.0)))
            .unwrap();
        // Later proved dangerous: reverted + tests broken.
        let factors = AposterioriFactors {
            reverted: 1.0,
            tests_broken: 1.0,
            ..AposterioriFactors::default()
        };
        let risk = apply_aposteriori(&s, &id, &factors).unwrap();
        assert_eq!(risk.transition(), RiskTransition::SafeToDangerous);
        assert!(risk.is_gold_signal());
        // Persisted.
        let reloaded = s.get_artifact_node(&id).unwrap().unwrap();
        assert!(reloaded.risk.is_gold_signal());
    }

    #[test]
    fn aggregate_takes_riskiest_child() {
        let agg = derive_aggregate(&[
            RiskState::new(0.1, 0.2),
            RiskState::new(0.7, 0.0),
            RiskState::new(0.3, 0.9),
        ]);
        assert!((agg.apriori - 0.7).abs() < 1e-6);
        assert!((agg.aposteriori - 0.9).abs() < 1e-6);
        assert_eq!(derive_aggregate(&[]), RiskState::new(0.0, 0.0));
    }

    #[test]
    fn recompute_aggregates_rolls_up_to_repo() {
        let s = SqliteStorage::open_in_memory().unwrap();
        // repo -> file -> two symbols
        s.put_artifact_node(&node("art_repo", Level::Repo, RiskState::default()))
            .unwrap();
        let mut file = node("art_file", Level::File, RiskState::default());
        file.parent_id = Some(ArtifactId::new("art_repo"));
        s.put_artifact_node(&file).unwrap();
        for (sid, risk) in [
            ("art_s1", RiskState::new(0.2, 0.0)),
            ("art_s2", RiskState::new(0.8, 0.4)),
        ] {
            let mut sym = node(sid, Level::Symbol, risk);
            sym.parent_id = Some(ArtifactId::new("art_file"));
            s.put_artifact_node(&sym).unwrap();
        }
        recompute_aggregates(&s, "repo").unwrap();
        let file = s
            .get_artifact_node(&ArtifactId::new("art_file"))
            .unwrap()
            .unwrap();
        assert!((file.risk.apriori - 0.8).abs() < 1e-6); // riskiest symbol
        let repo = s
            .get_artifact_node(&ArtifactId::new("art_repo"))
            .unwrap()
            .unwrap();
        assert!((repo.risk.apriori - 0.8).abs() < 1e-6); // rolled up
    }

    /// An artifact version attributed to an explicit commit (so several artifacts can share one).
    fn version_in(vid: &str, artifact: &str, commit: &str) -> ArtifactVersion {
        ArtifactVersion {
            id: VersionId::new(vid),
            artifact_id: ArtifactId::new(artifact),
            timestamp: ts(100),
            author: Author::new("Ada"),
            agent: Agent::new("claude-code"),
            source: ChangeSource::Git {
                commit_sha: commit.into(),
            },
            qualified_path: artifact.into(),
            fingerprint: "fp".into(),
            change_kind: ChangeKind::Modified,
            lines_added: 1,
            lines_removed: 0,
            diff_ref: None,
        }
    }

    #[test]
    fn cochange_links_files_that_repeatedly_change_together() {
        let s = SqliteStorage::open_in_memory().unwrap();
        for f in ["fa", "fb", "fc"] {
            s.put_artifact_node(&node(f, Level::File, RiskState::default()))
                .unwrap();
        }
        // fa+fb change together in c1 AND c2 (support 2); fc only joins once (c2).
        for v in [
            version_in("v1", "fa", "c1"),
            version_in("v2", "fb", "c1"),
            version_in("v3", "fa", "c2"),
            version_in("v4", "fb", "c2"),
            version_in("v5", "fc", "c2"),
        ] {
            s.append_artifact_version(&v).unwrap();
        }
        let written = recompute_cochange_coupling(&s, "repo", &CoChangeConfig::default()).unwrap();
        assert_eq!(written, 2, "only fa↔fb meet support 2 (both directions)");
        let fa = s.out_edges(EdgeKind::ArtifactDependsOn, "fa").unwrap();
        assert!(fa.iter().any(|e| matches!(
            e,
            brain0_model::Edge::ArtifactDependsOn { dependency, .. } if dependency.as_str() == "fb"
        )));
        // fc co-changed only once → below support → not linked.
        assert!(s
            .out_edges(EdgeKind::ArtifactDependsOn, "fc")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn cochange_ignores_bulk_commits() {
        let s = SqliteStorage::open_in_memory().unwrap();
        for f in ["fa", "fb", "fc"] {
            s.put_artifact_node(&node(f, Level::File, RiskState::default()))
                .unwrap();
        }
        // One commit touches 3 files; with max_commit_files = 2 it is bulk → ignored entirely.
        for v in [
            version_in("v1", "fa", "big"),
            version_in("v2", "fb", "big"),
            version_in("v3", "fc", "big"),
        ] {
            s.append_artifact_version(&v).unwrap();
        }
        let cfg = CoChangeConfig {
            min_support: 1,
            max_commit_files: 2,
        };
        assert_eq!(recompute_cochange_coupling(&s, "repo", &cfg).unwrap(), 0);
    }
}
