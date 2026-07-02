//! Privacy: secret-scanning + exclusion, applied at ingest.
//!
//! Secret scanning is **on by default** (secure-by-default): the [`Redactor`] runs the
//! [`SecretScanner`] over prompts/assistant text before anything is written to the payload
//! store or embedded, so a leaked secret never reaches T1/T2 in clear. It also skips whole
//! sessions whose cwd/path matches an exclusion glob. Built-in detectors can be disabled
//! explicitly; user patterns can be added.

use regex::Regex;

use crate::event::Turn;
use crate::secret::SecretScanner;
use crate::{AgentSrcError, Result};

/// Environment variable holding exclusion globs (OS path-list separated).
pub const ENV_EXCLUDE: &str = "BRAIN0_EXCLUDE";
/// Environment variable holding extra secret regex patterns (comma-separated).
pub const ENV_REDACT: &str = "BRAIN0_REDACT";
/// Set to disable the built-in secret detectors (explicit opt-out; secure default keeps them).
pub const ENV_DISABLE_SCAN: &str = "BRAIN0_DISABLE_SECRET_SCAN";

/// Redaction configuration (from a config file or env).
#[derive(Debug, Clone, Default)]
pub struct RedactionConfig {
    /// Glob patterns; a session whose cwd/path matches any is not ingested.
    pub exclude_globs: Vec<String>,
    /// Extra secret regex patterns (in addition to the built-in detectors).
    pub redact_patterns: Vec<String>,
    /// Explicit opt-out of the built-in secret detectors (default: keep them on).
    pub disable_builtin_scanners: bool,
}

impl RedactionConfig {
    /// Build from environment variables (additive convenience over a config file).
    #[must_use]
    pub fn from_env() -> Self {
        let exclude_globs = std::env::var_os(ENV_EXCLUDE)
            .map(|v| {
                std::env::split_paths(&v)
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let redact_patterns = std::env::var(ENV_REDACT)
            .ok()
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let disable_builtin_scanners = std::env::var_os(ENV_DISABLE_SCAN).is_some();
        Self {
            exclude_globs,
            redact_patterns,
            disable_builtin_scanners,
        }
    }
}

/// Translate a glob (`*`, `?`) into an anchored regex.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c if ".+()|[]{}^$\\".contains(c) => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    re.push('$');
    re
}

/// Applies secret-scanning + exclusion before any persistence.
#[derive(Debug, Default)]
pub struct Redactor {
    excludes: Vec<Regex>,
    scanner: Option<SecretScanner>,
}

impl Redactor {
    /// A no-op redactor (tests only — production must scan).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Compile a redactor from configuration. Built-in secret detectors are on unless
    /// explicitly disabled (secure-by-default).
    pub fn new(config: &RedactionConfig) -> Result<Self> {
        let excludes = config
            .exclude_globs
            .iter()
            .map(|g| Regex::new(&glob_to_regex(g)).map_err(|e| AgentSrcError::Parse(e.to_string())))
            .collect::<Result<Vec<_>>>()?;
        let scanner = if config.disable_builtin_scanners {
            SecretScanner::custom(&config.redact_patterns)?
        } else {
            SecretScanner::with_builtins(&config.redact_patterns)?
        };
        Ok(Self {
            excludes,
            scanner: Some(scanner),
        })
    }

    /// Whether a path (session cwd or file) is excluded from ingest.
    #[must_use]
    pub fn is_excluded(&self, path: &str) -> bool {
        self.excludes.iter().any(|re| re.is_match(path))
    }

    /// Redact secrets from free text.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        match &self.scanner {
            Some(scanner) => scanner.redact(text),
            None => text.to_owned(),
        }
    }

    /// Distinct secret KINDS detected in `text` (never the value) — for the DLP audit trail of
    /// what reached the model. Empty when scanning is disabled.
    #[must_use]
    pub fn scan_kinds(&self, text: &str) -> Vec<String> {
        let Some(scanner) = &self.scanner else {
            return Vec::new();
        };
        let (_, events) = scanner.scan(text, None);
        let mut kinds: Vec<String> = events.into_iter().map(|e| e.kind).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    /// A copy of the turn with prompt + assistant text redacted (declared paths kept as-is).
    #[must_use]
    pub fn redact_turn(&self, turn: &Turn) -> Turn {
        self.redact_turn_audited(turn).0
    }

    /// Like [`redact_turn`](Self::redact_turn) but also returns the redaction events, so the
    /// caller can write them to the audit log (kind + provenance only, never the value).
    #[must_use]
    pub fn redact_turn_audited(&self, turn: &Turn) -> (Turn, Vec<crate::secret::RedactionEvent>) {
        let Some(scanner) = &self.scanner else {
            return (turn.clone(), Vec::new());
        };
        let file = turn.provenance.file.as_str();
        let mut events = Vec::new();
        let mut redacted = turn.clone();
        if let Some(prompt) = &turn.prompt {
            let (text, mut evs) = scanner.scan(prompt, Some(file));
            redacted.prompt = Some(text);
            events.append(&mut evs);
        }
        let (text, mut evs) = scanner.scan(&turn.assistant_text, Some(file));
        redacted.assistant_text = text;
        events.append(&mut evs);
        (redacted, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Provenance;
    use std::path::PathBuf;

    fn turn(prompt: &str) -> Turn {
        use chrono::TimeZone;
        Turn {
            session_id: "s".into(),
            cwd: PathBuf::from("/p"),
            timestamp: chrono::Utc.timestamp_opt(0, 0).single().unwrap(),
            ordinal: 0,
            prompt: Some(prompt.into()),
            assistant_text: String::new(),
            model: None,
            tool_calls: Vec::new(),
            provenance: Provenance {
                adapter: "codex".into(),
                file: "f".into(),
                byte_offset: 0,
            },
            read_contents: Vec::new(),
        }
    }

    #[test]
    fn excludes_by_glob() {
        let r = Redactor::new(&RedactionConfig {
            exclude_globs: vec!["*/secret-project".into(), "*/.ssh/*".into()],
            ..RedactionConfig::default()
        })
        .unwrap();
        assert!(r.is_excluded("/home/u/secret-project"));
        assert!(r.is_excluded("/home/u/.ssh/config"));
        assert!(!r.is_excluded("/home/u/normal"));
    }

    #[test]
    fn builtin_scanning_is_on_by_default() {
        let r = Redactor::new(&RedactionConfig::default()).unwrap();
        let t = r.redact_turn(&turn("token AKIAIOSFODNN7EXAMPLE here"));
        assert!(t
            .prompt
            .as_deref()
            .unwrap()
            .contains("[REDACTED:aws_access_key]"));
        assert!(!t
            .prompt
            .as_deref()
            .unwrap()
            .contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn user_pattern_is_applied() {
        let r = Redactor::new(&RedactionConfig {
            redact_patterns: vec![r"INTERNAL-[0-9]+".into()],
            ..RedactionConfig::default()
        })
        .unwrap();
        assert_eq!(
            r.redact("ref INTERNAL-42 ok"),
            "ref [REDACTED:user_pattern] ok"
        );
    }

    #[test]
    fn empty_redactor_is_noop() {
        let r = Redactor::empty();
        assert!(!r.is_excluded("/anything"));
        assert_eq!(
            r.redact("token AKIAIOSFODNN7EXAMPLE"),
            "token AKIAIOSFODNN7EXAMPLE"
        );
    }
}
