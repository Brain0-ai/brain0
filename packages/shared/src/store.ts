/**
 * Read-client over the brain0 index — the TypeScript side of the abstract storage. Uses
 * Node's built-in `node:sqlite` (no native module) to read the same SQLite database the
 * Rust core writes. Vector search is a cosine scan in JS, mirroring the Rust backend.
 *
 * Run Node with `--experimental-sqlite`.
 */

import { DatabaseSync } from "node:sqlite";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import type {
  ArtifactNode,
  ArtifactVersion,
  ChangeSource,
  Edge,
  EdgeKind,
  Level,
  TaskNode,
  TaskVersion,
  VectorHit,
} from "./types.js";

type Row = Record<string, unknown>;

const ARTIFACT_COLS =
  "id, level, repo, qualified_path, lang, parent_id, current_version, risk_apriori, risk_aposteriori";
const TASK_COLS =
  "id, session_id, agent_name, agent_version, author_name, author_email, created_at, current_version, source_adapter, session_cwd";
const AV_COLS =
  "id, artifact_id, timestamp, author_name, author_email, agent_name, agent_version, source_kind, source_ref, qualified_path, fingerprint, change_kind, change_from, lines_added, lines_removed, diff_ref";
const TV_COLS =
  "id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json, reads_json, read_secrets_json";

function str(value: unknown): string {
  return value == null ? "" : String(value);
}
function optStr(value: unknown): string | undefined {
  return value == null ? undefined : String(value);
}
function num(value: unknown): number {
  return typeof value === "number" ? value : Number(value ?? 0);
}

/** Encode an f32 vector as little-endian bytes (matches the Rust BLOB layout). */
export function encodeVector(vector: number[]): Uint8Array {
  const buf = new Uint8Array(vector.length * 4);
  const view = new DataView(buf.buffer);
  vector.forEach((v, i) => view.setFloat32(i * 4, v, true));
  return buf;
}

/** Decode little-endian bytes into an f32 vector. */
export function decodeVector(bytes: Uint8Array): number[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const out: number[] = [];
  for (let i = 0; i + 4 <= bytes.byteLength; i += 4) out.push(view.getFloat32(i, true));
  return out;
}

export function cosine(a: number[], b: number[]): number {
  if (a.length !== b.length) return 0;
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < a.length; i++) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    dot += x * y;
    na += x * x;
    nb += y * y;
  }
  if (na === 0 || nb === 0) return 0;
  return dot / (Math.sqrt(na) * Math.sqrt(nb));
}

function mapArtifact(row: Row): ArtifactNode {
  return {
    id: str(row.id),
    level: str(row.level) as Level,
    repo: str(row.repo),
    qualifiedPath: str(row.qualified_path),
    lang: optStr(row.lang),
    parentId: optStr(row.parent_id),
    currentVersion: str(row.current_version),
    risk: { apriori: num(row.risk_apriori), aposteriori: num(row.risk_aposteriori) },
  };
}

function mapTask(row: Row): TaskNode {
  return {
    id: str(row.id),
    sessionId: str(row.session_id),
    agent: { name: str(row.agent_name), version: optStr(row.agent_version) },
    author: { name: str(row.author_name), email: optStr(row.author_email) },
    createdAt: str(row.created_at),
    currentVersion: str(row.current_version),
    sourceAdapter: optStr(row.source_adapter),
    sessionCwd: optStr(row.session_cwd),
  };
}

function mapArtifactVersion(row: Row): ArtifactVersion {
  const source: ChangeSource = {
    kind: str(row.source_kind) === "git" ? "git" : "checkpoint",
    ref: str(row.source_ref),
  };
  return {
    id: str(row.id),
    artifactId: str(row.artifact_id),
    timestamp: str(row.timestamp),
    author: { name: str(row.author_name), email: optStr(row.author_email) },
    agent: { name: str(row.agent_name), version: optStr(row.agent_version) },
    source,
    qualifiedPath: str(row.qualified_path),
    fingerprint: str(row.fingerprint),
    changeKind: str(row.change_kind),
    changeFrom: optStr(row.change_from),
    linesAdded: num(row.lines_added),
    linesRemoved: num(row.lines_removed),
    diffRef: optStr(row.diff_ref),
  };
}

function mapTaskVersion(row: Row): TaskVersion {
  return {
    id: str(row.id),
    taskId: str(row.task_id),
    timestamp: str(row.timestamp),
    promptRef: optStr(row.prompt_ref),
    decisionSummaryRef: optStr(row.decision_summary_ref),
    declared: row.declared_json ? JSON.parse(str(row.declared_json)) : [],
    drift: row.drift_json ? JSON.parse(str(row.drift_json)) : undefined,
    reads: row.reads_json ? JSON.parse(str(row.reads_json)) : [],
    readSecrets: row.read_secrets_json ? JSON.parse(str(row.read_secrets_json)) : [],
  };
}

/** Locate the shared SQLite schema relative to this module (dev/source layout). */
function schemaPath(): string {
  return fileURLToPath(new URL("../../../schema/sqlite.sql", import.meta.url));
}

