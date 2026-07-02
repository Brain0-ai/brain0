//! Secret scanning + redaction at ingest (, default-on).
//!
//! This runs **before** any payload is written and before any summary/embedding is computed
//! (the driver redacts each turn first), so a tier-0 secret can never be persisted in clear
//! nor leak through an invertible embedding. Detectors combine known patterns with an
//! entropy heuristic, and are extensible with user patterns. Detected secrets become typed
//! placeholders (`[REDACTED:<kind>]`) that preserve the surrounding text; a redaction
//! **event** records the kind and provenance — never the secret value.

use regex::{Captures, Regex};

use crate::{AgentSrcError, Result};

/// A record that a secret was redacted (no value, ever).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionEvent {
    pub kind: String,
    pub file: Option<String>,
}

enum Mode {
    /// Replace the whole match with the placeholder.
    Whole,
    /// Replace only capture group `n` (keep the surrounding structure).
    Group(usize),
    /// Replace the match only if it is high-entropy.
    Entropy,
}

struct Detector {
    kind: &'static str,
    re: Regex,
    mode: Mode,
}

/// Scans text for secrets and redacts them.
pub struct SecretScanner {
    detectors: Vec<Detector>,
    extra: Vec<Detector>,
}

impl std::fmt::Debug for SecretScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretScanner")
            .field("builtin", &self.detectors.len())
            .field("extra", &self.extra.len())
            .finish()
    }
}

/// Shannon entropy in bits/char.
fn entropy(s: &str) -> f64 {
    let len = s.chars().count() as f64;
    if len == 0.0 {
        return 0.0;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0u32) += 1;
    }
    counts
        .values()
        .map(|&n| {
            let p = f64::from(n) / len;
            -p * p.log2()
        })
        .sum()
}

/// Heuristic: a long, mixed token with high entropy is likely a secret/token.
fn is_high_entropy(token: &str) -> bool {
    token.len() >= 32
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && entropy(token) >= 4.0
}

fn builtin_detectors() -> Vec<Detector> {
    // Order matters: most specific / multiline first.
    let specs: &[(&str, &str, Mode)] = &[
        (
            "private_key",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            Mode::Whole,
        ),
        (
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b",
            Mode::Whole,
        ),
        (
            "anthropic_key",
            r"\bsk-ant-[A-Za-z0-9_-]{20,}\b",
            Mode::Whole,
        ),
        (
            "openai_key",
            r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}\b",
            Mode::Whole,
        ),
        ("aws_access_key", r"\bAKIA[0-9A-Z]{16}\b", Mode::Whole),
        ("gcp_api_key", r"\bAIza[0-9A-Za-z_-]{35}\b", Mode::Whole),
        (
            "github_token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,})\b",
            Mode::Whole,
        ),
        (
            "slack_token",
            r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b",
            Mode::Whole,
        ),
        // user:password@host → redact the password only.
        (
            "url_credentials",
            r"://[^:@/\s]+:([^@/\s]+)@",
            Mode::Group(1),
        ),
        // KEY=value / TOKEN: "value" → redact the value.
        (
            "env_secret",
            r#"(?i)\b([a-z0-9_]*(?:key|token|secret|password|passwd|pwd|credential)[a-z0-9_]*)\s*[:=]\s*["']?([^"'\s]{6,})["']?"#,
            Mode::Group(2),
        ),
        // Generic high-entropy token (checked by predicate).
        ("high_entropy", r"[A-Za-z0-9+/=_\-]{32,}", Mode::Entropy),
    ];
    specs
        .iter()
        .map(|(kind, pat, mode)| Detector {
            kind,
            re: Regex::new(pat).expect("valid builtin secret pattern"),
            mode: match mode {
                Mode::Whole => Mode::Whole,
                Mode::Group(g) => Mode::Group(*g),
                Mode::Entropy => Mode::Entropy,
            },
        })
        .collect()
}

