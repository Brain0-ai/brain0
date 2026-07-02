//! Cross-platform auto-discovery of agent home directories.
//!
//! No hardcoded paths: the user home is resolved portably (Linux/macOS `$HOME`, Windows
//! `%USERPROFILE%`, with WSL working naturally since brain0 runs under Linux there), each
//! adapter declares candidate paths relative to home, and existing ones are activated.
//! Overrides come from config/env and are additive.

use std::path::{Path, PathBuf};

/// Environment variable overriding the resolved home directory.
pub const ENV_HOME: &str = "BRAIN0_HOME";
/// Environment variable adding extra source roots (path-list separated by the OS separator).
pub const ENV_EXTRA_ROOTS: &str = "BRAIN0_AGENT_ROOTS";

/// Discovery configuration. All fields are optional; the common case needs none.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryConfig {
    /// Explicit home override (else env `BRAIN0_HOME`, else the OS home).
    pub home_override: Option<PathBuf>,
    /// Extra source roots to activate in addition to discovered ones.
    pub extra_roots: Vec<PathBuf>,
}

/// Resolve the home directory portably, honoring the override and env var.
#[must_use]
pub fn home_dir(config: &DiscoveryConfig) -> Option<PathBuf> {
    config
        .home_override
        .clone()
        .or_else(|| std::env::var_os(ENV_HOME).map(PathBuf::from))
        .or_else(dirs::home_dir)
}

/// Extra roots from config plus the `BRAIN0_AGENT_ROOTS` env var (existing ones only).
#[must_use]
pub fn extra_roots(config: &DiscoveryConfig) -> Vec<PathBuf> {
    let mut roots = config.extra_roots.clone();
    if let Some(value) = std::env::var_os(ENV_EXTRA_ROOTS) {
        roots.extend(std::env::split_paths(&value));
    }
    roots.into_iter().filter(|p| p.exists()).collect()
}

/// Probe `home`-relative candidate paths, returning those that exist.
#[must_use]
pub fn probe_candidates(home: &Path, relpaths: &[&str]) -> Vec<PathBuf> {
    relpaths
        .iter()
        .map(|rel| home.join(rel))
        .filter(|p| p.exists())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_override_takes_precedence() {
        let cfg = DiscoveryConfig {
            home_override: Some(PathBuf::from("/custom/home")),
            ..DiscoveryConfig::default()
        };
        assert_eq!(home_dir(&cfg), Some(PathBuf::from("/custom/home")));
    }

    #[test]
    fn probe_returns_only_existing_candidates() {
        let tmp = std::env::temp_dir().join(format!("brain0-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".codex")).unwrap();
        let found = probe_candidates(&tmp, &[".codex", ".claude/projects"]);
        assert_eq!(found, vec![tmp.join(".codex")]);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
