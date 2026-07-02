/**
 * TypeScript mirror of the Rust `brain0-model` types, so the agent and GUI read the same
 * index the Rust core writes. Field names are camelCase; the storage layer maps to/from
 * the snake_case SQL columns.
 */

export const BRAIN0_SCHEMA_VERSION = 1 as const;

export type Level = "repo" | "module" | "file" | "symbol";

export type EdgeKind =
  | "task_modifies_artifact"
  | "artifact_contains"
  | "artifact_depends_on"
  | "artifact_version_succeeds"
  | "task_follows";

export interface Author {
  name: string;
  email?: string;
}

export interface Agent {
  name: string;
  version?: string;
}

export interface RiskState {
  apriori: number;
  aposteriori: number;
}

export interface ArtifactNode {
  id: string;
  level: Level;
  repo: string;
  qualifiedPath: string;
  lang?: string;
  parentId?: string;
  currentVersion: string;
  risk: RiskState;
}

export interface TaskNode {
  id: string;
  sessionId: string;
  agent: Agent;
  author: Author;
  createdAt: string; // RFC3339 UTC
  currentVersion: string;
  /** Which agent-artifact adapter produced this intent (PRD2), if any. */
  sourceAdapter?: string;
  /** Working directory of the originating session (PRD2 project scoping), if any. */
  sessionCwd?: string;
}

export type ChangeSource =
  | { kind: "git"; ref: string }
  | { kind: "checkpoint"; ref: string };

export interface ArtifactVersion {
  id: string;
  artifactId: string;
  timestamp: string;
  author: Author;
  agent: Agent;
  source: ChangeSource;
  qualifiedPath: string;
  fingerprint: string;
  changeKind: string;
  changeFrom?: string;
  linesAdded: number;
  linesRemoved: number;
  diffRef?: string;
}

export interface DeclaredChange {
  path: string;
  symbol?: string;
  intent?: string;
}

export interface Drift {
  score: number;
  undeclared: string[];
  phantom: string[];
}

export interface TaskVersion {
  id: string;
  taskId: string;
  timestamp: string;
  promptRef?: string;
  decisionSummaryRef?: string;
  declared: DeclaredChange[];
  drift?: Drift;
  /** Files the agent read this turn (audit: what reached the model). Paths only, never content. */
  reads: string[];
  /** Reads whose content held secrets — kinds only, never values (DLP). */
  readSecrets?: Array<{ path: string; kinds: string[] }>;
}

/** A typed edge as stored in the index (the full serialized form). */
export interface Edge {
  kind: EdgeKind;
  // Endpoints and attributes depend on kind; kept as an open record matching the Rust
  // serde tagged representation.
  [key: string]: unknown;
}

/** A semantic-search candidate. */
export interface VectorHit {
  taskId: string;
  cosine: number;
  createdAt: string;
}
