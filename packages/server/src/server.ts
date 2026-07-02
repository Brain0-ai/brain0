/**
 * Minimal HTTP backend for the GUI. The browser cannot read the
 * SQLite index directly, so this Node server (run with `--experimental-sqlite`) exposes:
 *
 *   GET /graph.json     → the render-ready graph snapshot (buildGraphSnapshot)
 *   GET /api/debug?q=   → the smart chat: auto-intent debug/audit, structured findings + prose
 *   GET /*              → the built GUI (packages/gui/build or BRAIN0_GUI_DIR), so one process
 *                         serves both API and app (`brain0 up`); vite dev keeps working via proxy
 *
 * The LLM is LOCAL (Ollama, the default) or REMOTE (Anthropic/OpenAI when a key is present); there
 * is no offline reasoner at runtime. Embeddings default to local. When a channel is remote its
 * context is redacted before egress, and the active provider + egress state are surfaced to the GUI.
 */

import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { resolve } from "node:path";
import { BoundedCache, normalizeQuery } from "./askcache.js";
import { cacheControlFor, contentTypeFor, safeJoin } from "./staticfiles.js";
import {
  Brain0Store,
  FsPayloadReader,
  buildGraphSnapshot,
  buildNodeDetail,
  readablePayload,
} from "@brain0/shared";
import {
  Brain0Agent,
  ClaudeProvider,
  EchoLLM,
  LocalEmbeddingProvider,
  OpenAICompatProvider,
  OpenAIEmbeddingProvider,
  backfillEmbeddings,
  resolveEmbedderConfig,
  resolveLlmConfig,
  type EmbeddingProvider,
  type GetText,
  type LLMProvider,
} from "@brain0/agent";

// Embeddings default to LOCAL even when OPENAI_API_KEY is present (closing the silent egress of the
// raw query); the remote OpenAI embedder is used only via BRAIN0_EMBED_PROVIDER=openai.
function buildEmbedder(): EmbeddingProvider {
  const c = resolveEmbedderConfig(process.env);
  if (c.provider === "openai") return new OpenAIEmbeddingProvider(c.apiKey, c.model, 1536, c.endpoint);
  return new LocalEmbeddingProvider(c.dim);
}

// LOCAL Ollama by default; a present API key opts into that remote provider; `echo` only when
// explicitly pinned (tests/debug). NEVER a silent offline default. Probes once for a truthful
// boot log + GUI notice; a provider unreachable at request time is handled by the route.
async function buildLlm(): Promise<LLMProvider> {
  const c = resolveLlmConfig(process.env);
  let provider: LLMProvider;
  switch (c.provider) {
    case "anthropic":
      provider = new ClaudeProvider(c.apiKey, c.model, 4096, c.endpoint);
      break;
    case "openai":
      provider = new OpenAICompatProvider(c.apiKey, c.model, c.endpoint, { name: "openai", remote: true });
      break;
    case "echo":
      provider = new EchoLLM();
      break;
    default:
      provider = new OpenAICompatProvider("", c.model, c.endpoint, { name: "ollama", remote: false });
      break;
  }
  const ok = provider.probe ? await provider.probe() : true;
  if (provider.descriptor) provider.descriptor.ok = ok;
  return provider;
}

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, {
    "content-type": "application/json",
    "access-control-allow-origin": "*",
  });
  res.end(JSON.stringify(body));
}

// Resolve to absolute paths against the current working directory. This matters because
// `pnpm --filter @brain0/server start` runs with the cwd set to packages/server, so a relative
// default like ".brain0/index.db" would otherwise point at the wrong place.
const db = resolve(process.env.BRAIN0_DB ?? ".brain0/index.db");
const payloadDir = resolve(process.env.BRAIN0_PAYLOAD ?? ".brain0/payload");
const repo = process.env.BRAIN0_REPO ?? "";
const port = Number(process.env.PORT ?? 8787);

if (!existsSync(db)) {
  console.error(
    `brain0-server: index database not found at ${db}\n` +
      `  (cwd is ${process.cwd()}; pnpm runs scripts from the package directory)\n` +
      `  Pass an ABSOLUTE path, run from the repo root, e.g.:\n` +
      `    BRAIN0_REPO=${repo || "myorg/myrepo"} \\\n` +
      `    BRAIN0_DB="$PWD/.brain0/index.db" BRAIN0_PAYLOAD="$PWD/.brain0/payload" \\\n` +
      `      pnpm --filter @brain0/server start\n` +
      `  Create the index first with: brain0 observe (and/or brain0 ingest).`,
  );
  process.exit(1);
}

