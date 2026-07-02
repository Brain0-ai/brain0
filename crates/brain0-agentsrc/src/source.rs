//! The `AgentArtifactSource` abstraction + a registry of adapters.

use std::path::PathBuf;

use crate::event::{IncrementalRead, SessionFile};
use crate::scope::ProjectScope;
use crate::Result;

/// An adapter that normalizes one coding agent's on-disk artifacts into brain0's neutral
/// event model. New agents are added by implementing this trait and registering it — no
/// change to the rest of the system.
pub trait AgentArtifactSource: std::fmt::Debug + Send {
    /// Stable adapter id (e.g. `"codex"`, `"claude-code"`); used as `source_adapter`.
    fn name(&self) -> &str;

    /// Discovered roots this adapter is reading from.
    fn roots(&self) -> &[PathBuf];

    /// Session files in scope (each resolved to its working directory for §5 filtering).
    fn sessions(&self, scope: &ProjectScope) -> Result<Vec<SessionFile>>;

    /// Read new turns from a session file starting at `from_offset` (§6.1 cursor).
    fn read_incremental(&self, session: &SessionFile, from_offset: u64) -> Result<IncrementalRead>;

    /// Persistent memory files pertinent to the scope.
    fn memory_files(&self, scope: &ProjectScope) -> Result<Vec<PathBuf>>;
}

/// A registry of active adapters.
#[derive(Debug, Default)]
pub struct SourceRegistry {
    sources: Vec<Box<dyn AgentArtifactSource>>,
}

impl SourceRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: Box<dyn AgentArtifactSource>) {
        self.sources.push(source);
    }

    #[must_use]
    pub fn sources(&self) -> &[Box<dyn AgentArtifactSource>] {
        &self.sources
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}