/** Read-client over the brain0 index. */
export class Brain0Store {
  private readonly db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    // Mirror the Rust writer (crates/brain0-storage/src/sqlite.rs:49): give reads and the
    // post-refresh embedding backfill a retry budget under WAL instead of throwing SQLITE_BUSY
    // when a separate `brain0 ingest/observe` writer holds the lock briefly.
    this.db.exec("PRAGMA busy_timeout=5000");
  }

  /** Apply the shared schema (idempotent) — mainly for tests; the core normally migrates. */
  migrate(): void {
    this.db.exec(readFileSync(schemaPath(), "utf8"));
  }

  /** Direct access to the underlying connection (e.g. to seed test data). */
  raw(): DatabaseSync {
    return this.db;
  }

  close(): void {
    this.db.close();
  }

  getTask(id: string): TaskNode | undefined {
    const row = this.db.prepare(`SELECT ${TASK_COLS} FROM task_nodes WHERE id=?`).get(id) as
      | Row
      | undefined;
    return row ? mapTask(row) : undefined;
  }

  /** Commit task ids whose git SHA (stored as `session_id`) starts with `prefix` — for resolving
   *  an explicit commit referenced by SHA in a query. Commit tasks have no source adapter. */
  commitTaskIdsByShaPrefix(prefix: string): string[] {
    const rows = this.db
      .prepare(
        "SELECT id FROM task_nodes WHERE source_adapter IS NULL AND session_id LIKE ? ORDER BY created_at",
      )
      .all(`${prefix}%`) as Row[];
    return rows.map((r) => str(r.id));
  }

  taskVersions(taskId: string): TaskVersion[] {
    const rows = this.db
      .prepare(`SELECT ${TV_COLS} FROM task_versions WHERE task_id=? ORDER BY timestamp, id`)
      .all(taskId) as Row[];
    return rows.map(mapTaskVersion);
  }

  getArtifact(id: string): ArtifactNode | undefined {
    const row = this.db
      .prepare(`SELECT ${ARTIFACT_COLS} FROM artifact_nodes WHERE id=?`)
      .get(id) as Row | undefined;
    return row ? mapArtifact(row) : undefined;
  }

  /** All artifacts in a repo at a given level (e.g. all symbols), for audit/aggregation. */
  listArtifacts(repo: string, level: Level): ArtifactNode[] {
    const rows = this.db
      .prepare(`SELECT ${ARTIFACT_COLS} FROM artifact_nodes WHERE repo=? AND level=? ORDER BY qualified_path`)
      .all(repo, level) as Row[];
    return rows.map(mapArtifact);
  }

  children(parentId: string): ArtifactNode[] {
    const rows = this.db
      .prepare(`SELECT ${ARTIFACT_COLS} FROM artifact_nodes WHERE parent_id=? ORDER BY qualified_path`)
      .all(parentId) as Row[];
    return rows.map(mapArtifact);
  }

  /** Version chain for an artifact, oldest first (the timeline axis). */
  artifactVersions(artifactId: string): ArtifactVersion[] {
    const rows = this.db
      .prepare(`SELECT ${AV_COLS} FROM artifact_versions WHERE artifact_id=? ORDER BY timestamp, id`)
      .all(artifactId) as Row[];
    return rows.map(mapArtifactVersion);
  }

  outEdges(kind: EdgeKind, src: string): Edge[] {
    const rows = this.db
      .prepare("SELECT attrs_json FROM edges WHERE kind=? AND src=?")
      .all(kind, src) as Row[];
    return rows.map((r) => JSON.parse(str(r.attrs_json)) as Edge);
  }

  inEdges(kind: EdgeKind, dst: string): Edge[] {
    const rows = this.db
      .prepare("SELECT attrs_json FROM edges WHERE kind=? AND dst=?")
      .all(kind, dst) as Row[];
    return rows.map((r) => JSON.parse(str(r.attrs_json)) as Edge);
  }

  /** Task ids that have no embedding yet (the indexer's work-list). */
  tasksMissingEmbeddings(): string[] {
    const rows = this.db
      .prepare(
        "SELECT t.id FROM task_nodes t LEFT JOIN task_embeddings e ON e.task_id=t.id WHERE e.task_id IS NULL",
      )
      .all() as Row[];
    return rows.map((r) => str(r.id));
  }

  putTaskEmbedding(taskId: string, vector: number[]): void {
    this.db
      .prepare("INSERT OR REPLACE INTO task_embeddings (task_id, dim, vec) VALUES (?,?,?)")
      .run(taskId, vector.length, encodeVector(vector));
  }

  /** Top-k task nodes by cosine similarity to the query embedding. */
  searchTasksByVector(query: number[], k: number): VectorHit[] {
    const rows = this.db
      .prepare(
        "SELECT te.task_id AS task_id, te.vec AS vec, tn.created_at AS created_at " +
          "FROM task_embeddings te JOIN task_nodes tn ON tn.id = te.task_id",
      )
      .all() as Row[];
    const hits: VectorHit[] = rows.map((r) => ({
      taskId: str(r.task_id),
      cosine: cosine(query, decodeVector(r.vec as Uint8Array)),
      createdAt: str(r.created_at),
    }));
    hits.sort((a, b) => b.cosine - a.cosine);
    return hits.slice(0, k);
  }
}