impl SecretScanner {
    /// Builder with all built-in detectors plus optional user regex patterns.
    pub fn with_builtins(extra_patterns: &[String]) -> Result<Self> {
        let extra = extra_patterns
            .iter()
            .map(|p| {
                Ok(Detector {
                    kind: "user_pattern",
                    re: Regex::new(p).map_err(|e| AgentSrcError::Parse(e.to_string()))?,
                    mode: Mode::Whole,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            detectors: builtin_detectors(),
            extra,
        })
    }

    /// Builder with **only** user patterns (built-ins explicitly disabled — secure default
    /// is to keep them; this is the explicit opt-out).
    pub fn custom(extra_patterns: &[String]) -> Result<Self> {
        let extra = extra_patterns
            .iter()
            .map(|p| {
                Ok(Detector {
                    kind: "user_pattern",
                    re: Regex::new(p).map_err(|e| AgentSrcError::Parse(e.to_string()))?,
                    mode: Mode::Whole,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            detectors: Vec::new(),
            extra,
        })
    }

    /// Scan and redact `text`, returning the redacted text and the redaction events.
    #[must_use]
    pub fn scan(&self, text: &str, file: Option<&str>) -> (String, Vec<RedactionEvent>) {
        let mut out = text.to_owned();
        let mut events = Vec::new();
        for detector in self.detectors.iter().chain(self.extra.iter()) {
            out = apply(detector, &out, file, &mut events);
        }
        (out, events)
    }

    /// Convenience: redacted text only.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.scan(text, None).0
    }
}

fn apply(
    detector: &Detector,
    text: &str,
    file: Option<&str>,
    events: &mut Vec<RedactionEvent>,
) -> String {
    let kind = detector.kind;
    let placeholder = format!("[REDACTED:{kind}]");
    match detector.mode {
        Mode::Whole => detector
            .re
            .replace_all(text, |_: &Captures| {
                events.push(RedactionEvent {
                    kind: kind.to_owned(),
                    file: file.map(str::to_owned),
                });
                placeholder.clone()
            })
            .into_owned(),
        Mode::Group(group) => detector
            .re
            .replace_all(text, |caps: &Captures| {
                let whole = caps.get(0).expect("group 0");
                let Some(g) = caps.get(group) else {
                    return whole.as_str().to_owned();
                };
                events.push(RedactionEvent {
                    kind: kind.to_owned(),
                    file: file.map(str::to_owned),
                });
                let start = g.start() - whole.start();
                let end = g.end() - whole.start();
                let w = whole.as_str();
                format!("{}{placeholder}{}", &w[..start], &w[end..])
            })
            .into_owned(),
        Mode::Entropy => detector
            .re
            .replace_all(text, |caps: &Captures| {
                let token = &caps[0];
                if is_high_entropy(token) {
                    events.push(RedactionEvent {
                        kind: kind.to_owned(),
                        file: file.map(str::to_owned),
                    });
                    placeholder.clone()
                } else {
                    token.to_owned()
                }
            })
            .into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner() -> SecretScanner {
        SecretScanner::with_builtins(&[]).unwrap()
    }

    /// A corpus of FAKE secrets (never real). §3.4 acceptance: none survive in clear.
    // Literals are SPLIT via concat! so no real-token-shaped string ever appears contiguously
    // in the source blob: GitHub push protection (correctly) blocks pushes containing anything
    // that looks like a live token, and cannot know these are fakes. Runtime values are intact.
    const FAKE_SECRETS: &[(&str, &str)] = &[
        (
            "openai",
            concat!("sk-proj-", "abcDEF0123456789abcDEF0123456789ZZ"),
        ),
        (
            "anthropic",
            concat!("sk-ant-", "api03-abcDEF0123456789abcDEF0123"),
        ),
        ("aws", "AKIAIOSFODNN7EXAMPLE"), // AWS's own documented example key
        (
            "gcp",
            concat!("AIza", "SyA0123456789abcdefghijklmnopqrstuvw"),
        ),
        (
            "github",
            concat!("ghp_", "0123456789abcdefghijklmnopqrstuvwxyz"),
        ),
        ("slack", concat!("xoxb-", "123456789012-abcdefghijklmnop")),
    ];

    #[test]
    fn redacts_all_known_secret_kinds() {
        let s = scanner();
        for (label, secret) in FAKE_SECRETS {
            let text = format!("here is the {label} key: {secret} use it");
            let (redacted, events) = s.scan(&text, Some("f.jsonl"));
            assert!(
                !redacted.contains(secret),
                "{label} secret survived: {redacted}"
            );
            assert!(
                redacted.contains("[REDACTED:"),
                "no placeholder for {label}"
            );
            assert!(!events.is_empty());
            assert!(events.iter().all(|e| e.file.as_deref() == Some("f.jsonl")));
        }
    }

    #[test]
    fn redacts_private_key_block() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nABCdef123/+==\nmorestuff==\n-----END OPENSSH PRIVATE KEY-----";
        let (redacted, _) = scanner().scan(pem, None);
        assert_eq!(redacted, "[REDACTED:private_key]");
    }

    #[test]
    fn redacts_url_password_keeping_structure() {
        let (redacted, _) = scanner().scan("db at postgres://user:s3cretP4ss@host:5432/db", None);
        assert!(redacted.contains("postgres://user:[REDACTED:url_credentials]@host"));
        assert!(!redacted.contains("s3cretP4ss"));
    }

    #[test]
    fn redacts_env_assignment_value_only() {
        let (redacted, _) = scanner().scan("API_TOKEN=supersecretvalue123", None);
        assert!(redacted.starts_with("API_TOKEN="));
        assert!(redacted.contains("[REDACTED:env_secret]"));
        assert!(!redacted.contains("supersecretvalue123"));
    }

    #[test]
    fn keeps_ordinary_text_intact() {
        let text = "Refactor the parser to handle nested brackets in lib.rs";
        assert_eq!(scanner().redact(text), text);
    }

    #[test]
    fn entropy_redacts_long_random_token() {
        // 40 random-looking mixed chars → high entropy.
        let token = "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8S9t0";
        let (redacted, _) = scanner().scan(&format!("value {token} end"), None);
        assert!(!redacted.contains(token));
    }
}
