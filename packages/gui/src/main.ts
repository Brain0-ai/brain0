/**
 * GUI bootstrap: loads the graph snapshot, wires the PixiJS renderer, and connects the
 * search bar (natural-language debug via the agent), the timeline, and the attribution
 * panel.
 */

import type { DetailVersion, GraphNode, GraphSnapshot, Level, NodeDetail } from "@brain0/shared";
import { HttpDataSource, type DebugFinding, type DebugResponse } from "./datasource.js";
import { PixiGraph } from "./renderer.js";

function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node as T;
}

async function main(): Promise<void> {
  const stage = el<HTMLDivElement>("stage");
  const prompt = el<HTMLTextAreaElement>("prompt");
  const sendBtn = el<HTMLButtonElement>("send");
  const promptForm = el<HTMLFormElement>("promptbar");
  const timeline = el<HTMLInputElement>("timeline");
  const panel = el<HTMLElement>("panel");
  const refreshBtn = el<HTMLButtonElement>("refresh");
  const refreshStatus = el<HTMLElement>("refresh-status");

  const dataSource = new HttpDataSource();
  const graph = new PixiGraph();
  await graph.init(stage);

  // The default empty-state markup authored in index.html, restored when the selection clears.
  const emptyState = panel.innerHTML;
  let selectedNodeId: string | null = null;
  // The commit currently in view, so a prompt opened from it can offer "← back to commit".
  let lastCommitId: string | null = null;
  // The current snapshot's node ids — to validate which highlight ids the graph can render.
  const nodeIds = new Set<string>();
  // The last smart-chat result, so a node opened from a finding can return to the results list,
  // and clicking the empty background restores the results rather than the empty state.
  let lastAsk: { result: DebugResponse; findings: DebugFinding[] } | null = null;

  // Fetch + render a single diff on demand (one request, only when needed). Shared by the manual
  // "See details" button and the auto-expand for leaf nodes.
  const revealDiff = async (button: HTMLButtonElement): Promise<void> => {
    const ref = button.dataset.ref;
    const slot = button.parentElement?.querySelector<HTMLElement>(".diff-slot");
    if (!ref || !slot || slot.dataset.loaded === "1") return;
    button.textContent = "loading…";
    const result = await dataSource.diff(ref);
    slot.dataset.loaded = "1";
    slot.style.display = "block";
    slot.innerHTML = result.encrypted
      ? '<p class="muted">payload encrypted — re-run ingest with --no-encrypt-payload to preview</p>'
      : result.diff
        ? renderDiffHtml(result.diff)
        : '<p class="muted">diff not available — the payload is encrypted (.enc) or none was stored. Re-run: <code>brain0 ingest … --no-encrypt-payload</code></p>';
    button.textContent = "Hide details";
  };

  // Leaf artifacts (a single file/symbol) auto-load their latest diff on selection — still on
  // demand (one fetch for the clicked node), just without the extra "See details" click.
  const autoExpandDiffs = (): void => {
    panel
      .querySelectorAll<HTMLButtonElement>("button.see-details[data-autoload]")
      .forEach((button) => void revealDiff(button));
  };

  // Open a node's detail by id — used for nodes that are NOT in the graph (a commit's prompts) and
  // for clicked smart-chat findings. `back` adds a "← back to commit" or "← back to results" link.
  const showDetailById = (id: string, back?: { commit?: string; results?: boolean }): void => {
    selectedNodeId = id;
    panel.scrollTop = 0;
    void dataSource
      .detail(id)
      .then((detail) => {
        if (selectedNodeId !== id) return;
        if (!detail) {
          panel.innerHTML = '<p class="muted">not found</p>';
          return;
        }
        const backHtml = back?.results
          ? `<button class="back-link" data-back="results">← back to results</button>`
          : back?.commit
            ? `<button class="back-link" data-commit="${escapeHtml(back.commit)}">← back to commit</button>`
            : "";
        const node: GraphNode = {
          id,
          kind: detail.kind === "artifact" ? "artifact" : "task",
          label: detail.label,
        };
        panel.innerHTML = backHtml + renderDetail(node, detail);
        autoExpandDiffs();
      })
      .catch(() => {
        if (selectedNodeId === id) panel.innerHTML = '<p class="muted">failed to load</p>';
      });
  };

  // Highlight the whole result set in the graph: retrieval floor ∪ LLM-chosen, validated against
  // renderable nodes (tasks + artifacts; version ids are not graph nodes), colored by severity.
  const applyAskHighlights = (result: DebugResponse, findings: DebugFinding[]): void => {
    const severity = new Map<string, string>();
    const ids = new Set<string>();
    const addId = (id: string, sev?: string): void => {
      if (!nodeIds.has(id)) return;
      ids.add(id);
      if (sev) severity.set(id, sev);
    };
    for (const id of result.tasks) addId(id);
    for (const id of result.artifacts) addId(id);
    for (const f of findings) if (f.kind !== "version") addId(f.id, f.severity);
    graph.setHighlights(ids, severity);
  };

  // Return to the smart-chat results list (re-render + re-highlight the whole set).
  const showAskResults = (): void => {
    if (!lastAsk) {
      panel.innerHTML = emptyState;
      return;
    }
    selectedNodeId = null;
    applyAskHighlights(lastAsk.result, lastAsk.findings);
    panel.scrollTop = 0;
    panel.innerHTML = renderAsk(lastAsk.result, lastAsk.findings);
  };

  graph.onSelect = (node: GraphNode) => {
    // The renderer already highlighted the node + its neighbors; here we fill the docked panel.
    selectedNodeId = node.id;
    if (node.kind === "task") lastCommitId = node.id; // graph intent nodes are commits
    // Show a lightweight header in the SAME contextual style immediately (no raw-data flash),
    // then hydrate the full detail by reference and replace in place.
    panel.scrollTop = 0;
    panel.innerHTML = `${renderNodeLite(node)}<p class="muted">loading…</p>`;
    void dataSource
      .detail(node.id)
      .then((detail) => {
        // Ignore a late response if the selection changed or was cleared meanwhile.
        if (selectedNodeId !== node.id) return;
        if (detail) {
          panel.innerHTML = renderDetail(node, detail);
          autoExpandDiffs();
        } else {
          panel.innerHTML = renderNodeLite(node);
        }
      })
      .catch(() => {
        if (selectedNodeId === node.id) panel.innerHTML = renderNodeLite(node);
      });
  };

  // Click the empty graph background to unfocus: the renderer has already CLEARED the graph
  // illumination (single selection or a whole search set). The sidebar then returns to the search
  // results list (kept until a new query) — but the graph stays un-lit; re-light it by clicking a
  // finding (narrow) or "← back to results" (whole set). With no active search, restore empty state.
  graph.onDeselect = () => {
    selectedNodeId = null;
    panel.scrollTop = 0;
    panel.innerHTML = lastAsk ? renderAsk(lastAsk.result, lastAsk.findings) : emptyState;
  };

  // View control (bottom-right): the user picks the graph level here (explicit, decoupled from
  // zoom), and drives free zoom in/out/fit. Selecting a level shows it; zooming never changes it.
  const lodEl = el<HTMLElement>("lod");
  const zoomPct = el<HTMLElement>("zoom-pct");
  const lodSteps = Array.from(lodEl.querySelectorAll<HTMLElement>(".lod-step"));
  graph.onViewChange = (zoom, level) => {
    zoomPct.textContent = `${Math.round(zoom * 100)}%`;
    for (const step of lodSteps) step.classList.toggle("active", step.dataset.level === level);
  };
  el<HTMLButtonElement>("zoom-in").addEventListener("click", () => graph.zoomBy(1.3));
  el<HTMLButtonElement>("zoom-out").addEventListener("click", () => graph.zoomBy(1 / 1.3));
  el<HTMLButtonElement>("zoom-fit").addEventListener("click", () => graph.resetView());
  for (const step of lodSteps) {
    step.addEventListener("click", () => {
      const level = step.dataset.level as Level | undefined;
      if (level) graph.setLevel(level);
    });
  }

  panel.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;

    // Open a prompt behind the commit (a node that is not in the graph).
    const promptItem = target.closest<HTMLElement>(".prompt-item");
    if (promptItem) {
      const taskId = promptItem.dataset.task;
      if (taskId) showDetailById(taskId, lastCommitId ? { commit: lastCommitId } : undefined);
      return;
    }
    // Back: to the smart-chat results, or to the commit a prompt belongs to.
    const back = target.closest<HTMLElement>(".back-link");
    if (back) {
      if (back.dataset.back === "results") {
        showAskResults();
        return;
      }
      const commitId = back.dataset.commit;
      if (commitId) showDetailById(commitId);
      return;
    }
    // Open a debug/audit finding's node: narrow the graph to the nodes IMPACTED by this result
    // (the node + its directly connected neighbors), and offer a way back to the results list.
    const finding = target.closest<HTMLElement>(".finding-card");
    if (finding) {
      const id = finding.dataset.id;
      if (id) {
        if (nodeIds.has(id)) graph.setSelection(id); // highlight only this result's impacted nodes
        showDetailById(id, { results: true });
      }
      return;
    }

    // "+N more" on a chip row expands/collapses the full file list in place.
    const moreChip = target.closest<HTMLButtonElement>(".chip-more");
    if (moreChip) {
      const wrap = moreChip.closest(".chips");
      if (wrap) {
        const expanded = wrap.classList.toggle("expanded");
        moreChip.textContent = expanded ? "show less" : `+${moreChip.dataset.count} more`;
      }
      return;
    }

    // "See details" toggles a diff: first click loads it on demand, later clicks show/hide it.
    const button = target.closest<HTMLButtonElement>(".see-details");
    if (!button) return;
    const slot = button.parentElement?.querySelector<HTMLElement>(".diff-slot");
    if (!slot) return;
    if (slot.dataset.loaded === "1") {
      const showing = slot.style.display !== "none";
      slot.style.display = showing ? "none" : "block";
      button.textContent = showing ? "See details" : "Hide details";
      return;
    }
    void revealDiff(button);
  });

  let snapshot;
  try {
    snapshot = await dataSource.load();
  } catch {
    panel.innerHTML =
      '<h3>No data</h3><p class="muted">Run the brain0 observer and serve <code>graph.json</code> + <code>/api/debug</code>.</p>';
    return;
  }
  // Timeline scrubber: map the slider over the observed time range and hide everything after the
  // cutoff. The dated edges, the live cutoff label, and the filled track make "how far back" legible.
  // Wiring (fmt, paintTrack, the input listener) is set up ONCE; the per-snapshot range lives in
  // `tl` so a refresh can re-apply a new snapshot without stacking listeners or capturing a stale
  // range (the listener closes over `tl`, not over per-snapshot consts).
  const tlStart = el<HTMLElement>("tl-start");
  const tlEnd = el<HTMLElement>("tl-end");
  const tlNow = el<HTMLElement>("tl-now");

  const fmt = (ms: number): string =>
    new Date(ms).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });

  const paintTrack = (frac: number): void => {
    const pct = Math.round(frac * 100);
    timeline.style.background = `linear-gradient(90deg, var(--accent) ${pct}%, var(--hairline-strong) ${pct}%)`;
  };

  const tl = { minTime: 0, maxTime: 0, hasTime: false };
  const updateTimeline = (): void => {
    const frac = Number(timeline.value) / 100;
    if (!tl.hasTime) {
      graph.setTimeCutoff(Number.POSITIVE_INFINITY);
      tlNow.textContent = "all";
      paintTrack(1);
      return;
    }
    const cutoff = tl.minTime + (tl.maxTime - tl.minTime) * frac;
    graph.setTimeCutoff(frac >= 1 ? Number.POSITIVE_INFINITY : cutoff);
    tlNow.textContent = frac >= 1 ? "all" : `≤ ${fmt(cutoff)}`;
    paintTrack(frac);
  };
  timeline.addEventListener("input", updateTimeline); // registered ONCE

  // Apply a (possibly refreshed) snapshot: re-render the graph and recompute the timeline range.
  const applySnapshot = (snap: GraphSnapshot): void => {
    graph.setSnapshot(snap);
    nodeIds.clear();
    for (const n of snap.nodes) nodeIds.add(n.id);
    const times = snap.nodes
      .map((n) => (n.timestamp ? Date.parse(n.timestamp) : NaN))
      .filter((t) => !Number.isNaN(t));
    tl.minTime = times.length ? Math.min(...times) : 0;
    tl.maxTime = times.length ? Math.max(...times) : 0;
    tl.hasTime = times.length > 0 && tl.maxTime > tl.minTime;
    tlStart.textContent = tl.hasTime ? fmt(tl.minTime) : "—";
    tlEnd.textContent = tl.hasTime ? fmt(tl.maxTime) : "—";
    timeline.disabled = !tl.hasTime;
    updateTimeline(); // call once; no new listener
  };

  applySnapshot(snapshot);

  // Refresh button: re-run the passive observer server-side (ingest + observe + embeddings),
  // surface live status, then re-apply the new snapshot. Single-flight via the disabled button.
  let polling: number | null = null;
  refreshBtn.addEventListener("click", () => {
    if (refreshBtn.disabled) return;
    refreshBtn.disabled = true;
    refreshStatus.textContent = "refreshing…";
    void (async () => {
      const started = await dataSource.refresh();
      if (started.state === "error") {
        refreshStatus.textContent = started.error ?? "failed";
        refreshBtn.disabled = false;
        return;
      }
      if (polling !== null) clearInterval(polling);
      polling = setInterval(() => {
        void (async () => {
          const s = await dataSource.refreshStatus();
          if (s.state === "running") {
            refreshStatus.textContent = s.lines.at(-1) ?? s.phase ?? "working…";
          } else if (s.state === "done") {
            if (polling !== null) clearInterval(polling);
            const snap = await dataSource.load();
            applySnapshot(snap);
            // The selected node may no longer exist after a refresh — clear the panel if so.
            if (selectedNodeId && !snap.nodes.some((n) => n.id === selectedNodeId)) {
              selectedNodeId = null;
              panel.innerHTML = emptyState;
            }
            refreshStatus.textContent = "updated";
            refreshBtn.disabled = false;
          } else if (s.state === "error") {
            if (polling !== null) clearInterval(polling);
            refreshStatus.textContent = s.error ?? "failed";
            refreshBtn.disabled = false;
          }
        })();
      }, 1000);
    })();
  });

  // Prompt box (bottom-centered, text only — no speech-to-text): natural-language debug that
  // highlights the agent's chain. Enter asks; Shift+Enter inserts a newline.
  const autoresize = (): void => {
    prompt.style.height = "auto";
    prompt.style.height = `${Math.min(prompt.scrollHeight, 168)}px`;
  };
  const refreshSend = (): void => {
    const ready = prompt.value.trim().length > 0;
    sendBtn.disabled = !ready;
    sendBtn.classList.toggle("ready", ready);
  };
  const submitPrompt = (): void => {
    const query = prompt.value.trim();
    if (!query) return;
    void runDebug(query);
    prompt.value = "";
    autoresize();
    refreshSend();
  };
  prompt.addEventListener("input", () => {
    autoresize();
    refreshSend();
  });
  prompt.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      submitPrompt();
    }
  });
  promptForm.addEventListener("submit", (event) => {
    event.preventDefault();
    submitPrompt();
  });
  refreshSend();

  let debugRun = 0;
  async function runDebug(query: string): Promise<void> {
    panel.scrollTop = 0;
    panel.innerHTML = '<p class="muted">searching…</p>';
    const run = ++debugRun;
    // Phase 1 — retrieval only (milliseconds, zero LLM): the graph lights up immediately while
    // the model still writes the explanation. Superseded runs are ignored.
    void dataSource
      .debug(query, "retrieve")
      .then((fast) => {
        if (run !== debugRun || fast.error) return;
        applyAskHighlights(fast, fast.findings ?? []);
        const n = fast.tasks.length + fast.artifacts.length;
        panel.innerHTML = `<p class="muted">${n} related node(s) highlighted — thinking…</p>`;
      })
      .catch(() => {});
    let result: DebugResponse;
    try {
      result = await dataSource.debug(query);
      if (run !== debugRun) return; // a newer question superseded this one
    } catch {
      panel.innerHTML = '<p class="muted">request failed — is the server running?</p>';
      return;
    }
    const findings = result.findings ?? [];
    lastAsk = { result, findings }; // remembered so findings/background-click return to this list
    applyAskHighlights(result, findings); // highlight the whole result set
    selectedNodeId = null; // an ask run replaces the click-selection context
    panel.innerHTML = renderAsk(result, findings);
  }
}

