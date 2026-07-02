//! Deterministic, cross-machine symbol identity and rename/move tracking.
//!
//! Two machines observing the *same* symbol in the *same* state must compute the *same*
//! artifact id, so independent observers converge on one shared graph without merge logic
//!. Identity is realized in two layers:
//!
//! 1. **Deterministic id** — [`brain0_model::ArtifactId::derive`] hashes
//!    `repo + level + qualified_path + structural-fingerprint`.
//! 2. **Resolution** — [`resolve`] maps a fresh batch of observations against the
//!    previously-known artifacts of the same scope, deciding for each whether it is an
//!    edit (same path, changed structure), a rename/move (path changed but structure still
//!    matches above a similarity threshold → *same node, new coordinates*), or a brand new
//!    node. This realizes the principle "no unjustified new nodes".
//!
//! ## Split / merge policy
//! Matching is path-first, then a one-to-one greedy fuzzy match (highest similarity first):
//! * **Split** (one symbol becomes several): the best-matching survivor continues the
//!   identity (rename/modify); the extra symbols are new nodes (`Added`).
//! * **Merge** (several symbols become one): the best-matching source continues the
//!   identity; the others are `Removed`.
//!
//! This keeps resolution deterministic and total, with no node used twice.

pub mod similarity;

pub use similarity::jaccard;

use brain0_model::{ArtifactId, Level};
use brain0_parser::Fingerprint;

/// Tunable parameters for identity resolution.
#[derive(Debug, Clone, Copy)]
pub struct IdentityConfig {
    /// Minimum Jaccard similarity (over fingerprint shingles) for two symbols at different
    /// paths to be considered the *same* identity (a rename/move). `0.0..=1.0`.
    pub rename_threshold: f64,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            rename_threshold: 0.6,
        }
    }
}

/// A previously-known artifact in the scope being resolved.
#[derive(Debug, Clone)]
pub struct KnownSymbol {
    pub id: ArtifactId,
    pub qualified_path: String,
    pub fingerprint: Fingerprint,
}

/// A symbol observed in the current state.
#[derive(Debug, Clone)]
pub struct ObservedSymbol {
    pub qualified_path: String,
    pub fingerprint: Fingerprint,
}

/// How an observation relates to the prior state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeStatus {
    /// Same path, identical structural fingerprint.
    Unchanged,
    /// Same path, changed structure (an in-place edit).
    Modified,
    /// No prior node matched; a new identity is born.
    Added,
    /// Matched a prior node by similarity; path changed within the same container.
    Renamed { from: String },
    /// Matched a prior node by similarity; moved to a different container/file.
    Moved { from: String },
}

/// The resolution of a single observation: the (reused or freshly-minted) identity and how
/// it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSymbol {
    pub id: ArtifactId,
    pub status: ChangeStatus,
}

/// The full outcome of resolving an observation batch against the known set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveOutcome {
    /// One entry per input observation, in the same order.
    pub resolved: Vec<ResolvedSymbol>,
    /// Ids of known artifacts that are no longer present (deleted, or the losing side of a
    /// merge).
    pub removed: Vec<ArtifactId>,
}

/// The container (file portion) of a qualified path `"<rel_path>::<dotted>"`.
fn container_of(qualified_path: &str) -> &str {
    qualified_path.split("::").next().unwrap_or(qualified_path)
}

