//! Core data model for brain0.
//!
//! This crate defines the bipartite, multi-level decision graph: task nodes (intent),
//! artifact nodes (code), their dated versions, and the typed edges that make the graph
//! navigable. It is deliberately dependency-light and free of I/O so it can be shared by
//! every other crate (parser, identity, storage, observer, risk, mcp).
//!
//!
//! # Overview
//! * [`TaskNode`] / [`ArtifactNode`] — the two node families.
//! * [`TaskVersion`] / [`ArtifactVersion`] — dated, append-only versions.
//! * [`Edge`] — the five typed edge kinds (two mandatory navigation axes).
//! * [`RiskState`] — two scores fused into one [`RiskColor`].
//! * Ids ([`ArtifactId`], [`TaskId`], [`VersionId`]) are deterministic and content-addressed.

pub mod attribution;
pub mod declared;
pub mod edge;
pub mod id;
pub mod node;
pub mod risk;
pub mod version;

use thiserror::Error;

/// A precise instant, always stored in UTC.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

/// Re-export of `chrono` so downstream crates share one version.
pub use chrono;

/// Errors produced by the data model (parsing/validation of model values).
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("unknown artifact level: {0}")]
    UnknownLevel(String),
}

// Flat re-exports of the most-used types, so callers can `use brain0_model::*` or import
// individually without deep paths.
pub use attribution::{Agent, Author};
pub use declared::{DeclaredChange, Drift};
pub use edge::{Edge, EdgeKind};
pub use id::{ArtifactId, PayloadRef, SessionId, TaskId, VersionId};
pub use node::{ArtifactNode, Lang, Level, TaskNode};
pub use risk::{AposterioriFactors, AprioriFactors, Rgb, RiskColor, RiskState, RiskTransition};
pub use version::{ArtifactVersion, ChangeKind, ChangeSource, ReadSecret, TaskVersion};
