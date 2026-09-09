//! Tamper evidence for ingested transcripts.
//!
//! The read set (and the declared changes) come from one source: the transcript the agent
//! harness writes to disk, a plain user-writable file. brain0 cannot make that file a second,
//! independent witness, but it can pin what it saw: every ingest pass records a BLAKE3 digest
//! of the exact byte range it consumed in the append-only audit log, and [`verify_transcripts`]
//! re-hashes those ranges later. A transcript edited or truncated *after* ingest is caught; one
//! altered before ingest is not (documented in `docs/governance.md`).

use std::path::{Path, PathBuf};

use brain0_storage::Storage;

use crate::jsonl::hash_range;
use crate::Result;

/// Audit event type under which ingested ranges are recorded.
pub const INGEST_EVENT: &str = "ingest";

/// Outcome of re-checking one ingested transcript range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptStatus {
    /// The bytes on disk still hash to what was ingested.
    Ok,
    /// The file exists but the range hashes differently (rewritten) or is now shorter (truncated).
    Changed,
    /// The transcript file is gone (agents prune old sessions; not an integrity failure).
    Missing,
}

/// One ingested range and its current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptCheck {
    pub adapter: String,
    pub file: PathBuf,
    pub from: u64,
    pub to: u64,
    pub status: TranscriptStatus,
}

/// Record the range `[from, to)` of `file` consumed by an ingest pass (kind/offsets/digest only).
pub fn record_ingest(
    storage: &dyn Storage,
    adapter: &str,
    file: &Path,
    from: u64,
    to: u64,
) -> Result<()> {
    let digest = hash_range(file, from, to)?;
    storage.append_audit(
        INGEST_EVENT,
        &format!(
            "adapter={adapter} from={from} to={to} blake3={digest} file={}",
            file.display()
        ),
        chrono::Utc::now(),
    )?;
    Ok(())
}

/// Parse `key=value` pairs written by [`record_ingest`]. `file=` is last and may contain spaces.
fn parse_detail(detail: &str) -> Option<(String, PathBuf, u64, u64, String)> {
    let (head, file) = detail.split_once(" file=")?;
    let mut adapter = None;
    let mut from = None;
    let mut to = None;
    let mut digest = None;
    for pair in head.split(' ') {
        match pair.split_once('=') {
            Some(("adapter", v)) => adapter = Some(v.to_owned()),
            Some(("from", v)) => from = v.parse().ok(),
            Some(("to", v)) => to = v.parse().ok(),
            Some(("blake3", v)) => digest = Some(v.to_owned()),
            _ => {}
        }
    }
    Some((adapter?, PathBuf::from(file), from?, to?, digest?))
}

/// Re-hash every ingested transcript range recorded in the audit log.
pub fn verify_transcripts(storage: &dyn Storage) -> Result<Vec<TranscriptCheck>> {
    // The audit log is newest-first and bounded by `limit`; a negative i64 limit is
    // "unlimited" for SQLite, so pass the largest value that stays positive.
    let events = storage.list_audit(usize::try_from(i64::MAX).unwrap_or(usize::MAX))?;
    let mut out = Vec::new();
    for event in events.iter().rev() {
        if event.event_type != INGEST_EVENT {
            continue;
        }
        let Some((adapter, file, from, to, expected)) = parse_detail(&event.detail) else {
            continue;
        };
        let status = if !file.is_file() {
            TranscriptStatus::Missing
        } else {
            match hash_range(&file, from, to) {
                Ok(actual) if actual == expected => TranscriptStatus::Ok,
                _ => TranscriptStatus::Changed,
            }
        };
        out.push(TranscriptCheck {
            adapter,
            file,
            from,
            to,
            status,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_storage::SqliteStorage;
    use std::io::Write;

    #[test]
    fn ingested_range_is_pinned_and_rewrites_are_detected() {
        let dir = std::env::temp_dir().join(format!("brain0-integrity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sess.jsonl");
        std::fs::write(&file, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let store = SqliteStorage::open_in_memory().unwrap();

        record_ingest(&store, "claude-code", &file, 0, 8).unwrap();
        let audit = store.list_audit(5).unwrap();
        assert_eq!(audit[0].event_type, INGEST_EVENT);
        assert!(audit[0].detail.contains("blake3="));
        assert!(
            !audit[0].detail.contains("\"a\""),
            "audit must carry digests, not content"
        );

        let checks = verify_transcripts(&store).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, TranscriptStatus::Ok);
        assert_eq!((checks[0].from, checks[0].to), (0, 8));

        // Appending is fine (append-only transcripts): the pinned range is untouched.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "{{\"c\":3}}").unwrap();
        assert_eq!(
            verify_transcripts(&store).unwrap()[0].status,
            TranscriptStatus::Ok
        );

        // Rewriting inside the range is caught.
        std::fs::write(&file, "{\"a\":9}\n{\"b\":2}\n").unwrap();
        assert_eq!(
            verify_transcripts(&store).unwrap()[0].status,
            TranscriptStatus::Changed
        );

        // Truncation below the range is caught too.
        std::fs::write(&file, "{\"a").unwrap();
        assert_eq!(
            verify_transcripts(&store).unwrap()[0].status,
            TranscriptStatus::Changed
        );

        // A pruned transcript is reported, not treated as tampering.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            verify_transcripts(&store).unwrap()[0].status,
            TranscriptStatus::Missing
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
