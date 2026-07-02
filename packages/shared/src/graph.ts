/**
 * Graph snapshot: a compact, render-ready projection of the index for the GUI. Built purely from the read-client, so a server, a CLI export, or a test can
 * produce the same structure the PixiJS GUI consumes. Heavy payloads are never included —
 * only references and the light, navigable index.
 */

import type { Brain0Store } from "./store.js";
import { riskColor } from "./risk.js";
import type { EdgeKind, Level, RiskState, TaskNode } from "./types.js";

export interface GraphNode {
  id: string;
  kind: "task" | "artifact";
  level?: Level;
  label: string;
  risk?: RiskState;
  colorHex?: string;
  timestamp?: string;
  author?: string;
  agent?: string;
  /** Task (commit) only: the source ref — the git commit SHA — for a compact header. */
  ref?: string;
}

export interface GraphEdge {
  kind: EdgeKind;
  src: string;
  dst: string;
}

export interface GraphSnapshot {
  repo: string;
  nodes: GraphNode[];
  edges: GraphEdge[];
}

const LEVELS: Level[] = ["repo", "module", "file", "symbol"];

/** Build a render-ready graph snapshot for a repo from the index. */
export function buildGraphSnapshot(store: Brain0Store, repo: string): GraphSnapshot {
  const nodes: GraphNode[] = [];
  const edges: GraphEdge[] = [];
  const edgeKeys = new Set<string>();
  const commitTaskIds = new Set<string>();

  // Task lookups are repeated across many artifacts; cache them.
  const taskCache = new Map<string, TaskNode | undefined>();
  const getTask = (id: string): TaskNode | undefined => {
    if (!taskCache.has(id)) taskCache.set(id, store.getTask(id));
    return taskCache.get(id);
  };
  // Commits are the intent layer drawn in the graph. An observer/commit task has
  // no source adapter; an agent-prompt task does. Agent prompts stay in the index (reachable from
  // a commit's detail as the summaries behind it) but are no longer standalone graph nodes — two
  // indistinguishable lavender dots on one plane read as noise.
  const isCommitTask = (task: TaskNode): boolean => task.sourceAdapter === undefined;

  const pushEdge = (edge: GraphEdge): void => {
    const key = `${edge.kind}|${edge.src}|${edge.dst}`;
    if (!edgeKeys.has(key)) {
      edgeKeys.add(key);
      edges.push(edge);
    }
  };

  for (const level of LEVELS) {
    for (const artifact of store.listArtifacts(repo, level)) {
      nodes.push({
        id: artifact.id,
        kind: "artifact",
        level: artifact.level,
        label: artifact.qualifiedPath,
        risk: artifact.risk,
        colorHex: riskColor(artifact.risk).hex,
      });
      for (const edge of store.outEdges("artifact_contains", artifact.id)) {
        pushEdge({
          kind: "artifact_contains",
          src: String(edge.parent ?? ""),
          dst: String(edge.child ?? ""),
        });
      }
      for (const edge of store.inEdges("task_modifies_artifact", artifact.id)) {
        const taskId = String(edge.task ?? "");
        if (!taskId) continue;
        const task = getTask(taskId);
        if (!task || !isCommitTask(task)) continue; // only commits get a node + drawn edge
        commitTaskIds.add(taskId);
        pushEdge({ kind: "task_modifies_artifact", src: taskId, dst: artifact.id });
      }
    }
  }

  for (const taskId of commitTaskIds) {
    const task = getTask(taskId);
    if (task) {
      nodes.push({
        id: taskId,
        kind: "task",
        label: taskId,
        ref: task.sessionId, // observer/commit tasks carry the git commit SHA as their session id
        timestamp: task.createdAt,
        author: task.author.name,
        agent: task.agent.name,
      });
    }
  }

  return { repo, nodes, edges };
}
