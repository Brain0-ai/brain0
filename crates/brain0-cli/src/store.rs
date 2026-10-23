//! Where brain0 keeps its local state.
//!
//! The index and payload default to `.brain0/` in the current directory, which is usually the
//! observed repository's working tree. Two rules keep that state out of the repository:
//!
//! * a `.brain0/` directory ignores itself (a `.gitignore` containing `*`), so `git add -A`
//!   never stages it;
//! * key material never lives there: keys default to `<keys dir>/<store id>/<name>`, where the
//!   keys dir is `BRAIN0_KEYS_DIR` or `<user data dir>/brain0/keys`.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use anyhow::{bail, Context, Result};

/// Directory name of brain0's local store.
const STORE_DIR: &str = ".brain0";
/// Overrides the directory holding per-store key files.
const ENV_KEYS_DIR: &str = "BRAIN0_KEYS_DIR";
/// Payload KEK file name.
pub const PAYLOAD_KEY: &str = "keyring.key";
/// Attestation signing-key file name.
pub const ATTEST_KEY: &str = "attest.key";

const GITIGNORE: &str = "# Created by brain0: local state, never commit it.\n*\n";

static TRACKED_CHECK: Once = Once::new();

/// Make the `.brain0` directory containing `path` (if any) exist and ignore itself, and warn
/// once per run if git already tracks files in it.
pub fn ensure_self_ignored(path: &Path) -> Result<()> {
    let Some(dir) = path
        .ancestors()
        .find(|a| a.file_name() == Some(OsStr::new(STORE_DIR)))
    else {
        return Ok(());
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let gitignore = dir.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&gitignore)
    {
        Ok(mut file) => file.write_all(GITIGNORE.as_bytes())?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("writing {}", gitignore.display())),
    }
    TRACKED_CHECK.call_once(|| warn_if_tracked(dir));
    Ok(())
}

/// A `.gitignore` cannot untrack files committed before it existed (brain0 ≤ 0.1.1): say so.
fn warn_if_tracked(dir: &Path) {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--", "."])
        .output()
    else {
        return;
    };
    if out.status.success() && !out.stdout.is_empty() {
        eprintln!(
            "warning: git tracks files in {d} (index, payload, possibly keys).\n  \
             untrack them: git rm -r --cached {d}\n  \
             if they were ever pushed, treat their contents and keys as disclosed (docs/security.md)",
            d = dir.display()
        );
    }
}

/// Where key `name` lives for the index at `db`: `explicit` (`--key-file`) wins; otherwise
/// `<keys dir>/<store_id>/<name>`, adopting a key that brain0 ≤ 0.1.1 left inside `.brain0/`.
pub fn key_path(explicit: Option<&Path>, db: &Path, store_id: &str, name: &str) -> Result<PathBuf> {
    if let Some(path) = explicit {
        ensure_self_ignored(path)?;
        // A missing payload key file gets a fresh key, which would orphan the existing
        // payload when this index already has one (e.g. `--key-file .brain0/keyring.key`
        // after the upgrade moved it). A fresh attestation key only changes who signs.
        if name == PAYLOAD_KEY && !path.exists() {
            if let Ok(root) = keys_dir() {
                refuse_shadowing(&root.join(store_id).join(name), path)?;
            }
        }
        return Ok(path.to_path_buf());
    }
    let sibling = db.parent().unwrap_or(Path::new(".")).join(name);
    let stray = Path::new(STORE_DIR).join(name);
    locate_key(&keys_dir()?, store_id, name, &sibling, &stray)
}

/// Move every key brain0 ≤ 0.1.1 left next to the index at `db` out of the repository, whichever
/// command runs first, not only the commands that use that key.
pub fn migrate_legacy_keys(db: &Path, store_id: impl FnOnce() -> Result<String>) -> Result<()> {
    let dir = db.parent().unwrap_or(Path::new("."));
    if [PAYLOAD_KEY, ATTEST_KEY]
        .iter()
        .any(|n| dir.join(n).exists())
    {
        migrate_keys(&keys_dir()?, &store_id()?, dir)?;
    }
    Ok(())
}

