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

/** One provider row in the provider registry (mirrors `@brain0/agent`). */
export interface ProviderEntry {
  id: string;
  name: string;
  endpoint: string;
  defaultModel: string;
  remote: boolean;
  catalog: boolean;
  description?: string;
  docsUrl?: string;
  modelsUrl?: string;
  keysUrl?: string;
  envKey?: string;
}

export interface CatalogModel {
  id: string;
  name: string;
  contextLength?: number;
  inputModalities: string[];
  supportedEndpointTypes: string[];
}

export interface ModelCatalogResponse {
  source: "live" | "fallback";
  degraded: boolean;
  error?: string;
  models: CatalogModel[];
}

export interface ProviderStatus {
  active: { provider: string; model: string };
  providers: ProviderEntry[];
  orcarouter: {
    id: string;
    displayName: string;
    endpoint: string;
    defaultModel: string;
    fallbackModel: string;
    recommendedModels: ReadonlyArray<{ id: string; name: string; summary: string }>;
    credential: { present: boolean; source?: string; masked?: string };
    authState: { generation: number; needsReauth: boolean; reason?: string };
    catalog: { source: "live" | "fallback"; degraded: boolean; count: number; error?: string };
    links: { home: string; cyber: string; cyberModels: string; keys: string };
    envKey: string;
  };
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
      // The custom header is what the server requires before it spends an LLM call.
      const res = await fetch(`${this.base}/api/debug?q=${encodeURIComponent(query)}${phaseArg}`, {
        headers: { "x-brain0-ask": "1" },
      });
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

  /** Provider registry + masked credential state. The key itself never leaves the server. */
  async providerStatus(): Promise<ProviderStatus> {
    const res = await fetch(`${this.base}/api/providers`);
    if (!res.ok) throw new Error(`provider status failed: ${res.status}`);
    return (await res.json()) as ProviderStatus;
  }

  /** Compatible chat models for OrcaRouter, filtered server-side by declared capability. */
  async orcaRouterModels(modality?: string): Promise<ModelCatalogResponse> {
    const q = modality ? `&modality=${encodeURIComponent(modality)}` : "";
    const res = await fetch(`${this.base}/api/providers/orcarouter/models?capability=chat${q}`);
    if (!res.ok) throw new Error(`model catalog failed: ${res.status}`);
    return (await res.json()) as ModelCatalogResponse;
  }

  /** Hand a key to the server for this session. The custom header is the CSRF guard. */
  async saveOrcaRouterKey(apiKey: string): Promise<{ masked: string; reachable: boolean }> {
    const res = await fetch(`${this.base}/api/providers/orcarouter/key`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-brain0-provider": "1" },
      body: JSON.stringify({ api_key: apiKey }),
    });
    const body = (await res.json().catch(() => ({}))) as { error?: string; masked?: string; reachable?: boolean };
    if (!res.ok) throw new Error(body.error ?? `could not save the key (${res.status})`);
    return { masked: body.masked ?? "", reachable: body.reachable === true };
  }

  async clearOrcaRouterKey(): Promise<void> {
    const res = await fetch(`${this.base}/api/providers/orcarouter/key`, {
      method: "DELETE",
      headers: { "x-brain0-provider": "1" },
    });
    if (!res.ok) throw new Error(`could not clear the key (${res.status})`);
  }

  async selectModel(model: string): Promise<void> {
    const res = await fetch(`${this.base}/api/providers/model`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-brain0-provider": "1" },
      body: JSON.stringify({ model }),
    });
    if (!res.ok) {
      const body = (await res.json().catch(() => ({}))) as { error?: string };
      throw new Error(body.error ?? `could not select the model (${res.status})`);
    }
  }
}
