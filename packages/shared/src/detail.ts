/**
 * Node detail: the on-demand hydration behind a click in the GUI.
 *
 * The graph snapshot is deliberately light (only the navigable index). This builds the richer
 * per-node view from the index: full version history (who committed/when/what kind of change/
 * ±lines/source commit; for tasks, the declared changes and reconciliation drift), the small
 * payload text (prompt, decision summary) hydrated by reference, and — for artifacts — the
 * **intents** (agent tasks) that touched the file. The heavy **diff** text is *not* hydrated
 * here; only its reference is included so the GUI can lazy-load it on demand (see `/api/diff`).
 */

import type { Brain0Store } from "./store.js";
import type { Level, RiskState } from "./types.js";

export type GetText = (ref: string) => Promise<string | undefined>;

/** One version in a node's history (an artifact change, or a task intent). */
export interface DetailVersion {
  timestamp: string;
  /** Artifact: the git committer / author of record (a fact). */
  committer?: string;
  /** Artifact: added/modified/deleted/renamed/moved. */
  changeKind?: string;
  linesAdded?: number;
  linesRemoved?: number;
  /** Artifact: the objective source, e.g. `git:abc1234567`. */
  source?: string;
  /** Artifact: reference to the full diff in the payload store (lazy-loaded via `/api/diff`). */
  diffRef?: string;
  /** Artifact: set when `diffRef` is the *containing file's* diff (symbols store no own diff). */
  diffOfPath?: string;
  /** Task: the paths this intent declared it would change. */
  declared?: string[];
  /** Task: changed-but-not-declared paths (reconciliation drift). */
  driftUndeclared?: string[];
  /** Task: declared-but-not-observed paths (reconciliation drift). */
  driftPhantom?: string[];
  /** Task: the hydrated decision summary, if readable. (The full prompt is no longer persisted.) */
  summary?: string;
}

/** An intent (agent task) that modified an artifact — the real authorship, via reconciliation. */
export interface IntentRef {
  taskId: string;
  agent: string;
  author: string;
  when: string;
}

/** One file changed by a commit (git-style), with a lazy diff reference. */
export interface ChangedFile {
  artifactId: string;
  path: string;
  changeKind?: string;
  linesAdded?: number;
  linesRemoved?: number;
  /** Reference to the file's diff in the payload store (lazy-loaded via `/api/diff`). */
  diffRef?: string;
}

/** An agent prompt behind a commit — only the model-generated summary, never the raw prompt. */
export interface PromptRef {
  taskId: string;
  summary?: string;
  agent: string;
  author: string;
  when: string;
}

/** The full, hydrated detail for a clicked node. */
export interface NodeDetail {
  id: string;
  kind: "task" | "artifact" | "commit";
  label: string;
  level?: Level;
  risk?: RiskState;
  /** Artifact: qualified path. */
  path?: string;
  /** Artifact: number of contained children (e.g. symbols in a file). */
  childCount?: number;
  /** Artifact: the intents (agent/human tasks) that changed this artifact (reconciliation). */
  intents?: IntentRef[];
  /** Task/commit: who/what/when from the node. */
  agent?: string;
  author?: string;
  createdAt?: string;
  /** Commit: the commit message (hydrated decision summary). */
  message?: string;
  /** Commit: the objective source, e.g. `git:abc1234567`. */
  source?: string;
  /** Commit: the files this commit changed (git-style), with lazy diff refs. */
  changedFiles?: ChangedFile[];
  /** Commit: the agent prompts behind this commit (shared-version join), summaries only. */
  prompts?: PromptRef[];
  /** Commit: files the sessions behind this commit READ (audit: what reached the model). Paths
   * only; absolute paths are reads from outside the repo (the audit red flags). */
  reads?: string[];
  /** Commit: reads whose CONTENT held secrets (DLP) — path + detected kinds, never values. */
  readSecrets?: Array<{ path: string; kinds: string[] }>;
  versions: DetailVersion[];
  /** A human note, e.g. that payload text is encrypted and cannot be previewed. */
  note?: string;
}

/**
 * Returns the text if it looks like readable content, else `undefined` — used to avoid showing
 * encrypted ciphertext (read as UTF-8 it is mostly replacement/control characters).
 */
