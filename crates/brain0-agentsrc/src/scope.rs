//! Project scoping: observe only the sessions of the current project.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Normalize a path for comparison: canonicalize if it exists (resolving symlinks), else
/// resolve `.`/`..` lexically. Works cross-platform.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve the current project's root: the git toplevel if `cwd` is in a repo, else `cwd`.
#[must_use]
pub fn resolve_project_root(cwd: &Path) -> PathBuf {
    if let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        if output.status.success() {
            let top = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !top.is_empty() {
                return normalize(Path::new(&top));
            }
        }
    }
    normalize(cwd)
}

/// Which sessions to include.
#[derive(Debug, Clone)]
pub enum ProjectScope {
    /// Only sessions whose cwd is the project root or under it (the default).
    Project(PathBuf),
    /// Every session, for global audit.
    All,
}

impl ProjectScope {
    /// Scope to a project root (normalized).
    #[must_use]
    pub fn project(root: &Path) -> Self {
        ProjectScope::Project(normalize(root))
    }

    /// Whether a session with working directory `cwd` is in scope.
    #[must_use]
    pub fn includes(&self, cwd: &Path) -> bool {
        match self {
            ProjectScope::All => true,
            ProjectScope::Project(root) => {
                let normalized = normalize(cwd);
                normalized == *root || normalized.starts_with(root)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_scope_includes_self_and_subdirs_only() {
        let scope = ProjectScope::project(Path::new("/home/u/proj"));
        assert!(scope.includes(Path::new("/home/u/proj")));
        assert!(scope.includes(Path::new("/home/u/proj/sub/pkg")));
        assert!(!scope.includes(Path::new("/home/u/proj2"))); // sibling with shared prefix
        assert!(!scope.includes(Path::new("/home/u/other")));
    }

    #[test]
    fn all_scope_includes_everything() {
        assert!(ProjectScope::All.includes(Path::new("/anywhere/at/all")));
    }

    #[test]
    fn normalize_resolves_dot_segments() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
    }

    #[test]
    fn resolve_project_root_uses_git_toplevel_else_cwd() {
        // Non-git temp dir → cwd itself.
        let tmp = std::env::temp_dir().join(format!("brain0-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(resolve_project_root(&tmp), normalize(&tmp));

        // Git repo → toplevel even from a subdir.
        let ok = Command::new("git")
            .arg("-C")
            .arg(&tmp)
            .args(["init", "-q"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let sub = tmp.join("a/b");
            std::fs::create_dir_all(&sub).unwrap();
            assert_eq!(resolve_project_root(&sub), normalize(&tmp));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