/** Render the smart-chat result: intent badge, egress notice, audit numbers, finding cards, prose. */
function renderAsk(result: DebugResponse, findings: DebugFinding[]): string {
  const intent = result.intent ?? "debug";
  const title = intent === "audit" ? "Audit" : "Debug";
  const header =
    `<div class="hd"><div class="hd-top"><span class="badge badge-${intent}">${intent}</span>` +
    (result.confidence !== undefined && result.confidence < 0.34 && !result.error
      ? `<span class="muted" style="font-size:11px">low confidence — rephrase to steer debug/audit</span>`
      : "") +
    `</div><div class="hd-title">${title}</div></div>`;

  // Provider / egress notice (truthful, two-channel).
  let notice = "";
  const p = result.provider;
  if (p) {
    if (p.zeroEgress) notice = `<p class="ask-notice ok">model: ${escapeHtml(p.name)} · local, zero egress</p>`;
    else {
      const bits: string[] = [];
      bits.push(p.remote ? `LLM ${escapeHtml(p.name)} (remote, redacted)` : `LLM ${escapeHtml(p.name)} (local)`);
      if (p.embedder?.remote) bits.push(`embeddings ${escapeHtml(p.embedder.name)} (remote, redacted)`);
      notice = `<p class="ask-notice warn">${bits.join(" · ")} — not zero-egress</p>`;
    }
  }

  const errHtml = result.error
    ? `<p class="ask-notice err">⚠ No LLM reachable — run <code>ollama serve</code> or set an API key / <code>BRAIN0_LLM_*</code>. Showing retrieval highlights only.</p>`
    : "";

  let distHtml = "";
  if (intent === "audit" && result.distribution) {
    const d = result.distribution;
    const gold = result.goldSignals?.length ? ` · <b>${result.goldSignals.length}</b> gold (safe→dangerous)` : "";
    distHtml =
      `<h4>risk across the repo</h4><div class="dist">` +
      `<span class="dist-g">${d.green} safe</span> · <span class="dist-y">${d.yellow} watch</span> · <span class="dist-r">${d.red} risky</span>${gold}</div>`;
  }

  const explain = result.explanation
    ? `<p class="ask-explain">${escapeHtml(result.explanation)}</p>`
    : "";

  let cards = "";
  if (findings.length) {
    const items = findings
      .map((f) => {
        const name = f.path ? basename(f.path) : f.id;
        const ext = f.external ? `<span class="read-ext">external read</span>` : "";
        const verdict = f.verdict ? `<span class="f-verdict">${escapeHtml(f.verdict)}</span>` : "";
        return (
          `<button class="finding-card sev-${f.severity}" data-id="${escapeHtml(f.id)}">` +
          `<span class="f-top"><span class="sev-dot"></span>${escapeHtml(f.severity)}${verdict}${ext}</span>` +
          `<span class="f-name">${escapeHtml(name)}</span>` +
          `<span class="f-reason">${escapeHtml(f.reason)}</span>` +
          `</button>`
        );
      })
      .join("");
    cards = `<h4>findings (${findings.length})</h4><div class="finding-list">${items}</div>`;
  }

  return `${header}${notice}${errHtml}${explain}${distHtml}${cards}`;
}

