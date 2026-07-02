/**
 * The internal RAG-on-graph agent.
 *
 * It never reads the whole dataset: it queries the **index** (vector search + structured
 * traversal), ranks recency-aware, hydrates only the few selected nodes' payloads
 * (`maxHydrate`, typically 5–10), and asks the configured LLM to explain. Output is a set
 * of node **references** to highlight plus a textual explanation — the highlighting works
 * by reference, never by dumping content.
 */

import { Brain0Store, fusedScore, isGoldSignal, riskColor } from "@brain0/shared";
import type { Level } from "@brain0/shared";
import type { EmbeddingProvider } from "./embeddings.js";
import type { GetText } from "./indexer.js";
import { taskEmbeddingText } from "./indexer.js";
import { NoLlmError, type LLMProvider, type ProviderDescriptor } from "./llm.js";
import { classifyIntent } from "./intent.js";
import { isExternal, redactFreeText, redactStructural } from "./redact.js";
import { parseStructured, STRUCTURED_SCHEMA, type Intent, type Severity } from "./structured.js";
import { recencyAwareRank, type RankCandidate, type Ranked, type RankWeights } from "./ranking.js";

export interface AgentOptions {
  store: Brain0Store;
  embedder: EmbeddingProvider;
  llm: LLMProvider;
  /** Hydrates payload refs to text; defaults to a no-op (index-only). */
  getText?: GetText;
  /** Max nodes to hydrate (the 5–10 selected). */
  maxHydrate?: number;
  weights?: Partial<RankWeights>;
  /** Repo + level for the audit branch's repo-wide risk scan (server passes BRAIN0_REPO). */
  repo?: string;
  level?: Level;
}

export interface NodeHighlights {
  tasks: string[];
  artifacts: string[];
  versions: string[];
}

export interface DebugResult {
  query: string;
  highlights: NodeHighlights;
  ranked: Ranked[];
  explanation: string;
}

export interface AuditResult {
  repo: string;
  level: Level;
  distribution: { green: number; yellow: number; red: number };
  goldSignals: string[];
  topRisky: Array<{ id: string; path: string; fused: number }>;
  explanation: string;
}

/** One node the LLM (or retrieval floor) surfaced, enriched for the GUI cards. */
export interface AskFinding {
  id: string;
  kind: "task" | "artifact" | "version";
  reason: string;
  severity: Severity;
  verdict?: string;
  /** Path for artifacts; `external` flags an out-of-repo read (the audit red flag). */
  path?: string;
  external?: boolean;
}

/** Egress truth surfaced to the GUI: zero-egress requires BOTH channels local. */
export interface EgressInfo {
  llm: ProviderDescriptor;
  embedder: { name: string; remote: boolean };
  redacted: boolean;
  zeroEgress: boolean;
}

/** The unified smart-chat result: auto-detected intent + structured findings + prose. */
export interface AskResult {
  query: string;
  intent: Intent;
  confidence: number;
  highlights: NodeHighlights; // retrieval floor ∪ LLM-chosen
  findings: AskFinding[];
  ranked: Ranked[];
  distribution?: { green: number; yellow: number; red: number };
  goldSignals?: string[];
  topRisky?: Array<{ id: string; path: string; fused: number }>;
  explanation: string;
  egress: EgressInfo;
  error?: "no-llm" | "llm-unreachable";
}

const DEBUG_SYSTEM =
  "You are brain0's root-cause debugging assistant. Given an issue and the candidate " +
  "intents (prompts/decisions) plus the code they changed — ordered most-relevant and " +
  "most-recent first — identify the change most likely to have introduced the issue. " +
  "Cite task and artifact ids. Be concise.";

const AUDIT_SYSTEM =
  "You are brain0's audit assistant. Summarize what changed, where the risk concentrates, " +
  "and call out any 'looked safe but proved dangerous' (gold-signal) artifacts. Be concise.";

