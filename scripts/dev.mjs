#!/usr/bin/env node
// Additive all-in-one dev launcher. The manual commands remain the source of truth; this just
// chains them: cargo build → TS build → brain0 ingest → brain0 observe → server + GUI.
//
//   pnpm dev --repo <id> [--path <dir>] [--port 8787] [--encrypt] [--all]
//            [--no-ingest] [--no-observe]
//
// The four manual commands (cargo build, brain0 ingest, brain0 observe, server start, gui dev)
// keep working byte-for-byte; nothing here mutates their behavior.
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

// ── arg parsing (minimal, no deps) ──────────────────────────────────────────
const argv = process.argv.slice(2);
const flag = (n) => argv.includes(`--${n}`);
const opt = (n, d) => {
  const i = argv.indexOf(`--${n}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : d;
};

const repo = opt("repo", process.env.BRAIN0_REPO ?? "");
// Mirror the server's refresh validation so dev and refresh agree on what a legal repo id is.
if (!repo || repo.startsWith("-") || !/^[A-Za-z0-9._/-]+$/.test(repo)) {
  console.error(
    "brain0 dev: --repo is required and must match /^[A-Za-z0-9._/-]+$/.\n" +
      "  e.g. pnpm dev --repo myorg/myrepo",
  );
  process.exit(1);
}
const repoPath = resolve(opt("path", "."));
if (!existsSync(repoPath)) {
  console.error(`brain0 dev: --path not found: ${repoPath}`);
  process.exit(1);
}
const port = Number(opt("port", process.env.PORT ?? "8787"));
const encrypt = flag("encrypt"); // dev default is UNencrypted (GUI shows real diffs)
const observeAll = flag("all");
const doIngest = !flag("no-ingest");
const doObserve = !flag("no-observe");

// ── env wiring (absolute paths; server cwd will be packages/server) ─────────
const db = resolve(ROOT, ".brain0/index.db");
const payload = resolve(ROOT, ".brain0/payload");
const bin = resolve(ROOT, "target/debug/brain0");
const env = {
  ...process.env,
  BRAIN0_REPO: repo,
  BRAIN0_DB: db,
  BRAIN0_PAYLOAD: payload,
  PORT: String(port),
  BRAIN0_SERVER: `http://localhost:${port}`,
  // The server's refresh endpoint reads these; they are NOT inferred server-side.
  BRAIN0_REPO_PATH: repoPath, // observed working tree (cwd for spawned brain0)
  BRAIN0_BIN: bin, // explicit binary path (no fragile layout probing)
  // Encryption mode is NOT passed via env. Refresh reads it from the persisted meta flag so it
  // cannot diverge from how the index was actually built.
};

// ── colored, prefixed child output ──────────────────────────────────────────
const COLORS = { build: 36, server: 35, gui: 32, ingest: 33, observe: 34 };
const prefixer = (label) => {
  const c = COLORS[label] ?? 37;
  return (chunk) => {
    for (const line of chunk.toString().split(/\r?\n/)) {
      if (line.length) process.stdout.write(`\x1b[${c}m[${label}]\x1b[0m ${line}\n`);
    }
  };
};

// ── child tracking + single shutdown (registered BEFORE first spawn) ────────
const longLived = [];
let shuttingDown = false;
function shutdown(code = 0) {
  if (shuttingDown) return;
  shuttingDown = true;
  for (const child of longLived) {
    try {
      process.kill(-child.pid, "SIGTERM");
    } catch {
      /* already gone */
    }
  }
  process.exit(code);
}
for (const sig of ["SIGINT", "SIGTERM", "exit"]) process.on(sig, () => shutdown());

// ── spawn helpers ───────────────────────────────────────────────────────────
// Gated prelude children: NOT detached, so on Ctrl-C they share the controlling terminal's
// process group and get SIGINT directly. Do NOT detach these — it would orphan them.
function run(label, cmd, args, cwd) {
  return new Promise((res, rej) => {
    const child = spawn(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    child.stdout.on("data", prefixer(label));
    child.stderr.on("data", prefixer(label));
    child.on("exit", (c) => (c === 0 ? res() : rej(new Error(`${label} exited with code ${c}`))));
    child.on("error", rej);
  });
}
// Long-lived children: detached → own process group → group-kill on shutdown.
function runLongLived(label, cmd, args, cwd) {
  const child = spawn(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"], detached: true });
  child.stdout.on("data", prefixer(label));
  child.stderr.on("data", prefixer(label));
  child.on("exit", (c) => {
    if (!shuttingDown) shutdown(c ?? 0);
  });
  longLived.push(child);
  return child;
}

const encArgs = encrypt ? [] : ["--no-encrypt-payload"];

async function poll(url, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if ((await fetch(url)).ok) return true;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`timed out waiting for ${url}`);
}

// ── orchestration ───────────────────────────────────────────────────────────
try {
  await run("build", "cargo", ["build", "-p", "brain0-cli"], ROOT);
  // Build exactly what the SERVER needs (mirrors scripts/e2e.sh filter set). The GUI is served
  // UNBUILT by vite dev, matching the manual `pnpm --filter @brain0/gui dev`.
  await run(
    "build",
    "pnpm",
    ["--filter", "@brain0/shared", "--filter", "@brain0/agent", "--filter", "@brain0/server", "run", "build"],
    ROOT,
  );

  if (doIngest)
    await run(
      "ingest",
      bin,
      ["ingest", "--repo", repo, "--path", repoPath, "--db", db, "--payload", payload, ...encArgs],
      ROOT,
    );
  else console.log("[dev] skipping ingest (--no-ingest); risk colors may be stale");

  if (doObserve)
    await run(
      "observe",
      bin,
      [
        "observe",
        "--repo",
        repo,
        "--path",
        repoPath,
        "--db",
        db,
        "--payload",
        payload,
        ...encArgs,
        ...(observeAll ? ["--all"] : []),
      ],
      ROOT,
    );
  else console.log("[dev] skipping observe (--no-observe); declared-side data may be stale");

  runLongLived("server", "pnpm", ["--filter", "@brain0/server", "start"], ROOT);
  await poll(`http://localhost:${port}/health`); // bounded; surfaces boot failure
  // Force the GUI port so the printed URL is truthful and a clash fails loudly (on WSL2 vite
  // silently auto-increments to 5174+ otherwise, and the banner would lie).
  runLongLived(
    "gui",
    "pnpm",
    ["--filter", "@brain0/gui", "dev", "--", "--port", "5173", "--strictPort"],
    ROOT,
  );

  console.log(`\n  brain0 dev up:  server :${port}   gui :5173   (Ctrl-C to stop)\n`);
} catch (err) {
  console.error(`[dev] aborted: ${err.message}`);
  shutdown(1);
}