/** The contextual header built from the light graph node alone — shown instantly on click, in
 * the same style as the full detail, so there is never a raw-data flash before hydration. */
function renderNodeLite(node: GraphNode): string {
  if (node.kind === "task") {
    // Every graph "intent" node is a commit now (agent prompts are revealed inside a commit).
    const pills: string[] = [];
    if (node.ref) pills.push(pill(`<span class="k">commit</span> ${escapeHtml(node.ref.slice(0, 10))}`));
    if (node.author) pills.push(pill(`<span class="k">by</span> ${escapeHtml(node.author)}`));
    if (node.timestamp) pills.push(pill(relTime(node.timestamp), node.timestamp));
    return (
      `<div class="hd">` +
      `<div class="hd-top"><span class="badge badge-commit">commit</span></div>` +
      `<div class="hd-title">Commit</div>` +
      `<div class="pills">${pills.join("")}</div>` +
      `<div class="hd-id">${escapeHtml(node.id)}</div>` +
      `</div>`
    );
  }
  const name = basename(node.label);
  const pills: string[] = [];
  if (node.risk) {
    const r = riskInfo(node.risk);
    pills.push(
      pill(
        `<span class="dot" style="background:${escapeHtml(node.colorHex ?? "#8a8f98")}"></span>risk ${r.label} · ${r.score.toFixed(2)}`,
      ),
    );
  }
  return (
    `<div class="hd">` +
    `<div class="hd-top"><span class="badge badge-artifact">${escapeHtml(node.level ?? "code")}</span>` +
    `<span class="hd-title hd-title-sm">${escapeHtml(name)}</span></div>` +
    `<div class="pills">${pills.join("")}</div>` +
    `</div>`
  );
}