/// Resolve `observed` symbols (all at `level` within `repo`) against the `known` set.
///
/// `known` and `observed` must all share the same [`Level`]; the caller groups by level
/// (e.g. all symbols, or all files) and may choose the scope (per-file for edits, repo-wide
/// to catch cross-file moves).
#[must_use]
pub fn resolve(
    repo: &str,
    level: Level,
    known: &[KnownSymbol],
    observed: &[ObservedSymbol],
    config: &IdentityConfig,
) -> ResolveOutcome {
    let mut resolved: Vec<Option<ResolvedSymbol>> = vec![None; observed.len()];
    let mut known_consumed = vec![false; known.len()];
    let mut appeared: Vec<usize> = Vec::new();

    // Pass 1: exact path match → edit (Unchanged/Modified). First known wins per path.
    for (oi, obs) in observed.iter().enumerate() {
        let matched = known
            .iter()
            .enumerate()
            .find(|(ki, k)| !known_consumed[*ki] && k.qualified_path == obs.qualified_path);
        if let Some((ki, k)) = matched {
            known_consumed[ki] = true;
            let status = if k.fingerprint.hash == obs.fingerprint.hash {
                ChangeStatus::Unchanged
            } else {
                ChangeStatus::Modified
            };
            resolved[oi] = Some(ResolvedSymbol {
                id: k.id.clone(),
                status,
            });
        } else {
            appeared.push(oi);
        }
    }

    let disappeared: Vec<usize> = (0..known.len()).filter(|ki| !known_consumed[*ki]).collect();

    // Pass 2: fuzzy match appeared ↔ disappeared (rename/move). Build all candidate pairs
    // above threshold, then assign greedily by descending similarity, one-to-one.
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for &ki in &disappeared {
        for &oi in &appeared {
            let sim = jaccard(
                &known[ki].fingerprint.shingles,
                &observed[oi].fingerprint.shingles,
            );
            if sim >= config.rename_threshold {
                pairs.push((sim, ki, oi));
            }
        }
    }
    // Deterministic ordering: similarity desc, then known path, then observed path.
    pairs.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| known[a.1].qualified_path.cmp(&known[b.1].qualified_path))
            .then_with(|| {
                observed[a.2]
                    .qualified_path
                    .cmp(&observed[b.2].qualified_path)
            })
    });

    let mut known_used = vec![false; known.len()];
    let mut obs_matched = vec![false; observed.len()];
    for (_sim, ki, oi) in pairs {
        if known_used[ki] || obs_matched[oi] {
            continue;
        }
        known_used[ki] = true;
        obs_matched[oi] = true;
        let from = known[ki].qualified_path.clone();
        let status = if container_of(&from) == container_of(&observed[oi].qualified_path) {
            ChangeStatus::Renamed { from }
        } else {
            ChangeStatus::Moved { from }
        };
        resolved[oi] = Some(ResolvedSymbol {
            id: known[ki].id.clone(),
            status,
        });
    }

    // Pass 3: unmatched appeared → new nodes with deterministic ids.
    for &oi in &appeared {
        if resolved[oi].is_none() {
            let obs = &observed[oi];
            let id = ArtifactId::derive(repo, level, &obs.qualified_path, &obs.fingerprint.hash);
            resolved[oi] = Some(ResolvedSymbol {
                id,
                status: ChangeStatus::Added,
            });
        }
    }

    // Unmatched disappeared → removed.
    let removed: Vec<ArtifactId> = disappeared
        .iter()
        .filter(|ki| !known_used[**ki])
        .map(|ki| known[*ki].id.clone())
        .collect();

    ResolveOutcome {
        resolved: resolved
            .into_iter()
            .map(|entry| entry.expect("every observation is resolved"))
            .collect(),
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_parser::parse_source;

    /// Helper: fingerprint of the single function `f` in a Python snippet.
    fn fp(body: &str) -> Fingerprint {
        let parsed = parse_source("m.py", body).unwrap();
        parsed
            .symbols
            .into_iter()
            .next()
            .expect("a symbol")
            .fingerprint
    }

    fn known(id: &str, path: &str, fp: Fingerprint) -> KnownSymbol {
        KnownSymbol {
            id: ArtifactId::new(id),
            qualified_path: path.to_owned(),
            fingerprint: fp,
        }
    }

    fn obs(path: &str, fp: Fingerprint) -> ObservedSymbol {
        ObservedSymbol {
            qualified_path: path.to_owned(),
            fingerprint: fp,
        }
    }

    #[test]
    fn deterministic_id_is_cross_machine() {
        // Two "machines" independently derive an id for the same symbol+state.
        let f = fp("def f(x):\n    return x + 1\n");
        let machine_a = ArtifactId::derive("repo", Level::Symbol, "m.py::f", &f.hash);
        let machine_b = ArtifactId::derive("repo", Level::Symbol, "m.py::f", &f.hash);
        assert_eq!(machine_a, machine_b);
    }

    #[test]
    fn unchanged_when_same_path_same_fingerprint() {
        let f = fp("def f(x):\n    return x\n");
        let k = vec![known("art_1", "m.py::f", f.clone())];
        let o = vec![obs("m.py::f", f)];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(out.resolved[0].status, ChangeStatus::Unchanged);
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_1"));
        assert!(out.removed.is_empty());
    }

    #[test]
    fn modified_when_same_path_changed_structure() {
        let k = vec![known("art_1", "m.py::f", fp("def f(x):\n    return x\n"))];
        let o = vec![obs(
            "m.py::f",
            fp("def f(x):\n    if x:\n        return x\n    return 0\n"),
        )];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(out.resolved[0].status, ChangeStatus::Modified);
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_1")); // same identity
    }

    #[test]
    fn rename_tracks_same_identity() {
        // Identical structure, name changed → same container, path changed.
        let f = fp("def whatever(x):\n    return x + 1\n");
        let k = vec![known("art_1", "m.py::old_name", f.clone())];
        let o = vec![obs("m.py::new_name", f)];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(
            out.resolved[0].status,
            ChangeStatus::Renamed {
                from: "m.py::old_name".to_owned()
            }
        );
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_1")); // identity preserved
        assert!(out.removed.is_empty());
    }

    #[test]
    fn move_to_other_file_tracks_same_identity() {
        let f = fp("def f(x):\n    return x + 1\n");
        let k = vec![known("art_1", "a.py::f", f.clone())];
        let o = vec![obs("b.py::f", f)];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(
            out.resolved[0].status,
            ChangeStatus::Moved {
                from: "a.py::f".to_owned()
            }
        );
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_1"));
    }

    #[test]
    fn added_and_removed_below_threshold() {
        // Two structurally unrelated functions → no fuzzy match.
        let k = vec![known(
            "art_1",
            "m.py::gone",
            fp("def gone(x):\n    return x\n"),
        )];
        let o = vec![obs(
            "m.py::fresh",
            fp("class fresh:\n    def a(self):\n        for i in range(10):\n            print(i)\n        return None\n"),
        )];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(out.resolved[0].status, ChangeStatus::Added);
        assert!(out.resolved[0].id.as_str().starts_with("art_"));
        assert_eq!(out.removed, vec![ArtifactId::new("art_1")]);
    }

    #[test]
    fn split_keeps_best_match_and_adds_the_rest() {
        // `big` splits into `part_a` (identical structure) and `part_b` (different).
        let big = fp("def big(x):\n    return x + 1\n");
        let part_a = fp("def part_a(x):\n    return x + 1\n"); // identical structure to big
        let part_b = fp("def part_b(x):\n    for i in x:\n        print(i)\n    return None\n");
        let k = vec![known("art_big", "m.py::big", big)];
        let o = vec![obs("m.py::part_a", part_a), obs("m.py::part_b", part_b)];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        // part_a continues big's identity; part_b is a new node.
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_big"));
        assert!(matches!(
            out.resolved[0].status,
            ChangeStatus::Renamed { .. }
        ));
        assert_eq!(out.resolved[1].status, ChangeStatus::Added);
        assert!(out.removed.is_empty());
    }

    #[test]
    fn merge_keeps_best_match_and_removes_the_rest() {
        let f = fp("def keep(x):\n    return x + 1\n");
        let g = fp("def drop(x):\n    return x + 1\n"); // identical structure
                                                        // Both known; only one survivor with identical structure remains.
        let survivor = fp("def keep(x):\n    return x + 1\n");
        let k = vec![
            known("art_keep", "m.py::keep", f),
            known("art_drop", "m.py::drop", g),
        ];
        let o = vec![obs("m.py::keep", survivor)];
        let out = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        // Exact-path match wins for `keep`; `drop` disappears → removed.
        assert_eq!(out.resolved[0].id, ArtifactId::new("art_keep"));
        assert_eq!(out.removed, vec![ArtifactId::new("art_drop")]);
    }

    #[test]
    fn one_to_one_assignment_is_deterministic() {
        let f = fp("def f(x):\n    return x + 1\n");
        let k = vec![
            known("art_1", "m.py::a", f.clone()),
            known("art_2", "m.py::b", f.clone()),
        ];
        let o = vec![obs("m.py::c", f.clone()), obs("m.py::d", f)];
        let out1 = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        let out2 = resolve("repo", Level::Symbol, &k, &o, &IdentityConfig::default());
        assert_eq!(out1, out2); // stable across runs
    }
}
