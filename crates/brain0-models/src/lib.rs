//! Configurable local model providers for brain0.
//!
//! This crate is the authoritative source for default model selection:
//! * **Summarizer** for `decision_summary` — default `qwen3:4b` via Ollama.
//! * **Embedder** for semantic search — default `qwen3-embedding` (small) via Ollama, with
//!   `nomic-embed-text` as the low-end fallback and a deterministic offline embedder.
//!
//! Defaults live in [`ModelConfig`] (the config layer), overridable by file + `BRAIN0_*`
//! env vars without recompiling. Both providers operate only on **redacted** text.

pub mod config;
pub mod embed;
pub mod summarize;

pub use config::{EmbeddingConfig, ModelConfig, SummarizerConfig, DEFAULT_ENDPOINT};
pub use embed::{
    build_embedder, EmbeddingProvider, LocalEmbeddingProvider, OllamaEmbeddingProvider,
};
pub use summarize::{
    build_summarizer, DeterministicTextSummarizer, OllamaSummarizer, SummarizerProvider,
    SUMMARY_INSTRUCTION,
};

use thiserror::Error;

/// Errors from model providers.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("http error: {0}")]
    Http(String),
    #[error("decode error: {0}")]
    Decode(String),
    #[error("provider error: {0}")]
    Provider(String),
}

/// Convenience result type.
pub type Result<T> = std::result::Result<T, ModelError>;