/** A short relative time ("2d ago"), falling back to the raw string if unparseable. */
function relTime(iso: string): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return iso;
  const s = Math.max(0, Math.round((Date.now() - t) / 1000));
  if (s < 60) return "just now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.round(h / 24);
  if (d < 30) return `${d}d ago`;
  const mo = Math.round(d / 30);
  return mo < 12 ? `${mo}mo ago` : `${Math.round(mo / 12)}y ago`;
}

function basename(path: string): string {
  const parts = path.split("/");
  return parts[parts.length - 1] || path;
}

function pill(inner: string, title?: string): string {
  return `<span class="pill"${title ? ` title="${escapeHtml(title)}"` : ""}>${inner}</span>`;
}

/** Fused risk score (a-priori ⊕ a-posteriori) and a coarse label. */
function riskInfo(risk: { apriori: number; aposteriori: number }): { label: string; score: number } {
  const a = Math.max(0, Math.min(1, risk.apriori));
  const p = Math.max(0, Math.min(1, risk.aposteriori));
  const score = 1 - (1 - a) * (1 - p);
  return { label: score < 0.34 ? "low" : score < 0.67 ? "medium" : "high", score };
}

/** A contextual header: a meaningful title + badges/pills instead of raw key:value rows. */
function renderHeader(node: GraphNode, detail: NodeDetail): string {
  if (detail.kind === "task") {
    const latest = detail.versions[detail.versions.length - 1];
    const gist = latest?.summary?.trim();
    const title = gist ? escapeHtml(truncate(gist, 180)) : "Coding intent";
    const pills: string[] = [];
    if (detail.agent) pills.push(pill(`<span class="k">agent</span> ${escapeHtml(detail.agent)}`));
    if (detail.author) pills.push(pill(`<span class="k">by</span> ${escapeHtml(detail.author)}`));
    if (detail.createdAt) pills.push(pill(relTime(detail.createdAt), detail.createdAt));
    const n = detail.versions.length;
    pills.push(pill(`${n} turn${n === 1 ? "" : "s"}`));
    return (
      `<div class="hd">` +
      `<div class="hd-top"><span class="badge badge-task">intent</span></div>` +
      `<div class="hd-title">${title}</div>` +
      `<div class="pills">${pills.join("")}</div>` +
      `<div class="hd-id">${escapeHtml(detail.id)}</div>` +
      `</div>`
    );
  }

  // Artifact: lead with the file/symbol name; full path + badges/risk underneath.
  const path = detail.path ?? detail.label;
  const name = basename(path);
  const pills: string[] = [];
  if (node.risk) {
    const r = riskInfo(node.risk);
    pills.push(
      pill(
        `<span class="dot" style="background:${escapeHtml(node.colorHex ?? "#8a8f98")}"></span>risk ${r.label} · ${r.score.toFixed(2)}`,
        `a-priori ${node.risk.apriori.toFixed(2)} · a-posteriori ${node.risk.aposteriori.toFixed(2)}`,
      ),
    );
  }
  if (detail.childCount) pills.push(pill(`${detail.childCount} children`));
  const last = detail.versions[detail.versions.length - 1];
  if (last?.timestamp) pills.push(pill(`changed ${relTime(last.timestamp)}`, last.timestamp));
  return (
    `<div class="hd">` +
    `<div class="hd-top"><span class="badge badge-artifact">${escapeHtml(detail.level ?? "code")}</span>` +
    `<span class="hd-title hd-title-sm">${escapeHtml(name)}</span></div>` +
    (name !== path ? `<div class="hd-sub">${escapeHtml(path)}</div>` : "") +
    `<div class="pills">${pills.join("")}</div>` +
    `</div>`
  );
}

