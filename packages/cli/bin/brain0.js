#!/usr/bin/env node
/**
 * The `brain0` npm CLI. One product command plus passthrough:
 *
 *   brain0 up [--path <dir>] [--repo <id>] [--port 8787] [--encrypt] [--all]
 *             [--no-ingest] [--no-observe] [--no-open]
 *       Index the repo (git facts + agent transcripts) and serve the GUI — one command,
 *       zero config: repo id is inferred from the git remote, data lives in <repo>/.brain0.
 *
 *   brain0 <ingest|observe|query|mcp|verify|audit|reembed|purge|watch> …
 *       Forwarded verbatim to the Rust core binary.
 *
 * Resolution order (works from the monorepo during development and from the published
 * package): BRAIN0_BIN → @brain0/cli-<platform>-<arch> package → monorepo target/{release,debug}.
 */

import { execFileSync, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inferRepoId, parseArgs, REPO_RE } from "../src/lib.mjs";

const PKG_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "..");
// In the monorepo this is packages/cli → two levels up is the repo root.
const MONOREPO_ROOT = resolve(PKG_DIR, "../..");

// ── component resolution ────────────────────────────────────────────────────
function resolveRustBin() {
  if (process.env.BRAIN0_BIN && existsSync(process.env.BRAIN0_BIN)) return process.env.BRAIN0_BIN;
  const exe = process.platform === "win32" ? "brain0.exe" : "brain0";
  // Published layout: platform package installed next to us (npm resolves optionalDependencies).
  const platformPkg = `@brain0/cli-${process.platform}-${process.arch}`;
  try {
    const entry = import.meta.resolve(`${platformPkg}/package.json`);
    const p = resolve(dirname(fileURLToPath(entry)), exe);
    if (existsSync(p)) return p;
  } catch {
    /* not installed — fall through to the dev layout */
  }
  for (const rel of [`target/release/${exe}`, `target/debug/${exe}`]) {
    const p = resolve(MONOREPO_ROOT, rel);
    if (existsSync(p)) return p;
  }
  return undefined;
}

function resolveServerEntry() {
  for (const p of [
    resolve(PKG_DIR, "vendor/server/server.js"), // published: bundled at release time
    resolve(MONOREPO_ROOT, "packages/server/dist/server.js"), // monorepo dev
  ]) {
    if (existsSync(p)) return p;
  }
  return undefined;
}

function resolveGuiDir() {
  if (process.env.BRAIN0_GUI_DIR) return process.env.BRAIN0_GUI_DIR;
  for (const p of [
    resolve(PKG_DIR, "vendor/gui"), // published: bundled at release time
    resolve(MONOREPO_ROOT, "packages/gui/build"), // monorepo dev
  ]) {
    if (existsSync(resolve(p, "index.html"))) return p;
  }
  return undefined;
}

// ── small process helpers (mirrors scripts/dev.mjs) ─────────────────────────
const COLORS = { ingest: 33, observe: 34, server: 35 };
const prefixer = (label) => {
  const c = COLORS[label] ?? 37;
  return (chunk) => {
    for (const line of chunk.toString().split(/\r?\n/)) {
      if (line.length) process.stdout.write(`\x1b[${c}m[${label}]\x1b[0m ${line}\n`);
    }
  };
};

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

function run(label, cmd, args, cwd, env) {
  return new Promise((res, rej) => {
    const child = spawn(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
    child.stdout.on("data", prefixer(label));
    child.stderr.on("data", prefixer(label));
    child.on("exit", (c) => (c === 0 ? res() : rej(new Error(`${label} exited with code ${c}`))));
    child.on("error", rej);
  });
}

function runLongLived(label, cmd, args, cwd, env) {
  const child = spawn(cmd, args, { cwd, env, stdio: ["ignore", "pipe", "pipe"], detached: true });
  child.stdout.on("data", prefixer(label));
  child.stderr.on("data", prefixer(label));
  child.on("exit", (c) => {
    if (!shuttingDown) shutdown(c ?? 0);
  });
  longLived.push(child);
  return child;
}

async function poll(url, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      /* not up yet */
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`timed out waiting for ${url}`);
}

function openBrowser(url) {
  const cmd =
    process.platform === "darwin" ? "open" : process.platform === "win32" ? "start" : "xdg-open";
  try {
    spawn(cmd, [url], { stdio: "ignore", detached: true }).unref();
  } catch {
    /* headless is fine — the URL is printed */
  }
}