const ASK_SYSTEM =
  "You are brain0's debug+audit assistant. From the user's question, decide the INTENT: " +
  "'debug' (find the change/root cause of an issue) or 'audit' (what reached the model — files " +
  "read, secrets, risk concentration). Use ONLY the evidence provided. Reply with a single JSON " +
  "object: {\"intent\":\"debug\"|\"audit\", \"highlights\":[{\"id\":\"<copied from evidence>\"," +
  "\"kind\":\"task\"|\"artifact\"|\"version\",\"reason\":\"why it matters\"," +
  "\"severity\":\"info\"|\"warn\"|\"critical\"|\"gold\",\"verdict\":\"short label or null\"}]," +
  "\"explanation\":\"concise prose\"}. Every id MUST be copied verbatim from the evidence. " +
  "Highlight only the few nodes that matter (the root-cause chain for debug; the risky / " +
  "secret-touching nodes for audit), MOST IMPORTANT FIRST: AT MOST 8 highlights, each `reason` " +
  "<= 12 words and `verdict` a few words. Keep `explanation` to 1–2 sentences. Keep the whole " +
  "answer compact so the JSON stays complete and valid. Output JSON only, no prose outside it.";

export class Brain0Agent {
  private readonly store: Brain0Store;
  private readonly embedder: EmbeddingProvider;
  private readonly llm: LLMProvider;
  private readonly getText: GetText;
  private readonly maxHydrate: number;
  private readonly weights?: Partial<RankWeights>;
  private readonly repo?: string;
  private readonly level?: Level;

  constructor(opts: AgentOptions) {
    this.store = opts.store;
    this.embedder = opts.embedder;
    this.llm = opts.llm;
    this.getText = opts.getText ?? (() => Promise.resolve(undefined));
    this.maxHydrate = opts.maxHydrate ?? 8;
    this.weights = opts.weights;
    this.repo = opts.repo;
    this.level = opts.level;
  }

  /** Max fused risk across the artifacts a task modified (for ranking). */
  private taskRisk(taskId: string): number {
    let max = 0;
    for (const edge of this.store.outEdges("task_modifies_artifact", taskId)) {
      const artId = String(edge.artifact ?? "");
      const art = this.store.getArtifact(artId);
      if (art) max = Math.max(max, fusedScore(art.risk));
    }
    return max;
  }

  /**
   * Root-cause debug: from the issue text, find the relevant intents, rank recency-aware,
   * traverse to the code + its version chains, and explain — operating by reference.
   */
  async debug(query: string): Promise<DebugResult> {
    const qvec = await this.embedder.embed(query);
    const pool = this.store.searchTasksByVector(qvec, Math.max(this.maxHydrate * 3, this.maxHydrate));
    const candidates: RankCandidate[] = pool.map((h) => ({
      taskId: h.taskId,
      cosine: h.cosine,
      createdAt: h.createdAt,
      risk: this.taskRisk(h.taskId),
    }));
    const ranked = recencyAwareRank(candidates, { weights: this.weights });
    const top = ranked.slice(0, this.maxHydrate);

    const artifacts = new Set<string>();
    const versions = new Set<string>();
    const evidence: string[] = [];

    for (const cand of top) {
      const text = await taskEmbeddingText(this.store, cand.taskId, this.getText);
      const artifactLines: string[] = [];
      for (const edge of this.store.outEdges("task_modifies_artifact", cand.taskId)) {
        const artId = String(edge.artifact ?? "");
        const art = this.store.getArtifact(artId);
        if (!art) continue;
        artifacts.add(artId);
        const chain = this.store.artifactVersions(artId);
        for (const v of chain) versions.add(v.id);
        const last = chain[chain.length - 1];
        const color = riskColor(art.risk);
        artifactLines.push(
          `  - ${artId} ${art.qualifiedPath} risk=${color.fused.toFixed(2)} (${color.transition})` +
            ` lastChange=${last?.changeKind ?? "?"} by ${last?.author.name ?? "?"} @ ${last?.timestamp ?? "?"}`,
        );
      }
      evidence.push(
        `Task ${cand.taskId} [relevance=${cand.cosine.toFixed(2)} recency=${cand.recency.toFixed(2)} risk=${(cand.risk ?? 0).toFixed(2)}]\n` +
          `${text || "(no payload)"}\nChanged:\n${artifactLines.join("\n") || "  (none)"}`,
      );
    }

    const explanation = await this.llm.complete([
      { role: "system", content: DEBUG_SYSTEM },
      {
        role: "user",
        content: `Issue: ${query}\n\nCandidate intents (most relevant first):\n\n${evidence.join("\n\n")}`,
      },
    ]);

    return {
      query,
      highlights: {
        tasks: top.map((t) => t.taskId),
        artifacts: [...artifacts],
        versions: [...versions],
      },
      ranked: top,
      explanation,
    };
  }

