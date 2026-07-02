/**
 * Data source for the GUI. The browser cannot read SQLite directly, so the GUI consumes a
 * pre-built {@link GraphSnapshot} (produced server-side by `buildGraphSnapshot`) and calls
 * a small `/api/debug` endpoint that runs the internal agent. Both shapes are defined here
 * so the GUI is decoupled from how they are served.
 */

import type { GraphSnapshot, NodeDetail } from "@brain0/shared";

export type Severity = "info" | "warn" | "critical" | "gold";

export interface DebugFinding {
  id: string;
  kind: "task" | "artifact" | "version";
  reason: string;
  severity: Severity;
  verdict?: string;
  path?: string;
  external?: boolean;
}

export interface ProviderInfo {
  name: string;
  remote: boolean;
  ok?: boolean;
  embedder?: { name: string; remote: boolean };
  redacted: boolean;
  zeroEgress: boolean;
}

export interface DebugResponse {
  tasks: string[];
  artifacts: string[];
  explanation: string;
  intent?: "debug" | "audit";
  confidence?: number;
  findings?: DebugFinding[];
  distribution?: { green: number; yellow: number; red: number };
  goldSignals?: string[];
  topRisky?: Array<{ id: string; path: string; fused: number }>;
  provider?: ProviderInfo;
  error?: "no-llm" | "llm-unreachable";
}

export interface DiffResponse {
  diff?: string;
  encrypted?: boolean;
}

export interface RefreshStatus {
  state: "idle" | "running" | "done" | "error";
  phase: string;
  lines: string[];
  error?: string;
}

export interface GraphDataSource {
  load(): Promise<GraphSnapshot>;
  debug(query: string): Promise<DebugResponse>;
  detail(id: string): Promise<NodeDetail | undefined>;
  diff(ref: string): Promise<DiffResponse>;
  refresh(): Promise<{ jobId?: string; state?: string; error?: string }>;
  refreshStatus(): Promise<RefreshStatus>;
}

/** Reads `graph.json` and proxies debug queries to a backend that hosts the agent. */
export class HttpDataSource implements GraphDataSource {
  constructor(private readonly base = "") {}

  async load(): Promise<GraphSnapshot> {
    const res = await fetch(`${this.base}/graph.json`);
    if (!res.ok) throw new Error(`failed to load graph: ${res.status}`);
    return (await res.json()) as GraphSnapshot;
  }

  async debug(query: string, phase?: "retrieve"): Promise<DebugResponse> {
    try {
      const phaseArg = phase ? `&phase=${phase}` : "";
      const res = await fetch(`${this.base}/api/debug?q=${encodeURIComponent(query)}${phaseArg}`);
      if (!res.ok) return { tasks: [], artifacts: [], explanation: "", error: "llm-unreachable" };
      return (await res.json()) as DebugResponse;
    } catch {
      // Transport failure (server down) → degrade like an unreachable backend, never throw.
      return { tasks: [], artifacts: [], explanation: "", error: "llm-unreachable" };
    }
  }

  async detail(id: string): Promise<NodeDetail | undefined> {
    const res = await fetch(`${this.base}/api/node?id=${encodeURIComponent(id)}`);
    if (!res.ok) return undefined;
    return (await res.json()) as NodeDetail;
  }

  async diff(ref: string): Promise<DiffResponse> {
    const res = await fetch(`${this.base}/api/diff?ref=${encodeURIComponent(ref)}`);
    if (!res.ok) return {};
    return (await res.json()) as DiffResponse;
  }

  // Trigger a live re-observe (ingest + observe + embeddings). The custom header satisfies the
  // server's same-origin/CSRF guard. A 409 (already running) is not treated as an error.
  async refresh(): Promise<{ jobId?: string; state?: string; error?: string }> {
    const res = await fetch(`${this.base}/api/refresh`, {
      method: "POST",
      headers: { "x-brain0-refresh": "1" },
    });
    if (!res.ok && res.status !== 409) {
      const body = (await res.json().catch(() => ({}))) as { error?: string };
      return { state: "error", error: body.error ?? `http ${res.status}` };
    }
    return (await res.json()) as { jobId?: string; state?: string; error?: string };
  }

  async refreshStatus(): Promise<RefreshStatus> {
    const res = await fetch(`${this.base}/api/refresh/status`);
    if (!res.ok) return { state: "idle", phase: "", lines: [] };
    return (await res.json()) as RefreshStatus;
  }
}