fn migrate_keys(root: &Path, store_id: &str, dir: &Path) -> Result<()> {
    for name in [PAYLOAD_KEY, ATTEST_KEY] {
        let legacy = dir.join(name);
        if legacy.exists() {
            locate_key(root, store_id, name, &legacy, &legacy)?;
        }
    }
    Ok(())
}

fn refuse_shadowing(default: &Path, explicit: &Path) -> Result<()> {
    if default.exists() {
        bail!(
            "{} does not exist, but this index already has a payload key at {}; pass \
             --key-file {} or drop --key-file",
            explicit.display(),
            default.display(),
            default.display()
        );
    }
    Ok(())
}

fn keys_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(ENV_KEYS_DIR) {
        return Ok(PathBuf::from(dir));
    }
    dirs::data_dir()
        .map(|d| d.join("brain0").join("keys"))
        .context("no user data directory: set BRAIN0_KEYS_DIR or pass --key-file")
}

/// brain0 ≤ 0.1.1 kept keys at `./.brain0/<name>`, normally next to the index (`sibling`). A
/// sibling key is moved out of the repository; a key elsewhere in `./.brain0` (`stray`) is
/// never guessed at, and no second key is minted over it (that would orphan its payload).
fn locate_key(
    root: &Path,
    store_id: &str,
    name: &str,
    sibling: &Path,
    stray: &Path,
) -> Result<PathBuf> {
    let dir = root.join(store_id);
    let target = dir.join(name);
    if target.exists() {
        if sibling.exists() {
            eprintln!(
                "warning: stale key copy at {} (brain0 uses {}); delete it",
                sibling.display(),
                target.display()
            );
        }
        return Ok(target);
    }
    if !sibling.exists() && stray.exists() {
        bail!(
            "found a key at {} that is not next to this index; brain0 no longer reads keys from \
             inside the repository. Pass --key-file {} to keep using it, or move it to {}",
            stray.display(),
            stray.display(),
            target.display()
        );
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    brain0_crypto::restrict_dir_permissions(&dir)?;
    if sibling.exists() {
        move_file(sibling, &target)?;
        eprintln!(
            "moved {} → {}: brain0 keeps keys outside the repository now.\n  \
             if it was ever committed or shared, treat this key as disclosed (docs/security.md)",
            sibling.display(),
            target.display()
        );
    }
    Ok(target)
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_err() {
        // Across filesystems (e.g. a repository on another mount): copy, check, then remove.
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
        if std::fs::read(from)? != std::fs::read(to)? {
            bail!(
                "copy of {} to {} does not match",
                from.display(),
                to.display()
            );
        }
        std::fs::remove_file(from).with_context(|| format!("removing {}", from.display()))?;
    }
    brain0_crypto::restrict_permissions(to)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("brain0-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn store_dir_ignores_itself_without_clobbering() {
        let root = tmp("ignore");
        ensure_self_ignored(&root.join(".brain0/payload")).unwrap();
        let gitignore = root.join(".brain0/.gitignore");
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), GITIGNORE);

        std::fs::write(&gitignore, "custom\n").unwrap();
        ensure_self_ignored(&root.join(".brain0/index.db")).unwrap();
        assert_eq!(std::fs::read_to_string(&gitignore).unwrap(), "custom\n");

        // Paths outside a `.brain0` directory are the caller's choice: left alone.
        ensure_self_ignored(&root.join("elsewhere/index.db")).unwrap();
        assert!(!root.join("elsewhere").exists());
    }

    #[test]
    fn git_add_all_stages_nothing_from_the_store() {
        let root = tmp("git");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        if git(&["init", "-q"]).is_none() {
            return; // git unavailable
        }
        ensure_self_ignored(&root.join(".brain0/index.db")).unwrap();
        std::fs::write(root.join(".brain0/index.db"), "db").unwrap();
        std::fs::write(root.join(".brain0/keyring.key"), "k").unwrap();
        git(&["add", "-A"]).unwrap();
        let staged = git(&["diff", "--cached", "--name-only"]).unwrap();
        assert!(
            staged.stdout.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&staged.stdout)
        );
    }

    #[test]
    fn fresh_store_key_lives_outside_the_repository() {
        let root = tmp("fresh");
        let keys = root.join("keys");
        let key = locate_key(
            &keys,
            "abc",
            PAYLOAD_KEY,
            &root.join("repo/.brain0/keyring.key"),
            &root.join("cwd/.brain0/keyring.key"),
        )
        .unwrap();
        assert_eq!(key, keys.join("abc").join(PAYLOAD_KEY));
        assert!(keys.join("abc").is_dir());
        assert!(!key.exists()); // minted by the key provider on first use
    }

    #[test]
    fn legacy_key_next_to_the_index_is_moved_out() {
        let root = tmp("migrate");
        let sibling = root.join("repo/.brain0/keyring.key");
        std::fs::create_dir_all(sibling.parent().unwrap()).unwrap();
        std::fs::write(&sibling, "old-kek").unwrap();
        let key = locate_key(&root.join("keys"), "abc", PAYLOAD_KEY, &sibling, &sibling).unwrap();
        assert_eq!(std::fs::read_to_string(&key).unwrap(), "old-kek");
        assert!(!sibling.exists());

        // Second run: the moved key is reused as is.
        let again = locate_key(&root.join("keys"), "abc", PAYLOAD_KEY, &sibling, &sibling).unwrap();
        assert_eq!(again, key);
    }

    #[test]
    fn every_legacy_key_moves_whichever_command_runs_first() {
        let root = tmp("migrate-all");
        let dir = root.join("repo/.brain0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PAYLOAD_KEY), "kek").unwrap();
        std::fs::write(dir.join(ATTEST_KEY), "seed").unwrap();
        migrate_keys(&root.join("keys"), "abc", &dir).unwrap();
        let moved = |name: &str| std::fs::read_to_string(root.join("keys/abc").join(name)).unwrap();
        assert_eq!(moved(PAYLOAD_KEY), "kek");
        assert_eq!(moved(ATTEST_KEY), "seed");
        assert!(!dir.join(PAYLOAD_KEY).exists() && !dir.join(ATTEST_KEY).exists());
    }

    #[test]
    fn missing_explicit_key_never_shadows_the_moved_one() {
        let root = tmp("shadow");
        let moved = root.join("keys/abc").join(PAYLOAD_KEY);
        let explicit = root.join("repo/.brain0").join(PAYLOAD_KEY);
        refuse_shadowing(&moved, &explicit).unwrap(); // no key yet: a fresh one is fine
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, "kek").unwrap();
        let err = refuse_shadowing(&moved, &explicit).unwrap_err();
        assert!(err.to_string().contains("already has a payload key"));
    }

    #[test]
    fn existing_key_wins_over_a_stale_legacy_copy() {
        let root = tmp("stale");
        let target = root.join("keys/abc").join(PAYLOAD_KEY);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "new").unwrap();
        let sibling = root.join("keyring.key");
        std::fs::write(&sibling, "old").unwrap();
        let key = locate_key(&root.join("keys"), "abc", PAYLOAD_KEY, &sibling, &sibling).unwrap();
        assert_eq!(std::fs::read_to_string(key).unwrap(), "new");
        assert!(sibling.exists()); // never delete key material silently
    }

    #[test]
    fn stray_legacy_key_is_never_shadowed() {
        let root = tmp("stray");
        let stray = root.join("cwd/.brain0/keyring.key");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, "kek").unwrap();
        let sibling = root.join("other/keyring.key");
        let err = locate_key(&root.join("keys"), "abc", PAYLOAD_KEY, &sibling, &stray).unwrap_err();
        assert!(err.to_string().contains("--key-file"));
        assert!(!root.join("keys/abc").exists());
        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "kek");
    }
}