  /**
   * Big-picture audit over a repo: risk distribution, gold-signal artifacts, and the
   * riskiest nodes — by reference (no payload dump).
   */
  async audit(opts: { repo: string; level?: Level }): Promise<AuditResult> {
    const level = opts.level ?? "symbol";
    const artifacts = this.store.listArtifacts(opts.repo, level);
    const scored = artifacts.map((a) => ({ a, fused: fusedScore(a.risk) }));

    let green = 0;
    let yellow = 0;
    let red = 0;
    const goldSignals: string[] = [];
    for (const { a, fused } of scored) {
      if (fused < 0.34) green += 1;
      else if (fused < 0.66) yellow += 1;
      else red += 1;
      if (isGoldSignal(a.risk)) goldSignals.push(a.id);
    }
    const topRisky = [...scored]
      .sort((x, y) => y.fused - x.fused)
      .slice(0, this.maxHydrate)
      .map(({ a, fused }) => ({ id: a.id, path: a.qualifiedPath, fused }));

    const summary =
      `Repo ${opts.repo}: ${artifacts.length} ${level}(s). ` +
      `Risk distribution green=${green} yellow=${yellow} red=${red}. ` +
      `Gold-signal (looked safe → proved dangerous): ${goldSignals.length}.`;

    const explanation = await this.llm.complete([
      { role: "system", content: AUDIT_SYSTEM },
      {
        role: "user",
        content:
          `${summary}\nTop risky:\n` +
          topRisky.map((t) => `- ${t.id} ${t.path} ${t.fused.toFixed(2)}`).join("\n"),
      },
    ]);

    return { repo: opts.repo, level, distribution: { green, yellow, red }, goldSignals, topRisky, explanation };
  }

  /**
   * Assemble the enriched, reference-preserving evidence for the top tasks: hydrated summary,
   * files read (with out-of-repo flag), declared↔done drift, and each changed artifact's risk +
   * coupling (centrality/blast via the co-change `artifact_depends_on` edges, omitted when absent).
   * Returns the prompt block, the set of valid ids (to reject hallucinated ones), the retrieval
   * highlight floor, and which tasks read an out-of-repo file (for the audit card flag).
   */
  private async buildBundle(top: Ranked[]): Promise<{
    text: string;
    ids: Set<string>;
    retrieval: NodeHighlights;
    taskExternalRead: Map<string, boolean>;
  }> {
    const ids = new Set<string>();
    const tasks: string[] = [];
    const artifacts = new Set<string>();
    const versions = new Set<string>();
    const taskExternalRead = new Map<string, boolean>();
    const blocks: string[] = [];

    for (const cand of top) {
      tasks.push(cand.taskId);
      ids.add(cand.taskId);

      const reads = new Set<string>();
      let undeclared = 0;
      let phantom = 0;
      for (const v of this.store.taskVersions(cand.taskId)) {
        for (const r of v.reads ?? []) reads.add(r);
        undeclared += v.drift?.undeclared?.length ?? 0;
        phantom += v.drift?.phantom?.length ?? 0;
      }
      const readList = [...reads];
      taskExternalRead.set(cand.taskId, readList.some(isExternal));

      const artLines: string[] = [];
      for (const edge of this.store.outEdges("task_modifies_artifact", cand.taskId)) {
        const artId = String(edge.artifact ?? "");
        const art = this.store.getArtifact(artId);
        if (!art) continue;
        artifacts.add(artId);
        ids.add(artId);
        for (const v of this.store.artifactVersions(artId)) {
          versions.add(v.id);
          ids.add(v.id);
        }
        const color = riskColor(art.risk);
        const coupled = this.store.inEdges("artifact_depends_on", artId).length;
        const gold = isGoldSignal(art.risk) ? " GOLD(safe→dangerous)" : "";
        artLines.push(
          `    - ${artId} ${art.qualifiedPath} risk=${color.fused.toFixed(2)} (${color.transition})${gold}` +
            (coupled ? ` coupledTo=${coupled}` : ""),
        );
      }

      // Evidence budget: long payloads inflate tokens (and local-inference latency) without
      // adding signal — cap the summary and the per-task artifact list.
      const fullSummary = await taskEmbeddingText(this.store, cand.taskId, this.getText);
      const summary =
        fullSummary.length > 480 ? `${fullSummary.slice(0, 480)}… (truncated)` : fullSummary;
      const readsLine = readList.length
        ? readList.slice(0, 8).map((r) => (isExternal(r) ? `${r} (EXTERNAL)` : r)).join(", ")
        : "(none)";
      blocks.push(
        `Task ${cand.taskId} [rel=${cand.cosine.toFixed(2)} rec=${cand.recency.toFixed(2)} risk=${(cand.risk ?? 0).toFixed(2)}]\n` +
          `${summary || "(no payload)"}\n` +
          `  reads: ${readsLine}\n` +
          `  drift: undeclared=${undeclared} phantom=${phantom}\n` +
          `  changed:\n${capped(artLines, 6).join("\n") || "    (none)"}`,
      );
    }

    return {
      text: blocks.join("\n\n"),
      ids,
      retrieval: { tasks, artifacts: [...artifacts], versions: [...versions] },
      taskExternalRead,
    };
  }

