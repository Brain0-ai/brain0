//! Declared changes and drift — the "declared ↔ done" data.
//!
//! A coding agent *declares* what it intends/claims to change (via MCP). The observer
//! records what *actually* happened. The reconciliation worker (crate `brain0-reconcile`)
//! compares the two and attaches a [`Drift`] signal to the task. These are pure data
//! types; the comparison logic lives in `brain0-reconcile`.

use serde::{Deserialize, Serialize};

use crate::risk::clamp_unit;

/// A single change the agent claims to have made (or intends to make).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredChange {
    /// Path the agent says it touched (repo-relative).
    pub path: String,
    /// Optional symbol/function the agent says it touched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Optional free-text intent for this specific change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

impl DeclaredChange {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            symbol: None,
            intent: None,
        }
    }
}

/// The discrepancy between what an agent declared and what actually happened. A
/// first-class, visible signal in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drift {
    /// Drift magnitude in `0.0..=1.0` (0 = perfect match, 1 = total mismatch).
    pub score: f32,
    /// Paths that changed but were never declared (gap-filling caught these).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undeclared: Vec<String>,
    /// Paths the agent declared but that did not actually change (phantom declarations).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phantom: Vec<String>,
}

impl Drift {
    /// A drift result with no discrepancy.
    #[must_use]
    pub fn none() -> Self {
        Self {
            score: 0.0,
            undeclared: Vec::new(),
            phantom: Vec::new(),
        }
    }

    #[must_use]
    pub fn new(score: f32, undeclared: Vec<String>, phantom: Vec<String>) -> Self {
        Self {
            score: clamp_unit(score),
            undeclared,
            phantom,
        }
    }

    /// Whether any discrepancy was detected.
    #[must_use]
    pub fn is_present(&self) -> bool {
        !self.undeclared.is_empty() || !self.phantom.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_presence() {
        assert!(!Drift::none().is_present());
        assert!(Drift::new(0.5, vec!["a.rs".into()], vec![]).is_present());
    }

    #[test]
    fn score_is_clamped() {
        assert_eq!(Drift::new(2.0, vec![], vec![]).score, 1.0);
    }
}
