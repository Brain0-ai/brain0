//! A tiny standalone cache database for turn summaries.
//!
//! Keys are content hashes of the turn text, values the (already-redacted) summaries — so the
//! cache is safe to share across repos and to keep OUTSIDE the index: a full `rm -rf .brain0`
//! rebuild re-pays zero LLM calls for turns it has already summarized. Lives by default under
//! the user's cache dir, not inside the index it accelerates.

use rusqlite::{params, Connection, OptionalExtension};

use crate::{Result, StorageError};

/// Content-keyed summary cache on its own SQLite file.
#[derive(Debug)]
pub struct SummaryCacheDb {
    conn: Connection,
}

impl SummaryCacheDb {
    /// Open (creating if needed) the cache at `path`, with owner-only permissions.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            let _ = brain0_crypto::restrict_dir_permissions(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS summaries (
                 key     TEXT PRIMARY KEY,
                 summary TEXT NOT NULL
             );",
        )?;
        brain0_crypto::restrict_permissions(path)?;
        Ok(Self { conn })
    }

    /// The cached summary for a content key, if any.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .prepare("SELECT summary FROM summaries WHERE key=?1")?
            .query_row([key], |r| r.get::<_, String>(0))
            .optional()
            .map_err(StorageError::from)
    }

    /// Store a summary (content-keyed: the first write wins, rewrites are no-ops).
    pub fn put(&self, key: &str, summary: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO summaries (key, summary) VALUES (?1, ?2)",
            params![key, summary],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_first_write_wins() {
        let dir = std::env::temp_dir().join(format!("brain0-sumcache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cache.db");
        let db = SummaryCacheDb::open(&path).unwrap();
        assert_eq!(db.get("k").unwrap(), None);
        db.put("k", "first").unwrap();
        db.put("k", "second").unwrap();
        assert_eq!(db.get("k").unwrap().as_deref(), Some("first"));
        // A separate handle (a later run) sees the same content.
        let again = SummaryCacheDb::open(&path).unwrap();
        assert_eq!(again.get("k").unwrap().as_deref(), Some("first"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