  /** Repo-wide audit numbers (distribution / gold signals / top risky) — the misclassification-safe
   * floor, computed from the full level scan, not the ~8 retrieved tasks. */
  private auditNumbers(): {
    distribution: { green: number; yellow: number; red: number };
    goldSignals: string[];
    topRisky: Array<{ id: string; path: string; fused: number }>;
  } | undefined {
    if (!this.repo) return undefined;
    const level = this.level ?? "symbol";
    const scored = this.store
      .listArtifacts(this.repo, level)
      .map((a) => ({ a, fused: fusedScore(a.risk) }));
    let green = 0;
    let yellow = 0;
    let red = 0;
    const goldSignals: string[] = [];
    for (const { a, fused } of scored) {
      if (fused < 0.34) green += 1;
      else if (fused < 0.66) yellow += 1;
      else red += 1;
      if (isGoldSignal(a.risk)) goldSignals.push(a.id);
    }
    const topRisky = [...scored]
      .sort((x, y) => y.fused - x.fused)
      .slice(0, this.maxHydrate)
      .map(({ a, fused }) => ({ id: a.id, path: a.qualifiedPath, fused }));
    return { distribution: { green, yellow, red }, goldSignals, topRisky };
  }

  /** Artifacts (and their linked tasks) named by the query — the lexical channel. */
  private resolveEntityArtifacts(query: string): { artifacts: string[]; tasks: string[] } {
    const artifacts = new Set<string>();
    const tasks = new Set<string>();
    if (!this.repo) return { artifacts: [], tasks: [] };
    const tokens = extractEntityTokens(query).map((t) => t.toLowerCase());
    if (tokens.length === 0) return { artifacts: [], tasks: [] };
    for (const level of ["file", "symbol"] as const) {
      for (const art of this.store.listArtifacts(this.repo, level)) {
        const path = art.qualifiedPath.toLowerCase();
        if (tokens.some((t) => path.includes(t))) {
          artifacts.add(art.id);
          for (const edge of this.store.inEdges("task_modifies_artifact", art.id)) {
            const task = String(edge.task ?? "");
            if (task) tasks.add(task);
          }
        }
      }
    }
    return { artifacts: [...artifacts], tasks: [...tasks] };
  }

  /** Commit task ids referenced by a git SHA in the query (hex tokens of 7–40 chars matched as a
   *  commit's session_id prefix). Empty when the query names no known commit. */
  private resolveCommitTasks(query: string): string[] {
    const out = new Set<string>();
    const tokens = query.toLowerCase().match(/\b[0-9a-f]{7,40}\b/g) ?? [];
    for (const token of tokens) {
      for (const id of this.store.commitTaskIdsByShaPrefix(token)) out.add(id);
    }
    return [...out];
  }

