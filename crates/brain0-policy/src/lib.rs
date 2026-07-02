//! DLP / egress policy engine (`docs/governance.md`).
//!
//! Pure and I/O-free: given a [`Policy`] and a [`ReadEvent`] (a file an agent read, with whether it
//! is out-of-repo, any secret kinds detected in its content, and whether a remote model was in
//! use), it returns the [`Violation`]s. Callers (the `guard` command, a live daemon, a pre-flight
//! gate) decide how to surface them. brain0 observes transcripts *after* the fact, so this is a
//! **detective** control — it tells you a secret/sensitive file reached the (often remote) model.

#![forbid(unsafe_code)]

use regex::Regex;

/// How serious a policy violation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Critical,
}

impl Severity {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
        }
    }
}

/// Default sensitive-path globs (secrets, keys, cloud creds) when none are configured.
pub const DEFAULT_SENSITIVE_GLOBS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa",
    "**/id_ed25519",
    "**/credentials",
    "**/.aws/**",
    "**/.ssh/**",
    "**/secrets/**",
    "**/*secret*",
];

/// Environment variable holding extra/override sensitive globs (OS path-list separated).
pub const ENV_DLP_GLOBS: &str = "BRAIN0_DLP_GLOBS";

/// One observed file read, normalized for policy evaluation.
#[derive(Debug, Clone)]
pub struct ReadEvent {
    /// Repo-relative path (in-repo) or absolute path (out-of-repo).
    pub path: String,
    /// True when the path is outside the observed repo (absolute) — an audit red flag.
    pub external: bool,
    /// Secret KINDS detected in the read content (e.g. `aws_access_key`); empty if none/unknown.
    pub secret_kinds: Vec<String>,
    /// Whether a REMOTE model received this read (true for cloud agents like Claude Code / Codex).
    pub remote: bool,
}

/// A flagged read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    /// Which rule fired: `secret-in-read` | `sensitive-path` | `external-read`.
    pub rule: String,
    pub severity: Severity,
    pub detail: String,
}

/// Compiled DLP policy.
#[derive(Debug)]
pub struct Policy {
    sensitive: Vec<Regex>,
    /// Flag out-of-repo reads even if they match no sensitive glob.
    pub flag_external: bool,
    /// Only evaluate reads that reached a remote model (the egress case). Local-only stays silent.
    pub require_remote: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(
            DEFAULT_SENSITIVE_GLOBS.iter().map(|s| (*s).to_owned()),
            true,
            true,
        )
    }
}

impl Policy {
    /// Build from explicit globs + flags. Invalid globs are skipped (never panics).
    pub fn new(
        globs: impl IntoIterator<Item = String>,
        flag_external: bool,
        require_remote: bool,
    ) -> Self {
        let sensitive = globs
            .into_iter()
            .filter_map(|g| Regex::new(&glob_to_regex(&g)).ok())
            .collect();
        Self {
            sensitive,
            flag_external,
            require_remote,
        }
    }

    /// Build from the environment: `BRAIN0_DLP_GLOBS` overrides the default glob set; flags on.
    #[must_use]
    pub fn from_env() -> Self {
        let globs: Vec<String> = std::env::var_os(ENV_DLP_GLOBS)
            .map(|raw| {
                std::env::split_paths(&raw)
                    .filter_map(|p| p.to_str().map(str::to_owned))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| {
                DEFAULT_SENSITIVE_GLOBS
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect()
            });
        Self::new(globs, true, true)
    }

    fn matches_sensitive(&self, path: &str) -> bool {
        let base = path.rsplit('/').next().unwrap_or(path);
        self.sensitive
            .iter()
            .any(|re| re.is_match(path) || re.is_match(base))
    }
}

/// Evaluate one read against the policy. A read whose content held a secret is `critical`; a
/// sensitive-glob or out-of-repo read is `warn`. Nothing fires for local-only reads when
/// `require_remote` is set (no egress happened).
#[must_use]
pub fn evaluate(policy: &Policy, ev: &ReadEvent) -> Vec<Violation> {
    let mut out = Vec::new();
    if policy.require_remote && !ev.remote {
        return out;
    }
    for kind in &ev.secret_kinds {
        out.push(Violation {
            path: ev.path.clone(),
            rule: "secret-in-read".to_owned(),
            severity: Severity::Critical,
            detail: format!("secret [{kind}] in a file read by a remote-model agent"),
        });
    }
    if policy.matches_sensitive(&ev.path) {
        out.push(Violation {
            path: ev.path.clone(),
            rule: "sensitive-path".to_owned(),
            severity: Severity::Warn,
            detail: "matched a sensitive-path policy glob".to_owned(),
        });
    }
    if policy.flag_external && ev.external {
        out.push(Violation {
            path: ev.path.clone(),
            rule: "external-read".to_owned(),
            severity: Severity::Warn,
            detail: "read of a file outside the repository".to_owned(),
        });
    }
    out
}

/// Translate a glob (`**`, `*`, `?`) into an anchored regex. `**/` matches zero or more leading
/// path segments; `*` stays within a segment; `?` is a single non-`/` char.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?"); // `**/` → optional leading segments
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' | '|' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(path: &str, external: bool, kinds: &[&str], remote: bool) -> ReadEvent {
        ReadEvent {
            path: path.to_owned(),
            external,
            secret_kinds: kinds.iter().map(|s| (*s).to_owned()).collect(),
            remote,
        }
    }

    #[test]
    fn sensitive_globs_match_paths_and_basenames() {
        let p = Policy::default();
        assert!(p.matches_sensitive(".env"));
        assert!(p.matches_sensitive("packages/server/.env"));
        assert!(p.matches_sensitive("config/prod.pem"));
        assert!(p.matches_sensitive("/home/u/.aws/credentials"));
        assert!(!p.matches_sensitive("packages/gui/src/main.ts"));
    }

    #[test]
    fn secret_in_read_is_critical() {
        let v = evaluate(
            &Policy::default(),
            &ev("config/app.ts", false, &["aws_access_key"], true),
        );
        assert!(v
            .iter()
            .any(|x| x.rule == "secret-in-read" && x.severity == Severity::Critical));
    }

    #[test]
    fn sensitive_and_external_reads_are_flagged() {
        let p = Policy::default();
        let v = evaluate(&p, &ev("/home/u/.ssh/id_rsa", true, &[], true));
        assert!(v.iter().any(|x| x.rule == "sensitive-path"));
        assert!(v.iter().any(|x| x.rule == "external-read"));
    }

    #[test]
    fn ordinary_in_repo_read_is_clean() {
        assert!(evaluate(&Policy::default(), &ev("src/main.ts", false, &[], true)).is_empty());
    }

    #[test]
    fn local_only_reads_are_not_flagged_when_require_remote() {
        // Same risky read, but no remote model received it → no egress → no violation.
        assert!(evaluate(
            &Policy::default(),
            &ev("/home/u/.aws/credentials", true, &["aws_access_key"], false)
        )
        .is_empty());
    }

    #[test]
    fn env_globs_override() {
        let p = Policy::new(["**/topsecret.txt".to_owned()], false, true);
        assert!(p.matches_sensitive("a/b/topsecret.txt"));
        assert!(!p.matches_sensitive("a/b/.env")); // default set replaced
    }
}
