//! Risk: two independent scores fused into one color.
//!
//! Every artifact carries two *separate* risk scores:
//!
//! * **a-priori** — computable at write time from cheap structural signals;
//! * **a-posteriori** — a retroactive, event-driven score from later evidence.
//!
//! They are fused into a single green→red [`RiskColor`] for the GUI, but the internal
//! distinction is preserved so the GUI can surface the gold debugging signal:
//! *"a-priori green (looked safe) → a-posteriori red (turned out dangerous)"*.
//!
//! This module is pure data + math. The actual score *computation* lives in the
//! `brain0-risk` crate; here we define the types stored on the node and the fusion rule.

use serde::{Deserialize, Serialize};

/// Clamp a raw value into the canonical `0.0..=1.0` risk range.
#[must_use]
pub fn clamp_unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

/// Normalized contributions to the a-priori score (each in `0.0..=1.0`), kept for
/// transparency and for the GUI's risk breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AprioriFactors {
    /// Centrality of the symbol in the dependency graph.
    pub centrality: f32,
    /// Blast radius: weight of downstream symbols depending on this artifact.
    pub blast_radius: f32,
    /// Historical churn of the artifact.
    pub churn: f32,
    /// Lack of test coverage (1.0 = no tests, 0.0 = well covered).
    pub test_gap: f32,
    /// Size of the diff.
    pub diff_size: f32,
    /// Declared↔done discrepancy (drift) attached to the producing task.
    pub drift: f32,
}

impl Default for AprioriFactors {
    fn default() -> Self {
        Self {
            centrality: 0.0,
            blast_radius: 0.0,
            churn: 0.0,
            test_gap: 0.0,
            diff_size: 0.0,
            drift: 0.0,
        }
    }
}

/// Normalized contributions to the a-posteriori score (each in `0.0..=1.0`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AposterioriFactors {
    /// The artifact (or its change) was reverted.
    pub reverted: f32,
    /// A fix landed on the same symbol within a short window.
    pub immediate_fix: f32,
    /// Tests that previously passed broke after the change.
    pub tests_broken: f32,
    /// An issue was linked to the commit/version.
    pub linked_issue: f32,
}

impl Default for AposterioriFactors {
    fn default() -> Self {
        Self {
            reverted: 0.0,
            immediate_fix: 0.0,
            tests_broken: 0.0,
            linked_issue: 0.0,
        }
    }
}

/// The two scalar scores stored on an artifact node. The full factor breakdowns are
/// computed by `brain0-risk` and may be persisted alongside; the node only needs the two
/// scalars to drive color and ranking.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    /// Write-time structural risk, `0.0..=1.0`.
    pub apriori: f32,
    /// Retroactive evidence-based risk, `0.0..=1.0`.
    pub aposteriori: f32,
}

impl Default for RiskState {
    fn default() -> Self {
        Self {
            apriori: 0.0,
            aposteriori: 0.0,
        }
    }
}

/// A qualitative classification of the relationship between the two scores. The
/// [`RiskTransition::SafeToDangerous`] case is the gold debugging signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTransition {
    /// No meaningful retroactive evidence yet; color reflects a-priori only.
    Pending,
    /// Looked safe up front and stayed safe.
    Stable,
    /// Looked safe a-priori but proved dangerous a-posteriori (the gold signal).
    SafeToDangerous,
    /// Looked risky a-priori and confirmed dangerous a-posteriori.
    ConfirmedDangerous,
    /// Looked risky a-priori but proved fine a-posteriori.
    OverestimatedRisk,
}

/// 24-bit RGB color used by the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// `#rrggbb` hex string (handy for web/PixiJS).
    #[must_use]
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// The fused, display-ready risk color plus the preserved internal interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskColor {
    /// Fused scalar `0.0..=1.0` (0 = green/safe, 1 = red/dangerous).
    pub fused: f32,
    pub rgb: Rgb,
    pub transition: RiskTransition,
}

/// Boundaries used to classify [`RiskTransition`]. `low` and `high` partition the unit
/// interval into safe / mid / dangerous bands.
const LOW: f32 = 0.34;
const HIGH: f32 = 0.66;
/// Below this, a-posteriori is considered "no evidence yet".
const APOSTERIORI_EVIDENCE: f32 = 0.05;

impl RiskState {
    #[must_use]
    pub fn new(apriori: f32, aposteriori: f32) -> Self {
        Self {
            apriori: clamp_unit(apriori),
            aposteriori: clamp_unit(aposteriori),
        }
    }

    /// Probabilistic-OR fusion: either score can raise the overall risk, and they
    /// reinforce each other. Monotonic in both inputs and bounded to `0.0..=1.0`.
    #[must_use]
    pub fn fused(&self) -> f32 {
        let a = clamp_unit(self.apriori);
        let p = clamp_unit(self.aposteriori);
        1.0 - (1.0 - a) * (1.0 - p)
    }

