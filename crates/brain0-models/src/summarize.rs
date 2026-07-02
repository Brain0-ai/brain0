//! Summarizer providers. Default: `qwen3:4b` via Ollama, with a short,
//! constrained output. Operates on already-redacted text. The fail-safe fallback to a
//! deterministic summary lives in `brain0-agentsrc` so the ingest is never blocked.

use crate::config::SummarizerConfig;
use crate::{ModelError, Result};

/// The instruction that constrains the model to a short, fixed-shape decision summary.
/// Structured (JSON) output is requested so format does not drift over large volumes
///.
pub const SUMMARY_INSTRUCTION: &str =
    "Summarize the developer's intent and scope for this single turn in ONE short sentence \
     (intent + what was touched). Respond ONLY with JSON of the form \
     {\"summary\": \"<one sentence>\"}. No preamble, no code, no markdown.";

/// Condenses already-redacted text into a short decision summary.
pub trait SummarizerProvider: Send + Sync {
    fn summarize(&self, instruction: &str, input: &str) -> Result<String>;
    fn model_id(&self) -> &str;
}

/// Deterministic, offline text summarizer: the first non-empty line, truncated.
#[derive(Debug, Clone, Default)]
pub struct DeterministicTextSummarizer;

impl SummarizerProvider for DeterministicTextSummarizer {
    fn summarize(&self, _instruction: &str, input: &str) -> Result<String> {
        let line = input
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        Ok(line.chars().take(200).collect())
    }
    fn model_id(&self) -> &str {
        "deterministic"
    }
}

/// Ollama-served instruct model (e.g. `qwen3:4b`).
#[derive(Debug, Clone)]
pub struct OllamaSummarizer {
    model: String,
    endpoint: String,
}

impl OllamaSummarizer {
    #[must_use]
    pub fn new(model: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            endpoint: endpoint.into(),
        }
    }
}

impl SummarizerProvider for OllamaSummarizer {
    fn summarize(&self, instruction: &str, input: &str) -> Result<String> {
        let url = format!("{}/api/generate", self.endpoint.trim_end_matches('/'));
        let response = ureq::post(&url)
            .send_json(serde_json::json!({
                "model": self.model,
                "prompt": format!("{instruction}\n\n{input}"),
                "stream": false,
                // Disable chain-of-thought (thinking models) and force JSON so the output is
                // a clean, short summary, not reasoning or preamble.
                "think": false,
                "format": "json",
                "options": { "num_predict": 200, "temperature": 0.1 }
            }))
            .map_err(|e| ModelError::Http(e.to_string()))?;
        let value: serde_json::Value = response
            .into_json()
            .map_err(|e| ModelError::Decode(e.to_string()))?;
        let raw = value
            .get("response")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModelError::Decode("missing 'response'".to_owned()))?;
        // Prefer the structured {"summary": "..."}; fall back to the raw text if the model
        // didn't honor the JSON shape.
        let summary = serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|j| j.get("summary").and_then(|s| s.as_str()).map(str::to_owned))
            .unwrap_or_else(|| raw.trim().to_owned());
        Ok(summary)
    }
    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Build the configured summarizer provider.
#[must_use]
pub fn build_summarizer(config: &SummarizerConfig) -> Box<dyn SummarizerProvider> {
    match config.provider.as_str() {
        "deterministic" => Box::new(DeterministicTextSummarizer),
        _ => Box::new(OllamaSummarizer::new(
            config.model.clone(),
            config.endpoint.clone(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_text_summary_is_short_first_line() {
        let s = DeterministicTextSummarizer
            .summarize(SUMMARY_INSTRUCTION, "fix the parser\nlots of detail here")
            .unwrap();
        assert_eq!(s, "fix the parser");
    }

    #[test]
    fn ollama_summarizer_fails_when_unreachable() {
        let p = OllamaSummarizer::new("qwen3:4b", "http://127.0.0.1:1");
        assert!(p.summarize(SUMMARY_INSTRUCTION, "hello").is_err());
    }
}
