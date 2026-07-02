//! SQLite backend: local, zero-server, the "the server is you" case.
//!
//! Uses the bundled SQLite (no system dependency) in WAL mode so that multiple observer
//! processes can append concurrently to the same file. Vector search is implemented as a
//! filtered cosine scan in Rust; the schema is kept compatible with a future `sqlite-vec`
//! drop-in.

use std::str::FromStr;

use rusqlite::{params, Connection, OptionalExtension, Row};

use brain0_model::{
    Agent, ArtifactId, ArtifactNode, ArtifactVersion, Author, ChangeKind, ChangeSource, Edge,
    EdgeKind, Lang, Level, PayloadRef, RiskState, SessionId, TaskId, TaskNode, TaskVersion,
    Timestamp, VersionId,
};

use crate::{
    cosine, decode_u64s, decode_vector, encode_u64s, encode_vector, Result, Storage, StorageError,
    VectorHit,
};

const SCHEMA: &str = include_str!("../../../schema/sqlite.sql");

// These column lists and the codecs below are the single source of truth for the on-disk
// layout, re-exported via `crate::backend` for out-of-tree backends (e.g. brain0-enterprise's
// Postgres backend) so every backend serializes the graph identically.
pub const ARTIFACT_COLS: &str =
    "id, level, repo, qualified_path, lang, parent_id, current_version, risk_apriori, risk_aposteriori";
pub const TASK_COLS: &str =
    "id, session_id, agent_name, agent_version, author_name, author_email, created_at, current_version, source_adapter, session_cwd, model, reviewers_json";
pub const AV_COLS: &str = "id, artifact_id, timestamp, author_name, author_email, agent_name, agent_version, source_kind, source_ref, qualified_path, fingerprint, change_kind, change_from, lines_added, lines_removed, diff_ref";
pub const TV_COLS: &str =
    "id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json, reads_json, read_secrets_json";

/// A SQLite-backed [`Storage`].
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Connection,
}

impl SqliteStorage {
    /// Open (creating if needed) a database at `path` and apply the schema. The database
    /// file gets owner-only permissions.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;",
        )?;
        if let Some(parent) = path.parent() {
            if parent.is_dir() {
                let _ = brain0_crypto::restrict_dir_permissions(parent);
            }
        }
        brain0_crypto::restrict_permissions(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open an **encrypted** SQLite database (SQLCipher). Requires building with
    /// `--features sqlcipher`; otherwise it fails closed rather than silently storing in
    /// clear (, fail-closed).
    pub fn open_encrypted(path: impl AsRef<std::path::Path>, key_hex: &str) -> Result<Self> {
        #[cfg(feature = "sqlcipher")]
        {
            let path = path.as_ref();
            let conn = Connection::open(path)?;
            conn.pragma_update(None, "key", format!("x'{key_hex}'"))?;
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;",
            )?;
            brain0_crypto::restrict_permissions(path)?;
            let store = Self { conn };
            store.migrate()?;
            Ok(store)
        }
        #[cfg(not(feature = "sqlcipher"))]
        {
            let _ = (path.as_ref(), key_hex);
            Err(StorageError::Invalid(
                "SQLite DB encryption requires building with --features sqlcipher".to_owned(),
            ))
        }
    }

    /// Open an ephemeral in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Record how this index's payload is stored at rest (encrypted vs plaintext), so a later
    /// consumer (e.g. the GUI server's refresh endpoint) re-runs ingest/observe with the SAME
    /// mode instead of silently downgrading an encrypted store to plaintext. Mirrors the
    /// `meta` key/value pattern used for embedding model/dim. Idempotent (`INSERT OR REPLACE`).
    pub fn set_payload_encryption(&self, encrypted: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('payload_encryption', ?1)",
            params![if encrypted { "encrypted" } else { "plaintext" }],
        )?;
        Ok(())
    }

    /// Read the persisted payload encryption mode. `None` for a legacy index that predates the
    /// flag — callers MUST fail closed to encrypted, never assume plaintext.
    pub fn get_payload_encryption(&self) -> Result<Option<bool>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key='payload_encryption'")?;
        Ok(stmt
            .query_row([], |r| r.get::<_, String>(0))
            .optional()?
            .map(|v| v == "encrypted"))
    }
}

fn parse_ts(s: &str) -> Result<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|err| StorageError::Invalid(format!("bad timestamp '{s}': {err}")))
}

pub fn source_to_cols(source: &ChangeSource) -> (&'static str, String) {
    match source {
        ChangeSource::Git { commit_sha } => ("git", commit_sha.clone()),
        ChangeSource::Checkpoint { checkpoint_id } => ("checkpoint", checkpoint_id.clone()),
    }
}

pub fn source_from_cols(kind: &str, reference: String) -> Result<ChangeSource> {
    match kind {
        "git" => Ok(ChangeSource::Git {
            commit_sha: reference,
        }),
        "checkpoint" => Ok(ChangeSource::Checkpoint {
            checkpoint_id: reference,
        }),
        other => Err(StorageError::Invalid(format!("bad source kind: {other}"))),
    }
}

