//! `brain0 rewind` — the watch safety net's restore side.
//!
//! `brain0 watch` persists, per checkpoint, a full-tree manifest whose file contents live in
//! the content-addressed payload store (identical files across checkpoints share one blob).
//! `rewind --list` shows what can be restored; `rewind --to <id>` writes a checkpoint's tree
//! back — AFTER recording a fresh checkpoint of the current state, so a rewind is itself
//! reversible. Files created after the target checkpoint are left in place (no deletions).

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use brain0_model::Timestamp;
use brain0_storage::{PayloadStore, Storage};

/// Persist a checkpoint's full-tree manifest: every file's content into the payload store
/// (content-addressed → deduped) plus one `checkpoint_files` row per file.
pub fn persist_manifest(
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    checkpoint_id: &str,
    at: &Timestamp,
    files: &[(String, String)],
) -> Result<()> {
    for (rel_path, content) in files {
        let content_ref = payload.put_str(content)?;
        storage.put_checkpoint_file(checkpoint_id, rel_path, &content_ref, at)?;
    }
    Ok(())
}

/// Resolve a manifest's rel_path under `root`, refusing anything that could escape it
/// (absolute paths, `..` segments) — the manifest is data, not trusted input.
fn safe_dest(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let rel = Path::new(rel_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        bail!("refusing unsafe manifest path {rel_path:?}");
    }
    Ok(root.join(rel))
}

/// Restore the tree recorded by `checkpoint_id` under `root`. Returns how many files were
/// written. The caller records a pre-rewind checkpoint FIRST (see `cmd_rewind`).
pub fn restore(
    storage: &dyn Storage,
    payload: &dyn PayloadStore,
    checkpoint_id: &str,
    root: &Path,
) -> Result<usize> {
    let manifest = storage.checkpoint_files(checkpoint_id)?;
    if manifest.is_empty() {
        bail!("no manifest recorded for checkpoint {checkpoint_id:?} (run `brain0 rewind --list`)");
    }
    let mut written = 0usize;
    for (rel_path, content_ref) in &manifest {
        let content = payload
            .get_str(content_ref)?
            .with_context(|| format!("payload missing for {rel_path} ({content_ref:?})"))?;
        let dest = safe_dest(root, rel_path)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_storage::{InMemoryPayloadStore, SqliteStorage};
    use chrono::TimeZone;

    fn ts(secs: i64) -> Timestamp {
        brain0_model::chrono::Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn manifest_roundtrip_and_restore() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let root = std::env::temp_dir().join(format!("brain0-rewind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();

        let files = vec![
            ("src/a.rs".to_owned(), "fn a() {}".to_owned()),
            ("src/b.rs".to_owned(), "fn b() {}".to_owned()),
        ];
        persist_manifest(&storage, &payload, "cp_1", &ts(100), &files).unwrap();

        // The user (or an agent) wrecks the tree.
        std::fs::write(root.join("src/a.rs"), "GARBAGE").unwrap();
        std::fs::write(root.join("src/b.rs"), "MORE GARBAGE").unwrap();

        let written = restore(&storage, &payload, "cp_1", &root).unwrap();
        assert_eq!(written, 2);
        assert_eq!(
            std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "fn a() {}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/b.rs")).unwrap(),
            "fn b() {}"
        );

        // list_checkpoints sees it, newest first.
        let list = storage.list_checkpoints().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "cp_1");
        assert_eq!(list[0].2, 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_refuses_escaping_paths_and_unknown_checkpoints() {
        let storage = SqliteStorage::open_in_memory().unwrap();
        let payload = InMemoryPayloadStore::new();
        let root = std::env::temp_dir().join(format!("brain0-rewind-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Unknown checkpoint → clear error.
        assert!(restore(&storage, &payload, "cp_missing", &root).is_err());

        // A malicious manifest entry must be refused, not written outside root.
        let evil = vec![("../evil.txt".to_owned(), "pwn".to_owned())];
        persist_manifest(&storage, &payload, "cp_evil", &ts(5), &evil).unwrap();
        assert!(restore(&storage, &payload, "cp_evil", &root).is_err());
        assert!(!root.parent().unwrap().join("evil.txt").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