  /**
   * Single auto-intent entry point for the smart chat. Retrieval is ALWAYS intent-agnostic; the
   * LLM self-classifies debug vs audit and returns structured findings (validated against the
   * bundle ids) plus prose. Egress is redacted per-channel when the provider is remote; the result
   * carries the truthful egress state. The LLM call failing never blanks the graph — the retrieval
   * floor still highlights — and never throws: it returns `error` instead.
   */
  /** The shared retrieval stage (no LLM): classify, resolve entities, rank, build the bundle. */
  private async gather(query: string): Promise<{
    cls: ReturnType<typeof classifyIntent>;
    llmDesc: ProviderDescriptor;
    embDesc: NonNullable<EmbeddingProvider["descriptor"]>;
    top: Ranked[];
    bundle: Awaited<ReturnType<Brain0Agent["buildBundle"]>>;
  }> {
    const cls = classifyIntent(query);
    const llmDesc: ProviderDescriptor = this.llm.descriptor ?? {
      name: "echo",
      model: "",
      endpoint: "",
      remote: false,
    };
    const embDesc = this.embedder.descriptor ?? { name: "local", remote: false };

    // Explicit entity reference wins over fuzzy search: if the query names a commit by SHA, scope
    // the bundle to THAT commit (and the files it changed) — so the answer is about it, not the
    // top-N semantically-similar tasks (which would pull in unrelated commits). No embedding call
    // is made in this path (nothing leaves the machine for an explicit-commit query).
    const resolved = this.resolveCommitTasks(query);
    const entity = this.resolveEntityArtifacts(query);
    let top: Ranked[];
    if (resolved.length > 0) {
      top = resolved.slice(0, this.maxHydrate).map((taskId) => ({
        taskId,
        cosine: 1,
        createdAt: this.store.getTask(taskId)?.createdAt ?? "",
        risk: this.taskRisk(taskId),
        recency: 1,
        score: 1,
      }));
    } else {
      // Embedding channel: redact the query before a REMOTE embedder (it changes the vector — intended).
      const embedInput = embDesc.remote ? redactFreeText(query) : query;
      const qvec = await this.embedder.embed(embedInput);
      const pool = this.store.searchTasksByVector(qvec, Math.max(this.maxHydrate * 3, this.maxHydrate));
      const candidates: RankCandidate[] = pool.map((h) => ({
        taskId: h.taskId,
        cosine: h.cosine,
        createdAt: h.createdAt,
        risk: this.taskRisk(h.taskId),
      }));
      const ranked = recencyAwareRank(candidates, { weights: this.weights });
      // Hybrid retrieval: artifacts NAMED in the query (paths/symbols) pull their linked tasks
      // in ahead of fuzzy matches — embeddings alone miss exact-entity questions. Lexical tasks
      // take at most half the budget; semantic ranking fills the rest.
      const lex = entity.tasks
        .map((taskId) => ({
          taskId,
          cosine: 0.95,
          createdAt: this.store.getTask(taskId)?.createdAt ?? "",
          risk: this.taskRisk(taskId),
          recency: 1,
          score: 0.95,
        }))
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt))
        .slice(0, Math.ceil(this.maxHydrate / 2));
      const lexIds = new Set(lex.map((t) => t.taskId));
      top = [...lex, ...ranked.filter((t) => !lexIds.has(t.taskId))].slice(0, this.maxHydrate);
    }
    const bundle = await this.buildBundle(top);
    // Entity-named artifacts always reach the floor and are citable, even task-less ones.
    for (const id of entity.artifacts) {
      bundle.ids.add(id);
      if (!bundle.retrieval.artifacts.includes(id)) bundle.retrieval.artifacts.push(id);
    }
    return { cls, llmDesc, embDesc, top, bundle };
  }

  /**
   * Phase 1 of the two-phase smart chat: retrieval only — highlights, ranked candidates, and
   * (for audit-shaped queries) the deterministic numbers. Milliseconds, zero LLM tokens; the
   * GUI paints this immediately while `ask()` produces the explanation.
   */
  async retrieve(query: string): Promise<AskResult> {
    const { cls, llmDesc, embDesc, top, bundle } = await this.gather(query);
    const audit = cls.intent === "audit" ? this.auditNumbers() : undefined;
    return {
      query,
      intent: cls.intent,
      confidence: cls.confidence,
      highlights: bundle.retrieval,
      findings: [],
      ranked: top,
      distribution: audit?.distribution,
      goldSignals: audit?.goldSignals,
      topRisky: audit?.topRisky,
      explanation: "",
      egress: {
        llm: llmDesc,
        embedder: embDesc,
        redacted: llmDesc.remote,
        zeroEgress: !llmDesc.remote && !embDesc.remote,
      },
      error: undefined,
    };
  }

  async ask(query: string): Promise<AskResult> {
    const { cls, llmDesc, embDesc, top, bundle } = await this.gather(query);

    // LLM channel: redact the assembled message before a REMOTE provider (free-text query gets the
    // entropy detector; the id-heavy structural evidence does not).
    const queryOut = llmDesc.remote ? redactFreeText(query) : query;
    const structuralOut = llmDesc.remote ? redactStructural(bundle.text) : bundle.text;
    const userMsg = `Question: ${queryOut}\n\nEvidence (choose ids ONLY from here):\n\n${structuralOut}`;
    const messages = [
      { role: "system" as const, content: ASK_SYSTEM },
      { role: "user" as const, content: userMsg },
    ];

    let raw = "";
    let error: AskResult["error"];
    try {
      if (this.llm.completeStructured) {
        try {
          raw = await this.llm.completeStructured(messages, STRUCTURED_SCHEMA);
        } catch (err) {
          if (err instanceof NoLlmError) throw err; // null provider → no fallback
          raw = await this.llm.complete(messages); // provider lacks native JSON → universal floor
        }
      } else {
        raw = await this.llm.complete(messages);
      }
    } catch (err) {
      error = err instanceof NoLlmError ? "no-llm" : "llm-unreachable";
    }

    const structured = error
      ? { intent: cls.intent, highlights: [] as ReturnType<typeof parseStructured>["highlights"], explanation: "" }
      : parseStructured(raw, bundle.ids, cls.intent);

    // Highlights = retrieval floor ∪ LLM-chosen (the LLM may only ADD).
    const union = (floor: string[], add: string[]): string[] => [...new Set([...floor, ...add])];
    const llmByKind = (kind: "task" | "artifact" | "version"): string[] =>
      structured.highlights.filter((h) => h.kind === kind).map((h) => h.id);
    const highlights: NodeHighlights = {
      tasks: union(bundle.retrieval.tasks, llmByKind("task")),
      artifacts: union(bundle.retrieval.artifacts, llmByKind("artifact")),
      versions: union(bundle.retrieval.versions, llmByKind("version")),
    };

    // Enrich findings with path + the out-of-repo read flag (surfaced on the card).
    const findings: AskFinding[] = structured.highlights.map((h) => {
      const art = h.kind === "artifact" ? this.store.getArtifact(h.id) : undefined;
      const external =
        h.kind === "artifact" ? (art ? isExternal(art.qualifiedPath) : false) : bundle.taskExternalRead.get(h.id) ?? false;
      return { ...h, path: art?.qualifiedPath, external };
    });

    const audit = structured.intent === "audit" ? this.auditNumbers() : undefined;

    return {
      query,
      intent: structured.intent,
      confidence: cls.confidence,
      highlights,
      findings,
      ranked: top,
      distribution: audit?.distribution,
      goldSignals: audit?.goldSignals,
      topRisky: audit?.topRisky,
      explanation: structured.explanation,
      egress: { llm: llmDesc, embedder: embDesc, redacted: llmDesc.remote, zeroEgress: !llmDesc.remote && !embDesc.remote },
      error,
    };
  }
}

/**
 * Tokens in a query that look like code entities — the lexical retrieval channel. Conservative
 * on purpose: backtick-quoted spans, path-like words (`/` or `::`), and extension-bearing names.
 * Plain prose words never match, so the semantic channel keeps owning fuzzy questions.
 */
export function extractEntityTokens(query: string): string[] {
  const out = new Set<string>();
  for (const m of query.matchAll(/`([^`]+)`/g)) {
    const t = m[1]?.trim();
    if (t && t.length >= 3) out.add(t);
  }
  for (const raw of query.split(/\s+/)) {
    const w = raw.replace(/^[('"«]+|[)'"».,;:!?]+$/g, "");
    if (w.length < 3) continue;
    if (w.includes("/") || w.includes("::") || /\.[a-z]{1,4}$/i.test(w)) out.add(w);
  }
  return [...out];
}

/** First `keep` lines plus a "+N more" marker — evidence lists must not balloon the prompt. */
function capped(lines: string[], keep: number): string[] {
  if (lines.length <= keep) return lines;
  return [...lines.slice(0, keep), `    … +${lines.length - keep} more`];
}
