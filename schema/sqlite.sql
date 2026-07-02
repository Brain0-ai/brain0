-- brain0 index schema — SQLite dialect (the open-core local backend).
-- The Postgres dialect of this schema lives in the private brain0-enterprise repo;
-- both implement the same brain0_storage::Storage trait.
-- The graph is append-only: *_versions and edges are insert-only; node rows carry the
-- evolving "current" pointers and risk scalars.

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Artifact nodes (persistent code entities: symbols, or files in fallback).
CREATE TABLE IF NOT EXISTS artifact_nodes (
    id               TEXT PRIMARY KEY,
    level            TEXT NOT NULL,           -- repo|module|file|symbol
    repo             TEXT NOT NULL,
    qualified_path   TEXT NOT NULL,
    lang             TEXT,
    parent_id        TEXT,
    current_version  TEXT NOT NULL,
    risk_apriori     REAL NOT NULL DEFAULT 0,
    risk_aposteriori REAL NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_artifact_repo_path ON artifact_nodes (repo, level, qualified_path);
CREATE INDEX IF NOT EXISTS idx_artifact_parent    ON artifact_nodes (parent_id);

-- Task nodes (intents: prompts / agent sessions).
CREATE TABLE IF NOT EXISTS task_nodes (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    agent_name      TEXT NOT NULL,
    agent_version   TEXT,
    author_name     TEXT NOT NULL,
    author_email    TEXT,
    created_at      TEXT NOT NULL,            -- RFC3339 UTC
    current_version TEXT NOT NULL,
    source_adapter  TEXT,                     -- which agent-artifact adapter produced this
    session_cwd     TEXT,                     -- working directory of the session (project scoping)
    model           TEXT,                     -- the model the agent used this session (DLP + provenance)
    reviewers_json  TEXT                      -- human reviewers from commit trailers (review status §2.3)
);
CREATE INDEX IF NOT EXISTS idx_task_session ON task_nodes (session_id);
CREATE INDEX IF NOT EXISTS idx_task_created ON task_nodes (created_at);

-- Dated artifact versions (append-only).
CREATE TABLE IF NOT EXISTS artifact_versions (
    id             TEXT PRIMARY KEY,
    artifact_id    TEXT NOT NULL,
    timestamp      TEXT NOT NULL,             -- RFC3339 UTC
    author_name    TEXT NOT NULL,
    author_email   TEXT,
    agent_name     TEXT NOT NULL,
    agent_version  TEXT,
    source_kind    TEXT NOT NULL,             -- git|checkpoint
    source_ref     TEXT NOT NULL,             -- commit sha or checkpoint id
    qualified_path TEXT NOT NULL,
    fingerprint    TEXT NOT NULL,
    change_kind    TEXT NOT NULL,             -- added|modified|deleted|renamed|moved
    change_from    TEXT,                      -- prior path for renamed/moved
    lines_added    INTEGER NOT NULL DEFAULT 0,
    lines_removed  INTEGER NOT NULL DEFAULT 0,
    diff_ref       TEXT,
    payload_purged INTEGER NOT NULL DEFAULT 0  -- tombstone: diff payload purged
);
CREATE INDEX IF NOT EXISTS idx_av_artifact_ts ON artifact_versions (artifact_id, timestamp);

-- Dated task versions (append-only).
CREATE TABLE IF NOT EXISTS task_versions (
    id                   TEXT PRIMARY KEY,
    task_id              TEXT NOT NULL,
    timestamp            TEXT NOT NULL,        -- RFC3339 UTC
    prompt_ref           TEXT,
    decision_summary_ref TEXT,
    declared_json        TEXT,                 -- JSON array of DeclaredChange
    drift_json           TEXT,                 -- JSON of Drift, or NULL
    reads_json           TEXT,                 -- JSON array of read file paths (audit: what reached the model), or NULL
    read_secrets_json    TEXT,                 -- JSON array of {path,kinds} for reads whose content held secrets (DLP), or NULL
    payload_purged       INTEGER NOT NULL DEFAULT 0  -- tombstone: payload purged/crypto-shredded
);
CREATE INDEX IF NOT EXISTS idx_tv_task_ts ON task_versions (task_id, timestamp);

-- Typed edges (append-only, idempotent on (kind, src, dst)).
CREATE TABLE IF NOT EXISTS edges (
    kind       TEXT NOT NULL,
    src        TEXT NOT NULL,
    dst        TEXT NOT NULL,
    attrs_json TEXT NOT NULL,                  -- full serialized Edge
    PRIMARY KEY (kind, src, dst)
);
CREATE INDEX IF NOT EXISTS idx_edges_src ON edges (kind, src);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges (kind, dst);

-- Task embeddings for semantic search (vector stored as little-endian f32 BLOB).
CREATE TABLE IF NOT EXISTS task_embeddings (
    task_id TEXT PRIMARY KEY,
    dim     INTEGER NOT NULL,
    vec     BLOB NOT NULL
);

-- Append-only ingest cursors: per source file, the byte offset processed so far, so a
-- months-long transcript is never re-read from the start.
CREATE TABLE IF NOT EXISTS cursors (
    adapter     TEXT NOT NULL,
    file        TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    PRIMARY KEY (adapter, file)
);

-- Current structural fingerprint per artifact (hash + shingles as little-endian u64 BLOB).
-- Kept out of artifact_nodes so node reads stay light; loaded only by the observer to
-- resolve identity / rename across snapshots.
CREATE TABLE IF NOT EXISTS artifact_fingerprints (
    artifact_id TEXT PRIMARY KEY,
    hash        TEXT NOT NULL,
    shingles    BLOB NOT NULL
);

-- Append-only security audit log (redactions, purges, admin actions). Never stores secret
-- values or payload — only the event kind and non-sensitive detail.
CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp  TEXT NOT NULL,
    event_type TEXT NOT NULL,
    detail     TEXT NOT NULL DEFAULT ''
);

-- Full-tree manifests per checkpoint (the `watch` safety net): every file's content lives in
-- the payload store (content-addressed → deduped across checkpoints); `brain0 rewind` restores
-- from here. Append-only like everything else.
CREATE TABLE IF NOT EXISTS checkpoint_files (
    checkpoint_id TEXT NOT NULL,
    rel_path      TEXT NOT NULL,
    content_ref   TEXT NOT NULL,
    at            TEXT NOT NULL,        -- RFC3339 UTC (checkpoint instant)
    PRIMARY KEY (checkpoint_id, rel_path)
);
CREATE INDEX IF NOT EXISTS idx_cpf_at ON checkpoint_files (at);