/** First line of a (possibly multi-line) commit message. */
function firstLine(text: string): string {
  const i = text.indexOf("\n");
  return (i === -1 ? text : text.slice(0, i)).trim();
}

/**
 * A commit "card": the commit message, the prompts behind it (agent summaries, click to open),
 * and the files it changed (git-style, with a lazy diff per file). This is the intent layer the
 * graph draws — clicking a commit node lands here.
 */
function renderCommit(detail: NodeDetail): string {
  const msg = detail.message?.trim();
  const title = msg ? escapeHtml(truncate(firstLine(msg), 200)) : "Commit";
  const pills: string[] = [];
  if (detail.author) pills.push(pill(`<span class="k">by</span> ${escapeHtml(detail.author)}`));
  if (detail.source) pills.push(pill(`<span class="k">commit</span> ${escapeHtml(detail.source)}`));
  if (detail.createdAt) pills.push(pill(relTime(detail.createdAt), detail.createdAt));
  const nf = detail.changedFiles?.length ?? 0;
  pills.push(pill(`${nf} file${nf === 1 ? "" : "s"}`));
  const header =
    `<div class="hd">` +
    `<div class="hd-top"><span class="badge badge-commit">commit</span></div>` +
    `<div class="hd-title">${title}</div>` +
    `<div class="pills">${pills.join("")}</div>` +
    `<div class="hd-id">${escapeHtml(detail.id)}</div>` +
    `</div>`;

  // Full message body only when it adds more than the one-line gist already shown as the title.
  const body =
    msg && msg.split("\n").length > 1 ? `<pre class="commit-msg">${escapeHtml(msg)}</pre>` : "";

  // Prompts behind this commit: the agent summaries that led to its changes. Click to open one.
  const prompts = detail.prompts ?? [];
  let promptsHtml: string;
  if (prompts.length) {
    const items = prompts
      .map((p) => {
        const text = p.summary?.trim() ? escapeHtml(truncate(p.summary.trim(), 240)) : "(no summary)";
        const meta = [p.agent, p.author]
          .filter(Boolean)
          .map((s) => escapeHtml(s))
          .join(" · ");
        const when = p.when ? ` · ${escapeHtml(relTime(p.when))}` : "";
        return (
          `<button class="prompt-item" data-task="${escapeHtml(p.taskId)}">` +
          `<span class="prompt-sum">${text}</span>` +
          `<span class="prompt-meta">${meta}${when}</span>` +
          `</button>`
        );
      })
      .join("");
    promptsHtml = `<h4>prompts behind this commit (${prompts.length})</h4><div class="prompt-list">${items}</div>`;
  } else {
    promptsHtml = `<h4>prompts behind this commit</h4><p class="muted">no agent prompt linked to these files</p>`;
  }

  // Files read for this commit (audit): what the sessions behind it loaded into the model's
  // context — grouped by what a reviewer scans for. Secret-bearing reads (DLP) first in red with
  // the detected kind inline, out-of-repo reads in accent ("what left the project"), then the
  // plain repo files collapsed behind "+N more". Chips: name visible, full path on hover.
  const reads = detail.reads ?? [];
  const readSecrets = detail.readSecrets ?? [];
  let readsHtml = "";
  if (reads.length || readSecrets.length) {
    const isExternal = (p: string): boolean => p.startsWith("/") || /^[A-Za-z]:[\\/]/.test(p);
    const secretPaths = new Set(readSecrets.map((s) => s.path));
    const shortLabel = (p: string): string => {
      const segs = p.split("/").filter(Boolean);
      return segs.length > 1 ? segs.slice(-2).join("/") : p;
    };
    const secretChips = readSecrets.map((s) => ({
      text: `${basename(s.path)} · ${s.kinds[0] ?? "secret"}`,
      title: `${s.path} — ${s.kinds.join(", ")}`,
      cls: "chip-secret",
    }));
    const externalChips = reads
      .filter((p) => isExternal(p) && !secretPaths.has(p))
      .map((p) => ({ text: shortLabel(p), title: p, cls: "chip-external" }));
    const repoChips = reads
      .filter((p) => !isExternal(p) && !secretPaths.has(p))
      .map((p) => ({ text: basename(p), title: p, cls: "" }));

    const rows: string[] = [];
    if (secretChips.length) {
      rows.push(
        chipsRow(
          "⚠ read secrets",
          "these files' CONTENT held secrets when the agent read them (kinds only — values are never stored)",
          secretChips,
        ),
      );
    }
    if (externalChips.length) {
      rows.push(
        chipsRow(
          "outside the repo",
          "reads from outside the project tree that reached the model's context",
          externalChips,
        ),
      );
    }
    if (repoChips.length) {
      rows.push(chipsRow("repo files", "project files loaded into the model's context", repoChips));
    }
    const stats = [
      `${reads.length} read`,
      readSecrets.length ? `${readSecrets.length} with secrets` : "",
      externalChips.length ? `${externalChips.length} external` : "",
    ]
      .filter(Boolean)
      .join(" · ");
    readsHtml = `<h4>files read for this commit</h4><p class="read-sum">${escapeHtml(stats)}</p><div class="vfiles">${rows.join("")}</div>`;
  }

  // Files changed (git-style): path + change kind + ±lines, each with an on-demand diff.
  const files = detail.changedFiles ?? [];
  let filesHtml = "";
  if (files.length) {
    const rows = files
      .map((f) => {
        const kind = f.changeKind ? `<span class="cf-kind">${escapeHtml(f.changeKind)}</span>` : "";
        const stat =
          f.linesAdded !== undefined || f.linesRemoved !== undefined
            ? `<span class="cf-stat"><span class="add">+${f.linesAdded ?? 0}</span> <span class="del">−${f.linesRemoved ?? 0}</span></span>`
            : "";
        const diff = f.diffRef
          ? `<button class="see-details" data-ref="${escapeHtml(f.diffRef)}">See details</button>` +
            `<div class="diff-slot" style="display:none"></div>`
          : "";
        return (
          `<div class="cf">` +
          `<div class="cf-head"><code class="cf-path">${escapeHtml(f.path)}</code>${kind}${stat}</div>` +
          diff +
          `</div>`
        );
      })
      .join("");
    filesHtml = `<h4>files changed (${files.length})</h4><div class="cf-list">${rows}</div>`;
  }

  const note = detail.note ? `<p class="muted">⚠ ${escapeHtml(detail.note)}</p>` : "";
  return `${header}${body}${promptsHtml}${readsHtml}${filesHtml}${note}`;
}