const store = new Brain0Store(db);
const payload = new FsPayloadReader(payloadDir);
const getText: GetText = (ref) => payload.getText(ref);
const embedder = buildEmbedder();
const llm = await buildLlm();
const agent = new Brain0Agent({ store, embedder, llm, getText, repo: repo || undefined });

// ── Live refresh (local dev only) ───────────────────────────────────────────────────────────
// The dashboard's Refresh button re-runs the SAME passive observer the user runs by hand
// (`brain0 ingest` then `brain0 observe`), then backfills embeddings. Nothing here mutates the
// observed repo or the existing read-only routes; it just re-invokes the CLI and re-reads.

// Compiled to packages/server/dist/server.js → up three levels (dist→server→packages→root).
const repoRoot = resolve(import.meta.dirname, "../../..");

// The built GUI (vite output). Explicit override first (the npm-packaged CLI sets it), else the
// monorepo layout. When absent the API still works — the GUI routes 404 with a build hint.
const guiDir = process.env.BRAIN0_GUI_DIR
  ? resolve(process.env.BRAIN0_GUI_DIR)
  : resolve(repoRoot, "packages/gui/build");
const guiAvailable = existsSync(resolve(guiDir, "index.html"));

/** Serve a file from the built GUI, with an index.html fallback (the GUI is a single page). */
async function serveStatic(pathname: string, res: ServerResponse): Promise<void> {
  if (!guiAvailable) {
    json(res, 404, {
      error: "not found",
      hint: "GUI build not present — run `pnpm --filter @brain0/gui build` (or set BRAIN0_GUI_DIR)",
    });
    return;
  }
  const wanted = pathname === "/" ? "/index.html" : pathname;
  const file = safeJoin(guiDir, wanted);
  if (!file) {
    json(res, 403, { error: "forbidden" });
    return;
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, {
      "content-type": contentTypeFor(file),
      "cache-control": cacheControlFor(wanted),
    });
    res.end(body);
  } catch {
    try {
      const body = await readFile(resolve(guiDir, "index.html"));
      res.writeHead(200, { "content-type": "text/html; charset=utf-8", "cache-control": "no-cache" });
      res.end(body);
    } catch {
      json(res, 404, { error: "not found" });
    }
  }
}

// The observed working tree must be EXPLICIT (scripts/dev.mjs sets BRAIN0_REPO_PATH). It is NOT
// inferred from payloadDir — the payload may live anywhere; brain0 observes an external repo.
const repoPath = process.env.BRAIN0_REPO_PATH; // undefined → /api/refresh returns 503

// Argument-injection guard for the repo id passed to the spawned CLI.
const REPO_RE = /^[A-Za-z0-9._/-]+$/;

/** Locate the brain0 binary: explicit override first, then the usual cargo output dirs. */
function resolveBin(): string | undefined {
  if (process.env.BRAIN0_BIN && existsSync(process.env.BRAIN0_BIN)) return process.env.BRAIN0_BIN;
  for (const p of ["target/release/brain0", "target/debug/brain0"]) {
    const abs = resolve(repoRoot, p);
    if (existsSync(abs)) return abs;
  }
  return undefined;
}

/** Read the persisted at-rest mode; fail closed to ENCRYPTED for a legacy index (key absent). */
function payloadIsEncrypted(): boolean {
  try {
    const row = store.raw().prepare("SELECT value FROM meta WHERE key='payload_encryption'").get() as
      | { value?: string }
      | undefined;
    return row?.value !== "plaintext"; // 'encrypted' OR absent → treat as encrypted
  } catch {
    return true;
  }
}

type RefreshState = "idle" | "running" | "done" | "error";
const refresh: {
  state: RefreshState;
  phase: string;
  jobId: string;
  startedAt: number;
  finishedAt: number;
  error: string;
  lines: string[];
} = { state: "idle", phase: "", jobId: "", startedAt: 0, finishedAt: 0, error: "", lines: [] };

function pushLine(s: string): void {
  const last = refresh.lines.at(-1);
  if (s && s !== last) {
    // de-dupe consecutive CLI progress-redraw fragments; keep a bounded ring buffer
    refresh.lines.push(s);
    if (refresh.lines.length > 50) refresh.lines.shift();
  }
}