pub fn change_kind_to_cols(kind: &ChangeKind) -> (&'static str, Option<String>) {
    match kind {
        ChangeKind::Added => ("added", None),
        ChangeKind::Modified => ("modified", None),
        ChangeKind::Deleted => ("deleted", None),
        ChangeKind::Renamed { from } => ("renamed", Some(from.clone())),
        ChangeKind::Moved { from } => ("moved", Some(from.clone())),
    }
}

pub fn change_kind_from_cols(kind: &str, from: Option<String>) -> Result<ChangeKind> {
    let need_from = || {
        from.clone()
            .ok_or_else(|| StorageError::Invalid(format!("{kind} change missing change_from")))
    };
    match kind {
        "added" => Ok(ChangeKind::Added),
        "modified" => Ok(ChangeKind::Modified),
        "deleted" => Ok(ChangeKind::Deleted),
        "renamed" => Ok(ChangeKind::Renamed { from: need_from()? }),
        "moved" => Ok(ChangeKind::Moved { from: need_from()? }),
        other => Err(StorageError::Invalid(format!("bad change kind: {other}"))),
    }
}

fn map_artifact(row: &Row) -> Result<ArtifactNode> {
    let level_s: String = row.get(1)?;
    let lang: Option<String> = row.get(4)?;
    let parent: Option<String> = row.get(5)?;
    Ok(ArtifactNode {
        id: ArtifactId::new(row.get::<_, String>(0)?),
        level: Level::from_str(&level_s).map_err(|err| StorageError::Invalid(err.to_string()))?,
        repo: row.get(2)?,
        qualified_path: row.get(3)?,
        lang: lang.map(Lang::new),
        parent_id: parent.map(ArtifactId::new),
        current_version: VersionId::new(row.get::<_, String>(6)?),
        risk: RiskState::new(row.get::<_, f64>(7)? as f32, row.get::<_, f64>(8)? as f32),
    })
}

fn map_task(row: &Row) -> Result<TaskNode> {
    let agent_version: Option<String> = row.get(3)?;
    let author_email: Option<String> = row.get(5)?;
    let created_at: String = row.get(6)?;
    Ok(TaskNode {
        id: TaskId::new(row.get::<_, String>(0)?),
        session_id: SessionId::new(row.get::<_, String>(1)?),
        agent: Agent {
            name: row.get(2)?,
            version: agent_version,
        },
        author: Author {
            name: row.get(4)?,
            email: author_email,
        },
        created_at: parse_ts(&created_at)?,
        current_version: VersionId::new(row.get::<_, String>(7)?),
        source_adapter: row.get::<_, Option<String>>(8)?,
        session_cwd: row.get::<_, Option<String>>(9)?,
        model: row.get::<_, Option<String>>(10)?,
        reviewers: row
            .get::<_, Option<String>>(11)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
    })
}

fn map_artifact_version(row: &Row) -> Result<ArtifactVersion> {
    let timestamp: String = row.get(2)?;
    let author_email: Option<String> = row.get(4)?;
    let agent_version: Option<String> = row.get(6)?;
    let source_kind: String = row.get(7)?;
    let source_ref: String = row.get(8)?;
    let change_kind: String = row.get(11)?;
    let change_from: Option<String> = row.get(12)?;
    let diff_ref: Option<String> = row.get(15)?;
    Ok(ArtifactVersion {
        id: VersionId::new(row.get::<_, String>(0)?),
        artifact_id: ArtifactId::new(row.get::<_, String>(1)?),
        timestamp: parse_ts(&timestamp)?,
        author: Author {
            name: row.get(3)?,
            email: author_email,
        },
        agent: Agent {
            name: row.get(5)?,
            version: agent_version,
        },
        source: source_from_cols(&source_kind, source_ref)?,
        qualified_path: row.get(9)?,
        fingerprint: row.get(10)?,
        change_kind: change_kind_from_cols(&change_kind, change_from)?,
        lines_added: row.get::<_, i64>(13)? as u32,
        lines_removed: row.get::<_, i64>(14)? as u32,
        diff_ref: diff_ref.map(PayloadRef::new),
    })
}

fn map_task_version(row: &Row) -> Result<TaskVersion> {
    let timestamp: String = row.get(2)?;
    let prompt_ref: Option<String> = row.get(3)?;
    let decision_summary_ref: Option<String> = row.get(4)?;
    let declared_json: Option<String> = row.get(5)?;
    let drift_json: Option<String> = row.get(6)?;
    let reads_json: Option<String> = row.get(7)?;
    let read_secrets_json: Option<String> = row.get(8)?;
    Ok(TaskVersion {
        id: VersionId::new(row.get::<_, String>(0)?),
        task_id: TaskId::new(row.get::<_, String>(1)?),
        timestamp: parse_ts(&timestamp)?,
        prompt_ref: prompt_ref.map(PayloadRef::new),
        decision_summary_ref: decision_summary_ref.map(PayloadRef::new),
        declared: match declared_json {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        },
        drift: match drift_json {
            Some(json) => Some(serde_json::from_str(&json)?),
            None => None,
        },
        reads: match reads_json {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        },
        read_secrets: match read_secrets_json {
            Some(json) => serde_json::from_str(&json)?,
            None => Vec::new(),
        },
    })
}