/** The rich, hydrated detail panel for a selected node. */
function renderDetail(node: GraphNode, detail: NodeDetail): string {
  if (detail.kind === "commit") return renderCommit(detail);
  const kind = detail.kind; // narrowed to "task" | "artifact" — commit handled above
  const header = renderHeader(node, detail);

  // The real authorship: which intents (agent/human tasks) changed this artifact. git only
  // records the committer; that an agent wrote it is known only through these reconciled links.
  let intentsHtml = "";
  if (detail.kind === "artifact" && detail.intents) {
    if (detail.intents.length) {
      const items = detail.intents
        .map(
          (i) =>
            `<li>${escapeHtml(i.agent || "?")}${i.author ? ` (${escapeHtml(i.author)})` : ""} — ${escapeHtml(i.when)}</li>`,
        )
        .join("");
      intentsHtml = `<h4>changed by intent</h4><ul>${items}</ul>`;
    } else {
      intentsHtml = `<h4>changed by intent</h4><p class="muted">no agent intent linked — recorded only as a git commit by its committer</p>`;
    }
  }

  // A leaf artifact (a single file or symbol) is focused enough that its latest diff is shown
  // immediately on selection — still fetched on demand. Higher levels keep the manual button.
  const isLeaf = detail.kind === "artifact" && (detail.level === "file" || detail.level === "symbol");
  const versions = detail.versions
    .slice(-8) // most recent few; histories can be long
    .reverse()
    .map((v, i) => renderVersion(kind, v, isLeaf && i === 0))
    .join("");

  const note = detail.note ? `<p class="muted">⚠ ${escapeHtml(detail.note)}</p>` : "";
  const count = detail.versions.length;
  return (
    `${header}${intentsHtml}` +
    `<h4>history (${count} version${count === 1 ? "" : "s"})</h4>${versions}${note}`
  );
}