function spawnStep(bin: string, args: string[]): Promise<void> {
  return new Promise((res, rej) => {
    const child = spawn(bin, args, { cwd: repoPath, env: process.env }); // no shell
    const capture = (buf: Buffer): void => {
      // CliProgress overwrites in place with \r; final stats use \n. Split on BOTH.
      for (const raw of buf.toString().split(/[\r\n]+/)) {
        const l = raw.trim();
        if (l) pushLine(l);
      }
    };
    child.stdout.on("data", capture);
    child.stderr.on("data", capture);
    child.on("exit", (c) => (c === 0 ? res() : rej(new Error(`${args[0]} exited with code ${c}`))));
    child.on("error", rej);
  });
}

async function runRefresh(): Promise<void> {
  const bin = resolveBin();
  refresh.state = "running";
  refresh.jobId = randomUUID();
  refresh.startedAt = Date.now();
  refresh.finishedAt = 0;
  refresh.error = "";
  refresh.lines = [];
  const enc = payloadIsEncrypted() ? [] : ["--no-encrypt-payload"]; // mode from disk, never env-default
  const common = ["--repo", repo, "--path", repoPath!, "--db", db, "--payload", payloadDir, ...enc];
  try {
    refresh.phase = "ingest";
    await spawnStep(bin!, ["ingest", ...common]); // recompute_risk runs inside ingest
    refresh.phase = "observe";
    await spawnStep(bin!, ["observe", ...common]); // cursor-based, idempotent
    refresh.phase = "embed";
    // Only AFTER both Rust children exit → never contends with the Rust writer. Same call as
    // startup below: closes the startup-only embedding gap so new Tasks are searchable.
    await backfillEmbeddings(store, getText, embedder);
    refresh.state = "done";
  } catch (err) {
    refresh.state = "error";
    refresh.error = String(err);
  } finally {
    refresh.finishedAt = Date.now();
  }
}

// Backfill any missing embeddings on startup so search works immediately.
await backfillEmbeddings(store, getText, embedder);

// The JSON shape both smart-chat phases share (additive over the legacy fields).
function shapeAsk(result: Awaited<ReturnType<Brain0Agent["ask"]>>): Record<string, unknown> {
  return {
    tasks: result.highlights.tasks, // legacy
    artifacts: result.highlights.artifacts, // legacy
    explanation: result.explanation, // legacy
    intent: result.intent,
    confidence: result.confidence,
    findings: result.findings,
    distribution: result.distribution,
    goldSignals: result.goldSignals,
    topRisky: result.topRisky,
    provider: {
      name: result.egress.llm.name,
      remote: result.egress.llm.remote,
      ok: result.egress.llm.ok,
      embedder: result.egress.embedder,
      redacted: result.egress.redacted,
      zeroEgress: result.egress.zeroEgress,
    },
    error: result.error,
  };
}

// Repeat questions must not re-pay the LLM: full ask() results are cached per (query, index
// generation). The generation probe is three cheap aggregates; any ingest/refresh changes it,
// invalidating every stale entry implicitly.
const askCache = new BoundedCache<Record<string, unknown>>(100);
function indexGeneration(): string {
  try {
    const row = store
      .raw()
      .prepare(
        "SELECT (SELECT COUNT(*) FROM task_versions) tv, (SELECT COUNT(*) FROM artifact_versions) av, (SELECT COALESCE(MAX(timestamp),'') FROM artifact_versions) m",
      )
      .get() as { tv?: number; av?: number; m?: string } | undefined;
    return `${row?.tv ?? 0}:${row?.av ?? 0}:${row?.m ?? ""}`;
  } catch {
    return String(Date.now()); // probe failed → never serve stale
  }
}