export function readablePayload(text: string | undefined): string | undefined {
  if (text === undefined) return undefined;
  let bad = 0;
  for (const ch of text) {
    const c = ch.codePointAt(0) ?? 0;
    if (c === 0xfffd || c < 0x09 || (c > 0x7e && c < 0xa0)) bad++;
  }
  return bad / Math.max(text.length, 1) > 0.1 ? undefined : text;
}

/** Build the hydrated detail for a node id, or `undefined` if it is neither artifact nor task. */
export async function buildNodeDetail(
  store: Brain0Store,
  getText: GetText,
  id: string,
): Promise<NodeDetail | undefined> {
  const artifact = store.getArtifact(id);
  if (artifact) {
    // Only files store their own diff (git diffs a file). A symbol's change lives inside its
    // containing file, so when a version has no own diff we fall back to the parent file's diff
    // for the same commit — indexed here by source ref, with the file's latest as a last resort.
    const parent = artifact.parentId ? store.getArtifact(artifact.parentId) : undefined;
    const parentDiffByRef = new Map<string, string>();
    let parentLatestDiff: string | undefined;
    if (parent) {
      for (const pv of store.artifactVersions(parent.id)) {
        if (pv.diffRef) {
          parentDiffByRef.set(pv.source.ref, pv.diffRef);
          parentLatestDiff = pv.diffRef; // versions are time-ordered; the last with a diff wins
        }
      }
    }

    const versions: DetailVersion[] = store.artifactVersions(id).map((v) => {
      let diffRef = v.diffRef;
      let diffOfPath: string | undefined;
      if (!diffRef && parent) {
        diffRef = parentDiffByRef.get(v.source.ref) ?? parentLatestDiff;
        if (diffRef) diffOfPath = parent.qualifiedPath;
      }
      return {
        timestamp: v.timestamp,
        committer: v.author?.name,
        changeKind: v.changeKind,
        linesAdded: v.linesAdded,
        linesRemoved: v.linesRemoved,
        source: `${v.source.kind}:${v.source.ref.slice(0, 10)}`,
        diffRef,
        diffOfPath,
      };
    });

    // The real authorship of a change: the agent intents reconciled to this artifact. A bare git
    // commit only records its committer; whether an agent wrote it is known only via these links.
    const intents: IntentRef[] = [];
    for (const edge of store.inEdges("task_modifies_artifact", id)) {
      const taskId = String((edge as { task?: unknown }).task ?? "");
      const task = taskId ? store.getTask(taskId) : undefined;
      // Only agent intents — not the commit task itself (which is the committer, shown elsewhere).
      if (task && task.sourceAdapter !== undefined) {
        intents.push({
          taskId,
          agent: task.agent?.name ?? "",
          author: task.author?.name ?? "",
          when: task.createdAt,
        });
      }
    }

    return {
      id,
      kind: "artifact",
      label: artifact.qualifiedPath,
      level: artifact.level,
      risk: artifact.risk,
      path: artifact.qualifiedPath,
      childCount: store.children(id).length,
      intents,
      versions,
    };
  }

  const task = store.getTask(id);
  if (task) {
    // A commit/observer task has no source adapter. It becomes a "commit" detail: the commit
    // message, the files it changed (git-style), and the agent prompts behind it.
    if (task.sourceAdapter === undefined) {
      let encrypted = false;
      let message: string | undefined;
      for (const v of store.taskVersions(id)) {
        if (!v.decisionSummaryRef) continue;
        const raw = await getText(v.decisionSummaryRef);
        const text = readablePayload(raw);
        if (text) message = text; // the observer stores the commit message as the decision summary
        else if (raw !== undefined) encrypted = true;
      }

      // Files changed: the file-level artifacts this commit modified, each with its change kind,
      // ±lines and a lazy diff. Per-file facts come from the artifact version the edge points at
      // (more reliable than the edge's serialized attributes). We also remember every artifact the
      // commit touched, to find the prompts behind it.
      const changedFiles: ChangedFile[] = [];
      const touched = new Set<string>();
      // The specific artifact versions THIS commit produced. The "prompts behind it" join matches
      // on these versions (not merely the artifact), so it surfaces only the agent sessions the
      // reconciler time-correlated to this commit — never every older session that once touched
      // one of the same files.
      const touchedVersions = new Set<string>();
      for (const edge of store.outEdges("task_modifies_artifact", id)) {
        const artifactId = String((edge as { artifact?: unknown }).artifact ?? "");
        if (!artifactId) continue;
        touched.add(artifactId);
        const versionId = String((edge as { version?: unknown }).version ?? "");
        if (versionId) touchedVersions.add(versionId);
        const art = store.getArtifact(artifactId);
        if (!art || art.level !== "file") continue; // list files; symbols are reached by drilling in
        const avs = store.artifactVersions(artifactId);
        const v = avs.find((x) => x.id === versionId) ?? avs[avs.length - 1];
        changedFiles.push({
          artifactId,
          path: art.qualifiedPath,
          changeKind: v?.changeKind,
          linesAdded: v?.linesAdded,
          linesRemoved: v?.linesRemoved,
          diffRef: v?.diffRef,
        });
      }
      changedFiles.sort((a, b) => a.path.localeCompare(b.path));

      // Prompts behind this commit: the agent tasks the reconciler correlated to a version THIS
      // commit produced (a read-time join over existing edges, matched on the shared version).
      // Matching the version — not merely a shared file — is what keeps unrelated older sessions
      // out: a file touched across many commits would otherwise attach every session that ever
      // edited it. Gap-filling already time-correlated the agent turn to the observed version
      // (±30 min), so the shared version is the precise, principled link.
      const promptIds = new Set<string>();
      for (const artifactId of touched) {
        for (const edge of store.inEdges("task_modifies_artifact", artifactId)) {
          const other = String((edge as { task?: unknown }).task ?? "");
          const ev = String((edge as { version?: unknown }).version ?? "");
          if (other && other !== id && ev && touchedVersions.has(ev)) promptIds.add(other);
        }
      }
      const prompts: PromptRef[] = [];
      // Audit trail: union the files those sessions READ (what reached the model), across turns,
      // plus the DLP red flags — reads whose CONTENT held secrets (kinds only, never values).
      const readsSet = new Set<string>();
      const secretKinds = new Map<string, Set<string>>();
      for (const promptId of promptIds) {
        const t = store.getTask(promptId);
        if (!t || t.sourceAdapter === undefined) continue; // only agent prompts, not other commits
        let summary: string | undefined;
        for (const v of store.taskVersions(promptId)) {
          for (const r of v.reads ?? []) readsSet.add(r);
          for (const rs of v.readSecrets ?? []) {
            const set = secretKinds.get(rs.path) ?? new Set<string>();
            for (const k of rs.kinds) set.add(k);
            secretKinds.set(rs.path, set);
          }
          if (v.decisionSummaryRef) {
            const text = readablePayload(await getText(v.decisionSummaryRef));
            if (text) summary = text; // keep the latest readable summary
          }
        }
        prompts.push({
          taskId: promptId,
          summary,
          agent: t.agent?.name ?? "",
          author: t.author?.name ?? "",
          when: t.createdAt,
        });
      }
      prompts.sort((a, b) => a.when.localeCompare(b.when));
      const reads = [...readsSet].sort();
      const readSecrets = [...secretKinds.entries()]
        .map(([path, kinds]) => ({ path, kinds: [...kinds].sort() }))
        .sort((a, b) => a.path.localeCompare(b.path));

      return {
        id,
        kind: "commit",
        label: id,
        agent: task.agent?.name,
        author: task.author?.name,
        createdAt: task.createdAt,
        message,
        source: `git:${task.sessionId.slice(0, 10)}`,
        changedFiles,
        prompts,
        reads,
        readSecrets,
        versions: [],
        note: encrypted
          ? "payload encrypted — re-run observe with --no-encrypt-payload to preview the message"
          : undefined,
      };
    }

    // An agent-prompt task: summary only (the full prompt is no longer persisted,).
    let encrypted = false;
    const versions: DetailVersion[] = [];
    for (const v of store.taskVersions(id)) {
      let summary: string | undefined;
      if (v.decisionSummaryRef) {
        const raw = await getText(v.decisionSummaryRef);
        summary = readablePayload(raw);
        if (raw !== undefined && summary === undefined) encrypted = true;
      }
      versions.push({
        timestamp: v.timestamp,
        declared: v.declared.map((d) => d.path),
        driftUndeclared: v.drift?.undeclared,
        driftPhantom: v.drift?.phantom,
        summary,
      });
    }
    return {
      id,
      kind: "task",
      label: id,
      agent: task.agent?.name,
      author: task.author?.name,
      createdAt: task.createdAt,
      versions,
      note: encrypted
        ? "payload encrypted — re-run observe with --no-encrypt-payload to preview summaries"
        : undefined,
    };
  }

  return undefined;
}