// ── `up` ────────────────────────────────────────────────────────────────────
async function up(argv) {
  const { flags, opts } = parseArgs(argv, ["path", "repo", "port"]);
  const repoPath = resolve(opts.get("path") ?? ".");
  if (!existsSync(repoPath)) {
    console.error(`brain0 up: --path not found: ${repoPath}`);
    process.exit(1);
  }

  let repo = opts.get("repo") ?? "";
  if (!repo) {
    let remote = "";
    try {
      remote = execFileSync("git", ["-C", repoPath, "remote", "get-url", "origin"], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      }).trim();
    } catch {
      /* no git remote — fall back to the directory name */
    }
    repo = inferRepoId(remote, basename(repoPath));
  }
  if (!REPO_RE.test(repo) || repo.startsWith("-")) {
    console.error(`brain0 up: invalid repo id "${repo}" (allowed: letters digits . _ / -)`);
    process.exit(1);
  }

  const bin = resolveRustBin();
  if (!bin) {
    console.error(
      "brain0 up: core binary not found.\n" +
        `  Expected @brain0/cli-${process.platform}-${process.arch} (npm) or target/release/brain0 (repo).\n` +
        "  From the repo: cargo build --release -p brain0-cli",
    );
    process.exit(1);
  }
  const serverEntry = resolveServerEntry();
  if (!serverEntry) {
    console.error(
      "brain0 up: server not found — from the repo run: pnpm --filter @brain0/server build",
    );
    process.exit(1);
  }
  const guiDir = resolveGuiDir(); // optional: API still works without it

  const port = Number(opts.get("port") ?? process.env.PORT ?? "8787");
  const db = resolve(repoPath, ".brain0/index.db");
  const payload = resolve(repoPath, ".brain0/payload");
  // The GUI must read diffs/summaries, so `up` defaults to a plaintext payload (opt back into
  // at-rest encryption with --encrypt; the GUI then shows an "encrypted" notice instead of text).
  const encArgs = flags.has("encrypt") ? [] : ["--no-encrypt-payload"];
  const common = ["--repo", repo, "--path", repoPath, "--db", db, "--payload", payload, ...encArgs];

  const env = {
    ...process.env,
    BRAIN0_REPO: repo,
    BRAIN0_DB: db,
    BRAIN0_PAYLOAD: payload,
    BRAIN0_REPO_PATH: repoPath,
    BRAIN0_BIN: bin,
    PORT: String(port),
    ...(guiDir ? { BRAIN0_GUI_DIR: guiDir } : {}),
  };

  console.log(`brain0 up · repo=${repo} · path=${repoPath}`);
  try {
    if (!flags.has("no-ingest")) await run("ingest", bin, ["ingest", ...common], repoPath, env);
    if (!flags.has("no-observe"))
      await run(
        "observe",
        bin,
        ["observe", ...common, ...(flags.has("all") ? ["--all"] : [])],
        repoPath,
        env,
      );

    runLongLived(
      "server",
      process.execPath,
      ["--experimental-sqlite", serverEntry],
      repoPath,
      env,
    );
    await poll(`http://localhost:${port}/health`);

    const url = `http://localhost:${port}/`;
    console.log(
      guiDir
        ? `\n  brain0 is up → ${url}   (Ctrl-C to stop)\n`
        : `\n  brain0 API is up → ${url} (GUI build not bundled — set BRAIN0_GUI_DIR)\n`,
    );
    if (guiDir && !flags.has("no-open")) openBrowser(url);
  } catch (err) {
    console.error(`brain0 up: aborted — ${err.message}`);
    shutdown(1);
  }
}

// ── dispatcher ──────────────────────────────────────────────────────────────
const HELP = `brain0 — the black box for AI-written code

Usage:
  brain0 up [--path <dir>] [--repo <id>] [--port 8787] [--encrypt] [--all]
            [--no-ingest] [--no-observe] [--no-open]
  brain0 <ingest|observe|query|mcp|verify|audit|reembed|purge|watch> [args…]

\`up\` indexes the repo (git facts + coding-agent transcripts) and serves the GUI at
http://localhost:8787. Everything else is forwarded to the brain0 core binary.`;

const [cmd, ...rest] = process.argv.slice(2);
if (!cmd || cmd === "--help" || cmd === "-h" || cmd === "help") {
  console.log(HELP);
  process.exit(0);
}
if (cmd === "up") {
  await up(rest);
} else {
  const bin = resolveRustBin();
  if (!bin) {
    console.error(
      `brain0: core binary not found for "${cmd}" — install @brain0/cli-${process.platform}-${process.arch} or build target/release/brain0`,
    );
    process.exit(1);
  }
  const child = spawn(bin, [cmd, ...rest], { stdio: "inherit" });
  child.on("exit", (code) => process.exit(code ?? 0));
  child.on("error", (err) => {
    console.error(`brain0: ${err.message}`);
    process.exit(1);
  });
}