    /// Classify the relationship between the two scores.
    #[must_use]
    pub fn transition(&self) -> RiskTransition {
        let a = clamp_unit(self.apriori);
        let p = clamp_unit(self.aposteriori);

        if p < APOSTERIORI_EVIDENCE {
            return RiskTransition::Pending;
        }
        let looked_safe = a < LOW;
        let looked_risky = a >= HIGH;
        let proved_dangerous = p >= HIGH;
        let proved_safe = p < LOW;

        match (looked_safe, looked_risky, proved_dangerous, proved_safe) {
            (true, _, true, _) => RiskTransition::SafeToDangerous,
            (_, true, true, _) => RiskTransition::ConfirmedDangerous,
            (_, true, _, true) => RiskTransition::OverestimatedRisk,
            (true, _, _, true) => RiskTransition::Stable,
            _ => {
                // Mid-band cases: lean on whichever side dominates.
                if proved_dangerous {
                    RiskTransition::ConfirmedDangerous
                } else {
                    RiskTransition::Stable
                }
            }
        }
    }

    /// Map the fused score to a green→red color via an HSL hue sweep (120° green → 0° red)
    /// with a yellow midpoint, and attach the preserved transition.
    #[must_use]
    pub fn color(&self) -> RiskColor {
        let fused = self.fused();
        RiskColor {
            fused,
            rgb: hue_sweep_rgb(fused),
            transition: self.transition(),
        }
    }

    /// True when the change looked safe up front but later proved dangerous.
    #[must_use]
    pub fn is_gold_signal(&self) -> bool {
        self.transition() == RiskTransition::SafeToDangerous
    }
}

/// Map `t` in `0.0..=1.0` to an RGB on the green→yellow→red sweep.
///
/// Implemented as a hue rotation from 120° (green) to 0° (red) at full saturation and
/// 50% lightness, which yields the perceptually familiar traffic-light gradient.
#[must_use]
pub fn hue_sweep_rgb(t: f32) -> Rgb {
    let t = clamp_unit(t);
    let hue = 120.0 * (1.0 - t); // 120=green .. 0=red
    hsl_to_rgb(hue, 1.0, 0.5)
}

fn hsl_to_rgb(h_deg: f32, s: f32, l: f32) -> Rgb {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h = h_deg / 60.0;
    let x = c * (1.0 - (h.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Rgb {
        r: (((r1 + m) * 255.0).round()) as u8,
        g: (((g1 + m) * 255.0).round()) as u8,
        b: (((b1 + m) * 255.0).round()) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fusion_is_monotonic_and_bounded() {
        assert_eq!(RiskState::new(0.0, 0.0).fused(), 0.0);
        assert!((RiskState::new(1.0, 0.0).fused() - 1.0).abs() < 1e-6);
        assert!((RiskState::new(0.0, 1.0).fused() - 1.0).abs() < 1e-6);
        // Either signal alone raises risk; both raise it further.
        let only_a = RiskState::new(0.5, 0.0).fused();
        let only_p = RiskState::new(0.0, 0.5).fused();
        let both = RiskState::new(0.5, 0.5).fused();
        assert!(both > only_a && both > only_p);
    }

    #[test]
    fn gold_signal_safe_to_dangerous() {
        let r = RiskState::new(0.1, 0.9);
        assert_eq!(r.transition(), RiskTransition::SafeToDangerous);
        assert!(r.is_gold_signal());
    }

    #[test]
    fn pending_when_no_posterior_evidence() {
        assert_eq!(
            RiskState::new(0.9, 0.0).transition(),
            RiskTransition::Pending
        );
    }

    #[test]
    fn confirmed_and_overestimated() {
        assert_eq!(
            RiskState::new(0.9, 0.9).transition(),
            RiskTransition::ConfirmedDangerous
        );
        assert_eq!(
            RiskState::new(0.9, 0.1).transition(),
            RiskTransition::OverestimatedRisk
        );
    }

    #[test]
    fn color_endpoints_are_green_and_red() {
        let green = hue_sweep_rgb(0.0);
        assert!(green.g > 200 && green.r < 60);
        let red = hue_sweep_rgb(1.0);
        assert!(red.r > 200 && red.g < 60);
        let mid = hue_sweep_rgb(0.5);
        // Yellow-ish: high red and green, low blue.
        assert!(mid.r > 200 && mid.g > 200 && mid.b < 60);
    }

    #[test]
    fn hex_format() {
        assert_eq!(Rgb { r: 0, g: 255, b: 0 }.to_hex(), "#00ff00");
    }
}