impl Storage for SqliteStorage {
    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA)?;
        // Additive column migration for indexes created before `reads_json` existed. SQLite has no
        // "ADD COLUMN IF NOT EXISTS", so we run it and ignore the duplicate-column error.
        let _ = self
            .conn
            .execute("ALTER TABLE task_versions ADD COLUMN reads_json TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE task_versions ADD COLUMN read_secrets_json TEXT",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE task_nodes ADD COLUMN model TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE task_nodes ADD COLUMN reviewers_json TEXT", []);
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        Ok(())
    }

    fn put_artifact_node(&self, node: &ArtifactNode) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO artifact_nodes ({ARTIFACT_COLS}) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ),
            params![
                node.id.as_str(),
                node.level.as_str(),
                node.repo,
                node.qualified_path,
                node.lang.as_ref().map(Lang::as_str),
                node.parent_id.as_ref().map(ArtifactId::as_str),
                node.current_version.as_str(),
                f64::from(node.risk.apriori),
                f64::from(node.risk.aposteriori),
            ],
        )?;
        Ok(())
    }

    fn get_artifact_node(&self, id: &ArtifactId) -> Result<Option<ArtifactNode>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifact_nodes WHERE id=?1"
        ))?;
        let mut rows = stmt.query(params![id.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_artifact(row)?)),
            None => Ok(None),
        }
    }

    fn find_artifact_by_path(
        &self,
        repo: &str,
        level: Level,
        qualified_path: &str,
    ) -> Result<Option<ArtifactNode>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifact_nodes \
             WHERE repo=?1 AND level=?2 AND qualified_path=?3 LIMIT 1"
        ))?;
        let mut rows = stmt.query(params![repo, level.as_str(), qualified_path])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_artifact(row)?)),
            None => Ok(None),
        }
    }

    fn list_artifacts(&self, repo: &str, level: Level) -> Result<Vec<ArtifactNode>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifact_nodes WHERE repo=?1 AND level=?2 \
             ORDER BY qualified_path"
        ))?;
        let mut rows = stmt.query(params![repo, level.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_artifact(row)?);
        }
        Ok(out)
    }

    fn children(&self, parent: &ArtifactId) -> Result<Vec<ArtifactNode>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifact_nodes WHERE parent_id=?1 ORDER BY qualified_path"
        ))?;
        let mut rows = stmt.query(params![parent.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_artifact(row)?);
        }
        Ok(out)
    }

    fn update_artifact_risk(&self, id: &ArtifactId, risk: RiskState) -> Result<()> {
        self.conn.execute(
            "UPDATE artifact_nodes SET risk_apriori=?2, risk_aposteriori=?3 WHERE id=?1",
            params![
                id.as_str(),
                f64::from(risk.apriori),
                f64::from(risk.aposteriori)
            ],
        )?;
        Ok(())
    }

    fn put_task_node(&self, node: &TaskNode) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO task_nodes ({TASK_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"
            ),
            params![
                node.id.as_str(),
                node.session_id.as_str(),
                node.agent.name,
                node.agent.version,
                node.author.name,
                node.author.email,
                node.created_at.to_rfc3339(),
                node.current_version.as_str(),
                node.source_adapter,
                node.session_cwd,
                node.model,
                serde_json::to_string(&node.reviewers).unwrap_or_else(|_| "[]".to_owned()),
            ],
        )?;
        Ok(())
    }

    fn get_task_node(&self, id: &TaskId) -> Result<Option<TaskNode>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {TASK_COLS} FROM task_nodes WHERE id=?1"))?;
        let mut rows = stmt.query(params![id.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_task(row)?)),
            None => Ok(None),
        }
    }

    fn append_artifact_version(&self, version: &ArtifactVersion) -> Result<()> {
        let (source_kind, source_ref) = source_to_cols(&version.source);
        let (change_kind, change_from) = change_kind_to_cols(&version.change_kind);
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO artifact_versions ({AV_COLS}) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"
            ),
            params![
                version.id.as_str(),
                version.artifact_id.as_str(),
                version.timestamp.to_rfc3339(),
                version.author.name,
                version.author.email,
                version.agent.name,
                version.agent.version,
                source_kind,
                source_ref,
                version.qualified_path,
                version.fingerprint,
                change_kind,
                change_from,
                i64::from(version.lines_added),
                i64::from(version.lines_removed),
                version.diff_ref.as_ref().map(PayloadRef::as_str),
            ],
        )?;
        Ok(())
    }

    fn get_artifact_version(&self, id: &VersionId) -> Result<Option<ArtifactVersion>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {AV_COLS} FROM artifact_versions WHERE id=?1"
        ))?;
        let mut rows = stmt.query(params![id.as_str()])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_artifact_version(row)?)),
            None => Ok(None),
        }
    }

    fn artifact_versions(&self, artifact_id: &ArtifactId) -> Result<Vec<ArtifactVersion>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {AV_COLS} FROM artifact_versions WHERE artifact_id=?1 ORDER BY timestamp, id"
        ))?;
        let mut rows = stmt.query(params![artifact_id.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_artifact_version(row)?);
        }
        Ok(out)
    }

    fn append_task_version(&self, version: &TaskVersion) -> Result<()> {
        let declared_json = if version.declared.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&version.declared)?)
        };
        let drift_json = match &version.drift {
            Some(drift) => Some(serde_json::to_string(drift)?),
            None => None,
        };
        let reads_json = if version.reads.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&version.reads)?)
        };
        let read_secrets_json = if version.read_secrets.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&version.read_secrets)?)
        };
        self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO task_versions ({TV_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"
            ),
            params![
                version.id.as_str(),
                version.task_id.as_str(),
                version.timestamp.to_rfc3339(),
                version.prompt_ref.as_ref().map(PayloadRef::as_str),
                version
                    .decision_summary_ref
                    .as_ref()
                    .map(PayloadRef::as_str),
                declared_json,
                drift_json,
                reads_json,
                read_secrets_json,
            ],
        )?;
        Ok(())
    }

    fn task_versions(&self, task_id: &TaskId) -> Result<Vec<TaskVersion>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TV_COLS} FROM task_versions WHERE task_id=?1 ORDER BY timestamp, id"
        ))?;
        let mut rows = stmt.query(params![task_id.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(map_task_version(row)?);
        }
        Ok(out)
    }

    fn put_edge(&self, edge: &Edge) -> Result<()> {
        let (src, dst) = edge.endpoints();
        let attrs = serde_json::to_string(edge)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO edges (kind, src, dst, attrs_json) VALUES (?1,?2,?3,?4)",
            params![edge.kind().as_str(), src, dst, attrs],
        )?;
        Ok(())
    }

    fn out_edges(&self, kind: EdgeKind, src: &str) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT attrs_json FROM edges WHERE kind=?1 AND src=?2")?;
        let mut rows = stmt.query(params![kind.as_str(), src])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let json: String = row.get(0)?;
            out.push(serde_json::from_str(&json)?);
        }
        Ok(out)
    }

    fn in_edges(&self, kind: EdgeKind, dst: &str) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT attrs_json FROM edges WHERE kind=?1 AND dst=?2")?;
        let mut rows = stmt.query(params![kind.as_str(), dst])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let json: String = row.get(0)?;
            out.push(serde_json::from_str(&json)?);
        }
        Ok(out)
    }

    fn put_artifact_fingerprint(
        &self,
        artifact_id: &ArtifactId,
        hash: &str,
        shingles: &[u64],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO artifact_fingerprints (artifact_id, hash, shingles) \
             VALUES (?1,?2,?3)",
            params![artifact_id.as_str(), hash, encode_u64s(shingles)],
        )?;
        Ok(())
    }

    fn get_artifact_fingerprint(
        &self,
        artifact_id: &ArtifactId,
    ) -> Result<Option<(String, Vec<u64>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash, shingles FROM artifact_fingerprints WHERE artifact_id=?1")?;
        let mut rows = stmt.query(params![artifact_id.as_str()])?;
        match rows.next()? {
            Some(row) => {
                let hash: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok(Some((hash, decode_u64s(&blob))))
            }
            None => Ok(None),
        }
    }

    fn set_embedding_meta(&self, model: &str, dim: usize) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_model', ?1)",
            params![model],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('embedding_dim', ?1)",
            params![dim.to_string()],
        )?;
        Ok(())
    }

    fn get_embedding_meta(&self) -> Result<Option<(String, usize)>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key=?1")?;
        let model: Option<String> = stmt
            .query_row(params!["embedding_model"], |r| r.get(0))
            .optional()?;
        let dim: Option<String> = stmt
            .query_row(params!["embedding_dim"], |r| r.get(0))
            .optional()?;
        match (model, dim) {
            (Some(m), Some(d)) => Ok(Some((
                m,
                d.parse()
                    .map_err(|_| StorageError::Invalid(format!("bad embedding_dim '{d}'")))?,
            ))),
            _ => Ok(None),
        }
    }

    fn put_task_embedding(&self, task_id: &TaskId, vector: &[f32]) -> Result<()> {
        if let Some((_, dim)) = self.get_embedding_meta()? {
            if dim != vector.len() {
                return Err(StorageError::Invalid(format!(
                    "embedding dim {} does not match store dim {dim} (mixed dimensions forbidden)",
                    vector.len()
                )));
            }
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO task_embeddings (task_id, dim, vec) VALUES (?1,?2,?3)",
            params![task_id.as_str(), vector.len() as i64, encode_vector(vector)],
        )?;
        Ok(())
    }

    fn get_cursor(&self, adapter: &str, file: &str) -> Result<Option<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT byte_offset FROM cursors WHERE adapter=?1 AND file=?2")?;
        let mut rows = stmt.query(params![adapter, file])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get::<_, i64>(0)? as u64)),
            None => Ok(None),
        }
    }

    fn set_cursor(&self, adapter: &str, file: &str, offset: u64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO cursors (adapter, file, byte_offset) VALUES (?1,?2,?3)",
            params![adapter, file, offset as i64],
        )?;
        Ok(())
    }

    fn append_audit(&self, event_type: &str, detail: &str, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO audit_log (timestamp, event_type, detail) VALUES (?1,?2,?3)",
            params![at.to_rfc3339(), event_type, detail],
        )?;
        Ok(())
    }

    fn list_audit(&self, limit: usize) -> Result<Vec<crate::AuditEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, event_type, detail FROM audit_log ORDER BY id DESC LIMIT ?1",
        )?;
        let mut rows = stmt.query(params![limit as i64])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let ts: String = row.get(0)?;
            out.push(crate::AuditEvent {
                timestamp: parse_ts(&ts)?,
                event_type: row.get(1)?,
                detail: row.get(2)?,
            });
        }
        Ok(out)
    }

    fn all_payload_refs(&self) -> Result<Vec<PayloadRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT prompt_ref FROM task_versions WHERE prompt_ref IS NOT NULL \
             UNION SELECT decision_summary_ref FROM task_versions WHERE decision_summary_ref IS NOT NULL \
             UNION SELECT diff_ref FROM artifact_versions WHERE diff_ref IS NOT NULL",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(PayloadRef::new(row.get::<_, String>(0)?));
        }
        Ok(out)
    }

    fn all_task_ids(&self) -> Result<Vec<TaskId>> {
        let mut stmt = self.conn.prepare("SELECT id FROM task_nodes")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(TaskId::new(row.get::<_, String>(0)?));
        }
        Ok(out)
    }

    fn repos(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT repo FROM artifact_nodes ORDER BY repo")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get::<_, String>(0)?);
        }
        Ok(out)
    }

    fn put_checkpoint_file(
        &self,
        checkpoint_id: &str,
        rel_path: &str,
        content_ref: &brain0_model::PayloadRef,
        at: &Timestamp,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO checkpoint_files (checkpoint_id, rel_path, content_ref, at) VALUES (?1,?2,?3,?4)",
            rusqlite::params![checkpoint_id, rel_path, content_ref.as_str(), at.to_rfc3339()],
        )?;
        Ok(())
    }

    fn checkpoint_files(
        &self,
        checkpoint_id: &str,
    ) -> Result<Vec<(String, brain0_model::PayloadRef)>> {
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, content_ref FROM checkpoint_files WHERE checkpoint_id=?1 ORDER BY rel_path",
        )?;
        let mut rows = stmt.query([checkpoint_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((
                row.get::<_, String>(0)?,
                brain0_model::PayloadRef::new(row.get::<_, String>(1)?),
            ));
        }
        Ok(out)
    }

    fn list_checkpoints(&self) -> Result<Vec<(String, Timestamp, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT checkpoint_id, MAX(at), COUNT(*) FROM checkpoint_files GROUP BY checkpoint_id ORDER BY MAX(at) DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let at: String = row.get(1)?;
            let ts = brain0_model::chrono::DateTime::parse_from_rfc3339(&at)
                .map_err(|e| StorageError::Invalid(format!("bad checkpoint timestamp: {e}")))?
                .with_timezone(&brain0_model::chrono::Utc);
            out.push((row.get::<_, String>(0)?, ts, row.get::<_, i64>(2)? as usize));
        }
        Ok(out)
    }

    fn clear_all_embeddings(&self) -> Result<()> {
        self.conn.execute("DELETE FROM task_embeddings", [])?;
        Ok(())
    }

    fn mark_payload_purged(&self, reference: &PayloadRef) -> Result<()> {
        let r = reference.as_str();
        self.conn.execute(
            "UPDATE task_versions SET payload_purged=1 WHERE prompt_ref=?1 OR decision_summary_ref=?1",
            params![r],
        )?;
        self.conn.execute(
            "UPDATE artifact_versions SET payload_purged=1 WHERE diff_ref=?1",
            params![r],
        )?;
        Ok(())
    }

    fn delete_task_embedding(&self, task_id: &TaskId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM task_embeddings WHERE task_id=?1",
            params![task_id.as_str()],
        )?;
        Ok(())
    }

    fn task_payload_refs(&self, task_id: &TaskId) -> Result<Vec<PayloadRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT prompt_ref FROM task_versions WHERE task_id=?1 AND prompt_ref IS NOT NULL \
             UNION SELECT decision_summary_ref FROM task_versions WHERE task_id=?1 AND decision_summary_ref IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![task_id.as_str()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(PayloadRef::new(row.get::<_, String>(0)?));
        }
        Ok(out)
    }

    fn payloads_older_than(&self, cutoff: Timestamp) -> Result<Vec<(TaskId, PayloadRef)>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, prompt_ref FROM task_versions \
             WHERE timestamp < ?1 AND payload_purged=0 AND prompt_ref IS NOT NULL \
             UNION \
             SELECT task_id, decision_summary_ref FROM task_versions \
             WHERE timestamp < ?1 AND payload_purged=0 AND decision_summary_ref IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![cutoff.to_rfc3339()])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((
                TaskId::new(row.get::<_, String>(0)?),
                PayloadRef::new(row.get::<_, String>(1)?),
            ));
        }
        Ok(out)
    }

    fn search_tasks_by_vector(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        let mut stmt = self.conn.prepare(
            "SELECT te.task_id, te.vec, tn.created_at \
             FROM task_embeddings te JOIN task_nodes tn ON tn.id = te.task_id",
        )?;
        let mut rows = stmt.query([])?;
        let mut hits: Vec<VectorHit> = Vec::new();
        while let Some(row) = rows.next()? {
            let task_id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let created_at: String = row.get(2)?;
            let vector = decode_vector(&blob);
            hits.push(VectorHit {
                task_id: TaskId::new(task_id),
                cosine: cosine(query, &vector),
                created_at: parse_ts(&created_at)?,
            });
        }
        hits.sort_by(|a, b| {
            b.cosine
                .partial_cmp(&a.cosine)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(k);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain0_model::{DeclaredChange, Drift};
    use chrono::TimeZone;

    fn ts(secs: i64) -> Timestamp {
        chrono::Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    fn artifact(id: &str, path: &str, parent: Option<&str>, level: Level) -> ArtifactNode {
        ArtifactNode {
            id: ArtifactId::new(id),
            level,
            repo: "repo".into(),
            qualified_path: path.into(),
            lang: Some(Lang::new("python")),
            parent_id: parent.map(ArtifactId::new),
            current_version: VersionId::new("ver_cur"),
            risk: RiskState::default(),
        }
    }

    fn av(id: &str, artifact_id: &str, secs: i64) -> ArtifactVersion {
        ArtifactVersion {
            id: VersionId::new(id),
            artifact_id: ArtifactId::new(artifact_id),
            timestamp: ts(secs),
            author: Author::new("Ada"),
            agent: Agent::new("claude-code"),
            source: ChangeSource::Git {
                commit_sha: format!("sha-{id}"),
            },
            qualified_path: "m.py::f".into(),
            fingerprint: "fp".into(),
            change_kind: ChangeKind::Modified,
            lines_added: 1,
            lines_removed: 0,
            diff_ref: None,
        }
    }

    fn task(id: &str, secs: i64) -> TaskNode {
        TaskNode {
            id: TaskId::new(id),
            session_id: SessionId::new("sess"),
            agent: Agent::new("claude-code"),
            author: Author::new("Ada"),
            created_at: ts(secs),
            current_version: VersionId::new("ver_cur"),
            source_adapter: Some("codex".into()),
            session_cwd: Some("/home/dev/proj".into()),
            model: Some("qwen3:4b".into()),
            reviewers: vec!["Grace <grace@x.io>".into()],
        }
    }

    #[test]
    fn artifact_node_roundtrip_and_find() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let node = artifact("art_f", "m.py::f", Some("art_file"), Level::Symbol);
        s.put_artifact_node(&node).unwrap();
        assert_eq!(
            s.get_artifact_node(&ArtifactId::new("art_f")).unwrap(),
            Some(node.clone())
        );
        assert_eq!(
            s.find_artifact_by_path("repo", Level::Symbol, "m.py::f")
                .unwrap(),
            Some(node)
        );
        assert!(s
            .get_artifact_node(&ArtifactId::new("missing"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn versions_are_append_only_and_ordered() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.append_artifact_version(&av("ver_2", "art_f", 200))
            .unwrap();
        s.append_artifact_version(&av("ver_1", "art_f", 100))
            .unwrap();
        let chain = s.artifact_versions(&ArtifactId::new("art_f")).unwrap();
        assert_eq!(chain.len(), 2);
        // Oldest first (timeline axis).
        assert_eq!(chain[0].id, VersionId::new("ver_1"));
        assert_eq!(chain[1].id, VersionId::new("ver_2"));

        // Re-appending the same id is idempotent — never destructive.
        s.append_artifact_version(&av("ver_1", "art_f", 100))
            .unwrap();
        assert_eq!(
            s.artifact_versions(&ArtifactId::new("art_f"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn artifact_version_roundtrip_with_rename() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let mut v = av("ver_r", "art_f", 100);
        v.change_kind = ChangeKind::Renamed {
            from: "m.py::old".into(),
        };
        s.append_artifact_version(&v).unwrap();
        assert_eq!(
            s.get_artifact_version(&VersionId::new("ver_r")).unwrap(),
            Some(v)
        );
    }

    #[test]
    fn children_query() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_artifact_node(&artifact("art_file", "m.py", None, Level::File))
            .unwrap();
        s.put_artifact_node(&artifact(
            "art_a",
            "m.py::a",
            Some("art_file"),
            Level::Symbol,
        ))
        .unwrap();
        s.put_artifact_node(&artifact(
            "art_b",
            "m.py::b",
            Some("art_file"),
            Level::Symbol,
        ))
        .unwrap();
        let kids = s.children(&ArtifactId::new("art_file")).unwrap();
        assert_eq!(kids.len(), 2);
    }

    #[test]
    fn edges_out_and_in() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let edge = Edge::TaskModifiesArtifact {
            task: TaskId::new("tsk_1"),
            artifact: ArtifactId::new("art_f"),
            version: VersionId::new("ver_1"),
            change_kind: ChangeKind::Added,
            lines_added: 5,
            lines_removed: 0,
        };
        s.put_edge(&edge).unwrap();
        s.put_edge(&edge).unwrap(); // idempotent
        let out = s
            .out_edges(EdgeKind::TaskModifiesArtifact, "tsk_1")
            .unwrap();
        assert_eq!(out, vec![edge.clone()]);
        let inn = s.in_edges(EdgeKind::TaskModifiesArtifact, "art_f").unwrap();
        assert_eq!(inn, vec![edge]);
    }

    #[test]
    fn task_node_and_versions_with_drift() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_task_node(&task("tsk_1", 100)).unwrap();
        assert_eq!(
            s.get_task_node(&TaskId::new("tsk_1")).unwrap(),
            Some(task("tsk_1", 100))
        );
        let tv = TaskVersion {
            id: VersionId::new("ver_t1"),
            task_id: TaskId::new("tsk_1"),
            timestamp: ts(100),
            prompt_ref: Some(PayloadRef::new("blake3:abc")),
            decision_summary_ref: None,
            declared: vec![DeclaredChange::new("m.py")],
            drift: Some(Drift::new(0.5, vec!["other.py".into()], vec![])),
            // Audit: files read this turn (incl. an out-of-repo secret) round-trip through storage.
            reads: vec!["src/util.py".into(), "/home/dev/.env".into()],
            read_secrets: vec![brain0_model::ReadSecret {
                path: "/home/dev/.env".into(),
                kinds: vec!["env_secret".into()],
            }],
        };
        s.append_task_version(&tv).unwrap();
        let got = s.task_versions(&TaskId::new("tsk_1")).unwrap();
        assert_eq!(got, vec![tv]);
    }

    #[test]
    fn vector_search_ranks_by_cosine() {
        let s = SqliteStorage::open_in_memory().unwrap();
        for (i, id) in ["tsk_a", "tsk_b", "tsk_c"].iter().enumerate() {
            s.put_task_node(&task(id, 100 + i as i64)).unwrap();
        }
        s.put_task_embedding(&TaskId::new("tsk_a"), &[1.0, 0.0, 0.0])
            .unwrap();
        s.put_task_embedding(&TaskId::new("tsk_b"), &[0.0, 1.0, 0.0])
            .unwrap();
        s.put_task_embedding(&TaskId::new("tsk_c"), &[0.9, 0.1, 0.0])
            .unwrap();

        let hits = s.search_tasks_by_vector(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].task_id, TaskId::new("tsk_a")); // exact match first
        assert_eq!(hits[1].task_id, TaskId::new("tsk_c")); // near match second
    }

    #[test]
    fn task_node_carries_source_adapter_and_cwd() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_task_node(&task("tsk_1", 100)).unwrap();
        let got = s.get_task_node(&TaskId::new("tsk_1")).unwrap().unwrap();
        assert_eq!(got.source_adapter.as_deref(), Some("codex"));
        assert_eq!(got.session_cwd.as_deref(), Some("/home/dev/proj"));
        assert_eq!(got.model.as_deref(), Some("qwen3:4b"));
        assert_eq!(got.reviewers, vec!["Grace <grace@x.io>".to_owned()]);
    }

    #[test]
    fn purge_keeps_topology_and_invalidates_derivatives() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_task_node(&task("tsk_1", 100)).unwrap();
        let r = PayloadRef::new("blake3:secretref");
        s.append_task_version(&TaskVersion {
            id: VersionId::new("ver_t1"),
            task_id: TaskId::new("tsk_1"),
            timestamp: ts(100),
            prompt_ref: Some(r.clone()),
            decision_summary_ref: None,
            declared: vec![],
            drift: None,
            reads: vec![],
            read_secrets: vec![],
        })
        .unwrap();
        s.put_task_embedding(&TaskId::new("tsk_1"), &[1.0, 0.0])
            .unwrap();

        // Purge the payload + invalidate the derived embedding.
        s.mark_payload_purged(&r).unwrap();
        s.delete_task_embedding(&TaskId::new("tsk_1")).unwrap();

        // Topology intact: the task version still exists (append-only graph unbroken).
        assert_eq!(s.task_versions(&TaskId::new("tsk_1")).unwrap().len(), 1);
        // Derived embedding gone → no longer searchable (secret can't survive in the vector).
        assert!(s.search_tasks_by_vector(&[1.0, 0.0], 5).unwrap().is_empty());
    }

    #[test]
    fn audit_log_is_append_only_recent_first() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.append_audit("redaction", "kind=aws_access_key file=f", ts(100))
            .unwrap();
        s.append_audit("purge", "node=art_x", ts(200)).unwrap();
        let events = s.list_audit(10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, "purge"); // newest first
        assert_eq!(events[1].event_type, "redaction");
        // No secret value ever stored — only kinds/detail provided by the caller.
        assert!(events[1].detail.contains("aws_access_key"));
    }

    #[test]
    fn cursor_roundtrip() {
        let s = SqliteStorage::open_in_memory().unwrap();
        assert_eq!(s.get_cursor("codex", "/x/a.jsonl").unwrap(), None);
        s.set_cursor("codex", "/x/a.jsonl", 4096).unwrap();
        assert_eq!(s.get_cursor("codex", "/x/a.jsonl").unwrap(), Some(4096));
        s.set_cursor("codex", "/x/a.jsonl", 8192).unwrap();
        assert_eq!(s.get_cursor("codex", "/x/a.jsonl").unwrap(), Some(8192));
        // Distinct (adapter, file) keys are independent.
        assert_eq!(s.get_cursor("claude-code", "/x/a.jsonl").unwrap(), None);
    }

    #[test]
    fn artifact_fingerprint_roundtrip() {
        let s = SqliteStorage::open_in_memory().unwrap();
        let id = ArtifactId::new("art_f");
        assert!(s.get_artifact_fingerprint(&id).unwrap().is_none());
        s.put_artifact_fingerprint(&id, "deadbeef", &[1, 2, 3])
            .unwrap();
        assert_eq!(
            s.get_artifact_fingerprint(&id).unwrap(),
            Some(("deadbeef".to_owned(), vec![1, 2, 3]))
        );
        // Upsert replaces.
        s.put_artifact_fingerprint(&id, "cafe", &[9]).unwrap();
        assert_eq!(
            s.get_artifact_fingerprint(&id).unwrap(),
            Some(("cafe".to_owned(), vec![9]))
        );
    }

    #[test]
    fn embedding_meta_fixes_dimension_per_store() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_task_node(&task("tsk_1", 100)).unwrap();
        assert_eq!(s.get_embedding_meta().unwrap(), None);
        s.set_embedding_meta("qwen3-embedding:0.6b", 3).unwrap();
        assert_eq!(
            s.get_embedding_meta().unwrap(),
            Some(("qwen3-embedding:0.6b".to_owned(), 3))
        );
        // Matching dim is accepted.
        s.put_task_embedding(&TaskId::new("tsk_1"), &[0.1, 0.2, 0.3])
            .unwrap();
        // Mismatched dim is rejected (no mixing).
        assert!(s
            .put_task_embedding(&TaskId::new("tsk_1"), &[0.1, 0.2])
            .is_err());
    }

    #[test]
    fn reembed_migration_replaces_dimension() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_task_node(&task("tsk_1", 100)).unwrap();
        // Old model: dim 3.
        s.set_embedding_meta("old-model", 3).unwrap();
        s.put_task_embedding(&TaskId::new("tsk_1"), &[0.1, 0.2, 0.3])
            .unwrap();
        // Migration: clear, switch model/dim, re-embed at the new dimension.
        s.clear_all_embeddings().unwrap();
        s.set_embedding_meta("new-model", 2).unwrap();
        assert_eq!(s.all_task_ids().unwrap(), vec![TaskId::new("tsk_1")]);
        s.put_task_embedding(&TaskId::new("tsk_1"), &[0.5, 0.5])
            .unwrap(); // new dim ok
        assert_eq!(
            s.get_embedding_meta().unwrap(),
            Some(("new-model".to_owned(), 2))
        );
    }

    #[test]
    fn payload_encryption_meta_roundtrip() {
        let s = SqliteStorage::open_in_memory().unwrap();
        // Legacy index (flag absent) → None, so callers fail closed to encrypted.
        assert_eq!(s.get_payload_encryption().unwrap(), None);
        s.set_payload_encryption(true).unwrap();
        assert_eq!(s.get_payload_encryption().unwrap(), Some(true));
        // Idempotent overwrite to plaintext.
        s.set_payload_encryption(false).unwrap();
        assert_eq!(s.get_payload_encryption().unwrap(), Some(false));
    }

    #[test]
    fn risk_update_persists() {
        let s = SqliteStorage::open_in_memory().unwrap();
        s.put_artifact_node(&artifact("art_f", "m.py::f", None, Level::Symbol))
            .unwrap();
        s.update_artifact_risk(&ArtifactId::new("art_f"), RiskState::new(0.2, 0.8))
            .unwrap();
        let node = s
            .get_artifact_node(&ArtifactId::new("art_f"))
            .unwrap()
            .unwrap();
        assert!((node.risk.apriori - 0.2).abs() < 1e-6);
        assert!((node.risk.aposteriori - 0.8).abs() < 1e-6);
    }
}
