//! Model configuration with overrides.
//!
//! Defaults live here in the config layer (not as constants buried in logic), so a model
//! can be changed via a config file or environment variables without recompiling. This PRD
//! is the authoritative source for the default model selection.

use serde::{Deserialize, Serialize};

/// Default local runtime endpoint (Ollama).
pub const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Summarizer model configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarizerConfig {
    /// `"ollama"` (default) or `"deterministic"` (offline, no model).
    pub provider: String,
    /// Model tag (profile: instruct, ~2–8B, code-aware, Apache-2.0/MIT).
    pub model: String,
    pub endpoint: String,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".to_owned(),
            model: "qwen3:4b".to_owned(),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
        }
    }
}

/// Embedding model configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    /// `"ollama"` (default), or `"local"` (offline feature-hashing fallback).
    pub provider: String,
    /// Model tag (profile: multilingual + code-aware embedding, Apache-2.0/MIT). Default is
    /// `qwen3-embedding` small; the official low-end fallback is `nomic-embed-text`.
    pub model: String,
    pub endpoint: String,
    /// Output dimension. Fixed per store; declared here and persisted in the store meta.
    pub dim: usize,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: "ollama".to_owned(),
            model: "qwen3-embedding:0.6b".to_owned(),
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            dim: 1024,
        }
    }
}

/// Top-level model configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub summarizer: SummarizerConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}

impl ModelConfig {
    /// Load config: defaults → optional JSON file → environment overrides.
    pub fn load(file: Option<&std::path::Path>) -> Self {
        let mut config = match file {
            Some(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            None => Self::default(),
        };
        config.apply_env();
        config
    }

    /// Apply `BRAIN0_*` environment overrides (highest precedence).
    pub fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("BRAIN0_SUMMARIZER_PROVIDER") {
            self.summarizer.provider = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_SUMMARIZER_MODEL") {
            self.summarizer.model = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_SUMMARIZER_ENDPOINT") {
            self.summarizer.endpoint = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_EMBED_PROVIDER") {
            self.embedding.provider = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_EMBED_MODEL") {
            self.embedding.model = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_EMBED_ENDPOINT") {
            self.embedding.endpoint = v;
        }
        if let Ok(v) = std::env::var("BRAIN0_EMBED_DIM") {
            if let Ok(d) = v.parse() {
                self.embedding.dim = d;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_prd4_models() {
        let c = ModelConfig::default();
        assert_eq!(c.summarizer.provider, "ollama");
        assert_eq!(c.summarizer.model, "qwen3:4b");
        assert_eq!(c.embedding.model, "qwen3-embedding:0.6b");
        assert_eq!(c.embedding.dim, 1024);
    }

    #[test]
    fn env_overrides_model_without_code_change() {
        std::env::set_var("BRAIN0_EMBED_MODEL", "nomic-embed-text");
        std::env::set_var("BRAIN0_EMBED_DIM", "768");
        std::env::set_var("BRAIN0_SUMMARIZER_MODEL", "qwen3:8b");
        let mut c = ModelConfig::default();
        c.apply_env();
        assert_eq!(c.embedding.model, "nomic-embed-text");
        assert_eq!(c.embedding.dim, 768);
        assert_eq!(c.summarizer.model, "qwen3:8b");
        std::env::remove_var("BRAIN0_EMBED_MODEL");
        std::env::remove_var("BRAIN0_EMBED_DIM");
        std::env::remove_var("BRAIN0_SUMMARIZER_MODEL");
    }
}