function renderVersion(kind: "task" | "artifact", v: DetailVersion, autoload = false): string {
  const parts: string[] = [];
  parts.push(`<div class="ts">${escapeHtml(v.timestamp)}</div>`);
  if (kind === "artifact") {
    const meta: string[] = [];
    if (v.changeKind) meta.push(escapeHtml(v.changeKind));
    if (v.linesAdded !== undefined || v.linesRemoved !== undefined) {
      meta.push(`+${v.linesAdded ?? 0}/-${v.linesRemoved ?? 0}`);
    }
    if (v.committer) meta.push(`committed by ${escapeHtml(v.committer)}`);
    if (v.source) meta.push(escapeHtml(v.source));
    if (meta.length) parts.push(`<div>${meta.join(" · ")}</div>`);
    // The diff is always fetched on demand; `data-autoload` makes a leaf node load it right away.
    if (v.diffRef) {
      if (v.diffOfPath) {
        parts.push(
          `<div class="muted">diff of containing file: <code>${escapeHtml(v.diffOfPath)}</code></div>`,
        );
      }
      const auto = autoload ? " data-autoload" : "";
      parts.push(
        `<button class="see-details" data-ref="${escapeHtml(v.diffRef)}"${auto}>See details</button>` +
          `<div class="diff-slot" style="display:none"></div>`,
      );
    }
  } else {
    // A turn card: the summary as readable prose, then the files as chips grouped by what they
    // MEAN — declared (announced), undeclared (changed silently — the drift that matters), and
    // phantom (announced but never observed). Hover a chip for the full path.
    if (v.summary) parts.push(`<p class="vsum">${escapeHtml(truncate(v.summary, 600))}</p>`);
    const rows: string[] = [];
    if (v.declared && v.declared.length) {
      rows.push(
        fileRow("declared", "files this turn said it changed", v.declared, ""),
      );
    }
    if (v.driftUndeclared && v.driftUndeclared.length) {
      rows.push(
        fileRow(
          "⚠ changed, not declared",
          "the agent changed these without saying so",
          v.driftUndeclared,
          "undeclared",
        ),
      );
    }
    if (v.driftPhantom && v.driftPhantom.length) {
      rows.push(
        fileRow(
          "declared, not observed",
          "the agent said it changed these, but no change was seen",
          v.driftPhantom,
          "phantom",
        ),
      );
    }
    if (rows.length) parts.push(`<div class="vfiles">${rows.join("")}</div>`);
  }
  return `<div class="version">${parts.join("")}</div>`;
}

