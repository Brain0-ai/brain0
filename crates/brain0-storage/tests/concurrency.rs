//! Multi-client append-only concurrency.
//!
//! Several independent clients (separate connections to the same SQLite file, as separate
//! observer processes would be) append versions for the *same* symbol concurrently. Because
//! the symbol identity is deterministic, the contributions converge on one node; because
//! the model is append-only, every version survives — no update is lost or corrupted.

use std::collections::HashSet;
use std::thread;

use brain0_model::{
    Agent, ArtifactId, ArtifactVersion, Author, ChangeKind, ChangeSource, VersionId,
};
use brain0_storage::{SqliteStorage, Storage};
use chrono::TimeZone;

fn temp_db() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("brain0-concurrency-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn concurrent_clients_converge_and_preserve_all_versions() {
    let path = temp_db();
    // First connection creates the schema.
    SqliteStorage::open(&path).unwrap();

    const CLIENTS: usize = 8;
    const PER_CLIENT: usize = 25;
    let shared = ArtifactId::new("art_shared"); // same symbol id from every client

    let handles: Vec<_> = (0..CLIENTS)
        .map(|client| {
            let path = path.clone();
            let shared = shared.clone();
            thread::spawn(move || {
                // Each client is its own connection, like a separate observer process.
                let storage = SqliteStorage::open(&path).unwrap();
                for i in 0..PER_CLIENT {
                    let version = ArtifactVersion {
                        id: VersionId::new(format!("ver_{client}_{i}")),
                        artifact_id: shared.clone(),
                        timestamp: chrono::Utc
                            .timestamp_opt(1_700_000_000 + (client * PER_CLIENT + i) as i64, 0)
                            .single()
                            .unwrap(),
                        author: Author::new(format!("dev{client}")),
                        agent: Agent::new("claude-code"),
                        source: ChangeSource::Git {
                            commit_sha: format!("sha_{client}_{i}"),
                        },
                        qualified_path: "m.py::f".into(),
                        fingerprint: format!("fp{i}"),
                        change_kind: ChangeKind::Modified,
                        lines_added: 1,
                        lines_removed: 0,
                        diff_ref: None,
                    };
                    storage.append_artifact_version(&version).unwrap();
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    let storage = SqliteStorage::open(&path).unwrap();
    let versions = storage.artifact_versions(&shared).unwrap();

    // Append-only: every version from every client survived.
    assert_eq!(versions.len(), CLIENTS * PER_CLIENT);

    // Convergence: all on one node's single timeline, ordered, multi-author.
    let authors: HashSet<&str> = versions.iter().map(|v| v.author.name.as_str()).collect();
    assert_eq!(
        authors.len(),
        CLIENTS,
        "each author is a tag on the same timeline"
    );
    for pair in versions.windows(2) {
        assert!(
            pair[0].timestamp <= pair[1].timestamp,
            "timeline is ordered"
        );
    }

    let _ = std::fs::remove_file(&path);
}