const server = createServer((req, res) => {
  const url = new URL(req.url ?? "/", "http://localhost");
  void (async () => {
    try {
      if (url.pathname === "/graph.json") {
        json(res, 200, buildGraphSnapshot(store, repo));
      } else if (url.pathname === "/api/node") {
        const detail = await buildNodeDetail(store, getText, url.searchParams.get("id") ?? "");
        if (detail) json(res, 200, detail);
        else json(res, 404, { error: "node not found" });
      } else if (url.pathname === "/api/diff") {
        // Lazy, on-demand: only the requested diff text is read from the payload store.
        const raw = await getText(url.searchParams.get("ref") ?? "");
        if (raw === undefined) {
          json(res, 404, { error: "diff not found" });
        } else {
          const text = readablePayload(raw);
          json(res, 200, text === undefined ? { encrypted: true } : { diff: text });
        }
      } else if (url.pathname === "/api/debug") {
        // Two-phase smart chat. phase=retrieve → highlights in milliseconds, zero LLM tokens
        // (the GUI paints these immediately). The full phase runs the LLM once per
        // (query, index generation): repeats are served from the bounded cache. `ask()` never
        // throws on LLM failure — it returns `error` with the retrieval floor.
        const q = url.searchParams.get("q") ?? "";
        if (url.searchParams.get("phase") === "retrieve") {
          json(res, 200, shapeAsk(await agent.retrieve(q)));
        } else {
          const key = `${indexGeneration()}|${normalizeQuery(q)}`;
          const hit = askCache.get(key);
          if (hit) {
            json(res, 200, { ...hit, cached: true });
          } else {
            const result = await agent.ask(q);
            const shaped = shapeAsk(result);
            if (!result.error) askCache.set(key, shaped); // never cache failures
            json(res, 200, shaped);
          }
        }
      } else if (url.pathname === "/health") {
        json(res, 200, {
          ok: true,
          repo,
          db,
          llm: { name: llm.descriptor?.name ?? "echo", remote: llm.descriptor?.remote ?? false, ok: llm.descriptor?.ok },
          embedder: { name: embedder.descriptor?.name ?? "local", remote: embedder.descriptor?.remote ?? false },
          zeroEgress: !(llm.descriptor?.remote ?? false) && !(embedder.descriptor?.remote ?? false),
        });
      } else if (url.pathname === "/api/refresh" && req.method === "POST") {
        // Same-origin / CSRF guard: the custom header forces a CORS preflight that the
        // wildcard-less policy denies for foreign origins, and Sec-Fetch-Site blocks simple
        // cross-site POSTs. This is the first state-changing, process-spawning route.
        const sameSite = (req.headers["sec-fetch-site"] ?? "same-origin") as string;
        if (req.headers["x-brain0-refresh"] !== "1" || (sameSite !== "same-origin" && sameSite !== "none")) {
          json(res, 403, { error: "forbidden: refresh is same-origin only" });
        } else if (!repo || !REPO_RE.test(repo) || repo.startsWith("-")) {
          json(res, 503, {
            error: "BRAIN0_REPO unset or invalid",
            hint: "start the server with a valid BRAIN0_REPO=<id>",
          });
        } else if (!repoPath || !existsSync(repoPath) || repoPath.startsWith("-")) {
          json(res, 503, {
            error: "BRAIN0_REPO_PATH unset or not a directory",
            hint: "set BRAIN0_REPO_PATH to the observed repo working tree (dev.mjs sets it)",
          });
        } else if (!resolveBin()) {
          json(res, 503, { error: "brain0 binary not found", hint: "run: cargo build -p brain0-cli" });
        } else if (refresh.state === "running") {
          json(res, 409, { error: "refresh already running", jobId: refresh.jobId });
        } else {
          void runRefresh();
          json(res, 202, { jobId: refresh.jobId, state: "running" });
        }
      } else if (url.pathname === "/api/refresh/status") {
        json(res, 200, refresh);
      } else if (req.method === "GET" || req.method === "HEAD") {
        // Everything else is the app itself: serve the built GUI from this same process/port.
        await serveStatic(url.pathname, res);
      } else {
        json(res, 404, { error: "not found" });
      }
    } catch (err) {
      json(res, 500, { error: String(err) });
    }
  })();
});

// Bind to loopback only: a state-changing, process-spawning endpoint must never be exposed on
// the LAN. The read-only routes were only ever meant for localhost anyway.
const zeroEgress = !(llm.descriptor?.remote ?? false) && !(embedder.descriptor?.remote ?? false);
server.listen(port, "127.0.0.1", () => {
  console.log(`brain0-server listening on :${port} (repo=${repo || "<unset>"} db=${db})`);
  console.log(
    guiAvailable
      ? `  gui: http://localhost:${port}/ (serving ${guiDir})`
      : `  gui: not built — API only (run \`pnpm --filter @brain0/gui build\` or set BRAIN0_GUI_DIR)`,
  );
  console.log(
    `  llm: ${llm.descriptor?.name ?? "echo"}${llm.descriptor?.ok === false ? " (UNREACHABLE — run `ollama serve` or set a key)" : ""}` +
      ` · embeddings: ${embedder.descriptor?.name ?? "local"} · ${zeroEgress ? "zero egress (all local)" : "REMOTE — context redacted before egress"}`,
  );
});