/** A labeled chip row from prebuilt chips (text/title/cls), capped at 12 with "+N more". */
function chipsRow(
  label: string,
  explain: string,
  items: Array<{ text: string; title: string; cls: string }>,
): string {
  const MAX = 12;
  const chips = items
    .map(
      (c, i) =>
        `<span class="chip${c.cls ? ` ${c.cls}` : ""}${i >= MAX ? " chip-hidden" : ""}" title="${escapeHtml(c.title)}">${escapeHtml(c.text)}</span>`,
    )
    .join("");
  const more =
    items.length > MAX
      ? `<button class="chip chip-more" data-count="${items.length - MAX}">+${items.length - MAX} more</button>`
      : "";
  return (
    `<div class="vf-row">` +
    `<span class="vf-label" title="${escapeHtml(explain)}">${escapeHtml(label)}</span>` +
    `<span class="chips">${chips}${more}</span>` +
    `</div>`
  );
}

/** One labeled chip row of the turn card (label tooltip explains what the group means). */
function fileRow(label: string, explain: string, paths: string[], kind: string): string {
  const MAX = 12;
  // Disambiguate duplicate basenames (two Cargo.toml) with their parent dir.
  const counts = new Map<string, number>();
  for (const p of paths) counts.set(basename(p), (counts.get(basename(p)) ?? 0) + 1);
  const label_of = (p: string): string => {
    if ((counts.get(basename(p)) ?? 0) < 2) return basename(p);
    const segs = p.split("/");
    return segs.slice(-2).join("/");
  };
  const cls = kind ? ` chip-${kind}` : "";
  // Everything is rendered; chips beyond MAX start hidden and the "+N more" button toggles them.
  const chips = paths
    .map(
      (p, i) =>
        `<span class="chip${cls}${i >= MAX ? " chip-hidden" : ""}" title="${escapeHtml(p)}">${escapeHtml(label_of(p))}</span>`,
    )
    .join("");
  const more =
    paths.length > MAX
      ? `<button class="chip chip-more" data-count="${paths.length - MAX}">+${paths.length - MAX} more</button>`
      : "";
  return (
    `<div class="vf-row">` +
    `<span class="vf-label" title="${escapeHtml(explain)}">${escapeHtml(label)}</span>` +
    `<span class="chips">${chips}${more}</span>` +
    `</div>`
  );
}

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}… (${text.length} chars)` : text;
}

/** Render a unified diff git-style: added lines green, removed red, hunks/meta dimmed. */
function renderDiffHtml(diff: string): string {
  const MAX_LINES = 4000;
  const lines = diff.split("\n");
  const shown = lines.slice(0, MAX_LINES);
  const rows = shown
    .map((line) => {
      let cls = "ctx";
      if (line.startsWith("+++") || line.startsWith("---")) cls = "meta";
      else if (line.startsWith("+")) cls = "add";
      else if (line.startsWith("-")) cls = "del";
      else if (line.startsWith("@@")) cls = "hunk";
      else if (line.startsWith("diff ") || line.startsWith("index ")) cls = "meta";
      return `<span class="dl ${cls}">${escapeHtml(line) || " "}</span>`;
    })
    .join("\n");
  const more =
    lines.length > MAX_LINES
      ? `\n<span class="dl meta">… ${lines.length - MAX_LINES} more lines</span>`
      : "";
  return `<pre class="diff">${rows}${more}</pre>`;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;",
  );
}

void main();
