//! Embedding providers. Default: `qwen3-embedding` (small) via Ollama;
//! offline fallback: deterministic feature-hashing. All operate on already-redacted text.

use brain0_storage::{local_embed, LOCAL_EMBED_DIM};

use crate::config::EmbeddingConfig;
use crate::{ModelError, Result};

/// Computes embeddings for the recency-aware search.
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// The fixed output dimension for this provider/store.
    fn dim(&self) -> usize;
    /// Stable model identifier (persisted in store meta).
    fn model_id(&self) -> &str;
}

/// Deterministic, offline embedding (feature hashing). The zero-dependency fallback that
/// keeps brain0 fully air-gapped.
#[derive(Debug, Clone)]
pub struct LocalEmbeddingProvider {
    dim: usize,
}

impl LocalEmbeddingProvider {
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for LocalEmbeddingProvider {
    fn default() -> Self {
        Self {
            dim: LOCAL_EMBED_DIM,
        }
    }
}

impl EmbeddingProvider for LocalEmbeddingProvider {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(local_embed(text, self.dim))
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        "local-feature-hash"
    }
}

/// Ollama-served embedding model (e.g. `qwen3-embedding:0.6b`, `nomic-embed-text`).
#[derive(Debug, Clone)]
pub struct OllamaEmbeddingProvider {
    model: String,
    endpoint: String,
    dim: usize,
}

impl OllamaEmbeddingProvider {
    #[must_use]
    pub fn new(model: impl Into<String>, endpoint: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            endpoint: endpoint.into(),
            dim,
        }
    }
}

impl OllamaEmbeddingProvider {
    /// POST to one of Ollama's two embedding APIs and pull the vector out of the response.
    fn request(&self, path: &str, body: serde_json::Value, field: &str) -> Result<Vec<f32>> {
        let url = format!("{}{path}", self.endpoint.trim_end_matches('/'));
        let response = ureq::post(&url)
            .send_json(body)
            .map_err(|e| ModelError::Http(e.to_string()))?;
        let value: serde_json::Value = response
            .into_json()
            .map_err(|e| ModelError::Decode(e.to_string()))?;
        // Modern `/api/embed` nests the vector: {"embeddings": [[…]]}; legacy is {"embedding": […]}.
        let array = match field {
            "embeddings" => value
                .get(field)
                .and_then(|v| v.as_array())
                .and_then(|batch| batch.first())
                .and_then(|v| v.as_array()),
            _ => value.get(field).and_then(|v| v.as_array()),
        }
        .ok_or_else(|| ModelError::Decode(format!("missing '{field}' in response")))?;
        Ok(array
            .iter()
            .filter_map(|n| n.as_f64().map(|f| f as f32))
            .collect())
    }
}

impl EmbeddingProvider for OllamaEmbeddingProvider {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Modern endpoint first (`/api/embed` — the only one newer embedding models reliably
        // serve), then the legacy `/api/embeddings` for older Ollama servers.
        let vector = self
            .request(
                "/api/embed",
                serde_json::json!({ "model": self.model, "input": text }),
                "embeddings",
            )
            .or_else(|_| {
                self.request(
                    "/api/embeddings",
                    serde_json::json!({ "model": self.model, "prompt": text }),
                    "embedding",
                )
            })?;
        if vector.len() != self.dim {
            return Err(ModelError::Provider(format!(
                "model '{}' returned dim {} but config declares {} — set BRAIN0_EMBED_DIM={} (and `brain0 reembed` if the store already has vectors)",
                self.model,
                vector.len(),
                self.dim,
                vector.len()
            )));
        }
        Ok(vector)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Build the configured embedding provider.
#[must_use]
pub fn build_embedder(config: &EmbeddingConfig) -> Box<dyn EmbeddingProvider> {
    match config.provider.as_str() {
        "local" | "deterministic" => Box::new(LocalEmbeddingProvider::new(config.dim)),
        _ => Box::new(OllamaEmbeddingProvider::new(
            config.model.clone(),
            config.endpoint.clone(),
            config.dim,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_provider_embeds_with_configured_dim() {
        let p = LocalEmbeddingProvider::new(128);
        let v = p.embed("redacted prompt text").unwrap();
        assert_eq!(v.len(), 128);
        assert_eq!(p.dim(), 128);
        assert_eq!(p.model_id(), "local-feature-hash");
    }

    #[test]
    fn build_embedder_selects_local() {
        let cfg = EmbeddingConfig {
            provider: "local".to_owned(),
            model: "x".to_owned(),
            endpoint: "".to_owned(),
            dim: 64,
        };
        assert_eq!(build_embedder(&cfg).dim(), 64);
    }

    #[test]
    fn ollama_provider_fails_when_unreachable() {
        // No Ollama at this port → error path (caller decides fallback). Not data.
        let p = OllamaEmbeddingProvider::new("qwen3-embedding:0.6b", "http://127.0.0.1:1", 1024);
        assert!(p.embed("text").is_err());
    }
}
