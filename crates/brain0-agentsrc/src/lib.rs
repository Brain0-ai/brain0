//! Passive ingest of coding-agent artifacts for brain0 (Mandate B).
//!
//! brain0 is a pure observer: agents need not cooperate. This crate reads the transcripts
//! and memory that agents already write to disk (Codex, Claude Code, …), normalizes them
//! into brain0's event model, and feeds the *declared/intent* side of the graph — the
//! counterpart to the git/checkpoint *fact* side. Ingest is economical: append-only
//! per-file cursors, deterministic Rust extraction, and per-turn cached summaries
//!.

pub mod claude;
pub mod codex;
pub mod discovery;
pub mod driver;
pub mod event;
mod jsonl;
pub mod redact;
pub mod scope;
pub mod secret;
pub mod source;
pub mod summarize;

pub use claude::ClaudeSource;
pub use codex::CodexSource;
pub use discovery::DiscoveryConfig;
pub use driver::{run_ingest, run_ingest_reporting, IngestProgress, IngestStats, NoProgress};
pub use event::{IncrementalRead, Provenance, SessionFile, ToolCall, Turn};
pub use redact::{RedactionConfig, Redactor};
pub use scope::{resolve_project_root, ProjectScope};
pub use secret::{RedactionEvent, SecretScanner};
pub use source::{AgentArtifactSource, SourceRegistry};
pub use summarize::{
    session_summary, summarize_cached, DeterministicSummarizer, InMemorySummaryCache,
    LlmTurnSummarizer, PersistentSummaryCache, SummaryCache, TurnSummarizer,
};

use thiserror::Error;

/// Auto-discover and register the available adapters (Codex, Claude Code) under the
/// resolved home, plus any configured/env extra roots.
#[must_use]
pub fn discover(config: &DiscoveryConfig) -> SourceRegistry {
    let mut registry = SourceRegistry::new();
    if let Some(home) = discovery::home_dir(config) {
        for root in discovery::probe_candidates(&home, &[".codex"]) {
            registry.register(Box::new(CodexSource::new(root)));
        }
        for root in discovery::probe_candidates(&home, &[".claude/projects"]) {
            registry.register(Box::new(ClaudeSource::new(root)));
        }
    }
    for root in discovery::extra_roots(config) {
        // Type the extra root by structure: a `sessions/` dir means Codex, else Claude.
        if root.join("sessions").exists() {
            registry.register(Box::new(CodexSource::new(root)));
        } else {
            registry.register(Box::new(ClaudeSource::new(root)));
        }
    }
    registry
}

/// Errors produced by the agent-artifact ingest.
#[derive(Debug, Error)]
pub enum AgentSrcError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Storage(#[from] brain0_storage::StorageError),
    #[error(transparent)]
    Reconcile(#[from] brain0_reconcile::ReconcileError),
    #[error(transparent)]
    Model(#[from] brain0_models::ModelError),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, AgentSrcError>;
