#!/usr/bin/env node
// Release versioning (used by .github/workflows/auto-release.yml and release.yml, and locally):
//
//   node scripts/release-version.mjs current            → version in the manifests
//   node scripts/release-version.mjs next <level>       → version the next release takes
//   node scripts/release-version.mjs set <X.Y.Z>        → write X.Y.Z into every manifest
//
// The git tag is the source of truth for a release's version. main's ruleset (pull request +
// review) does not let a workflow push a "bump version" commit, so a merged pull request is
// released by tagging its merge commit, and release.yml stamps the tag's version into the
// manifests of its own checkout before building. The committed manifests therefore only move
// when someone raises them in a pull request, and `next` honours that:
//
//   next = max(latest v* tag bumped by <level>, version in the manifests)
//
// <level> is patch | minor | major. Pre-release tags (v1.2.3-rc.1) are ignored.
//
// `set` rewrites the [workspace.package] version in Cargo.toml, the workspace members'
// entries in Cargo.lock (so `cargo build --locked` still passes), and the root and
// packages/* package.json files.

import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const LEVELS = ["patch", "minor", "major"];

/** Parse "X.Y.Z" into [X, Y, Z]; throws on anything else (no pre-release, no build metadata). */
export function parse(version) {
  const m = SEMVER.exec(version);
  if (!m) throw new Error(`not a release version (want X.Y.Z): "${version}"`);
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

/** Compare two X.Y.Z versions: negative, zero or positive. */
export function compare(a, b) {
  const [pa, pb] = [parse(a), parse(b)];
  for (let i = 0; i < 3; i++) if (pa[i] !== pb[i]) return pa[i] - pb[i];
  return 0;
}

/** Bump X.Y.Z by patch | minor | major. */
export function bump(version, level) {
  const [major, minor, patch] = parse(version);
  switch (level) {
    case "patch":
      return `${major}.${minor}.${patch + 1}`;
    case "minor":
      return `${major}.${minor + 1}.0`;
    case "major":
      return `${major + 1}.0.0`;
    default:
      throw new Error(`unknown bump level "${level}" (want ${LEVELS.join(" | ")})`);
  }
}

/** Highest release version among tag names ("v1.2.3"); null when there is none. */
export function latestTag(tags) {
  let best = null;
  for (const tag of tags) {
    const v = tag.trim().replace(/^v/, "");
    if (!tag.trim().startsWith("v") || !SEMVER.test(v)) continue;
    if (best === null || compare(v, best) > 0) best = v;
  }
  return best;
}

/** The version the next release takes (see the header). */
export function nextVersion({ tags, current, level }) {
  parse(current);
  if (!LEVELS.includes(level)) throw new Error(`unknown bump level "${level}"`);
  const latest = latestTag(tags);
  if (latest === null) return current;
  const bumped = bump(latest, level);
  return compare(current, bumped) > 0 ? current : bumped;
}

/** Cargo.toml: rewrite `version` in [workspace.package] only. */
export function stampCargoToml(text, version) {
  // The section body runs up to the next table header or the end of the file.
  const m = /^\[workspace\.package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(text);
  if (!m) throw new Error("Cargo.toml: no [workspace.package] section");
  const line = /^version\s*=\s*"[^"]*"/m;
  if (!line.test(m[1])) throw new Error("Cargo.toml: [workspace.package] has no version");
  const start = m.index + m[0].length - m[1].length;
  return text.slice(0, start) + m[1].replace(line, `version = "${version}"`) + text.slice(start + m[1].length);
}

/**
 * Cargo.lock: rewrite the version of every workspace member. Members are local packages, so
 * their entries carry no `source` line; registry packages with the same name are left alone.
 */
export function stampCargoLock(text, version, members) {
  const seen = new Set();
  const out = text
    .split(/(?=^\[\[package\]\]$)/m)
    .map((block) => {
      const name = /^name = "([^"]+)"$/m.exec(block)?.[1];
      if (!name || !members.has(name) || /^source = /m.test(block)) return block;
      seen.add(name);
      return block.replace(/^version = "[^"]*"$/m, `version = "${version}"`);
    })
    .join("");
  const missing = [...members].filter((n) => !seen.has(n));
  if (missing.length) throw new Error(`Cargo.lock: no entry for workspace member(s) ${missing.join(", ")}`);
  return out;
}

/** package.json: rewrite the top-level "version" in place (keeps the file's formatting). */
export function stampPackageJson(text, version, file = "package.json") {
  const out = text.replace(/^(\s*"version"\s*:\s*")[^"]*(")/m, `$1${version}$2`);
  if (JSON.parse(out).version !== version) throw new Error(`${file}: could not set the version`);
  return out;
}

// ── filesystem side ─────────────────────────────────────────────────────────

const read = (p) => readFileSync(resolve(ROOT, p), "utf8");

function packageJsonFiles() {
  const dirs = readdirSync(resolve(ROOT, "packages"), { withFileTypes: true })
    .filter((d) => d.isDirectory() && existsSync(resolve(ROOT, "packages", d.name, "package.json")))
    .map((d) => `packages/${d.name}/package.json`)
    .sort();
  return ["package.json", ...dirs];
}

function workspaceMembers() {
  const toml = read("Cargo.toml");
  const list = /^members\s*=\s*\[([\s\S]*?)\]/m.exec(toml)?.[1];
  if (!list) throw new Error("Cargo.toml: no workspace members");
  return new Set(
    [...list.matchAll(/"([^"]+)"/g)].map(([, dir]) => {
      const name = /^name\s*=\s*"([^"]+)"/m.exec(read(`${dir}/Cargo.toml`))?.[1];
      if (!name) throw new Error(`${dir}/Cargo.toml: no package name`);
      return name;
    }),
  );
}

/** The version in the manifests; they must all agree. */
export function currentVersion() {
  const cargo = /^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m.exec(read("Cargo.toml"))?.[1];
  const found = new Map([["Cargo.toml", cargo]]);
  for (const f of packageJsonFiles()) found.set(f, JSON.parse(read(f)).version);
  const distinct = new Set(found.values());
  if (distinct.size !== 1) {
    const detail = [...found].map(([f, v]) => `  ${f}: ${v}`).join("\n");
    throw new Error(`the manifests disagree on the version:\n${detail}`);
  }
  parse(cargo);
  return cargo;
}

function setVersion(version) {
  parse(version);
  const members = workspaceMembers();
  const writes = [
    ["Cargo.toml", stampCargoToml(read("Cargo.toml"), version)],
    ["Cargo.lock", stampCargoLock(read("Cargo.lock"), version, members)],
    ...packageJsonFiles().map((f) => [f, stampPackageJson(read(f), version, f)]),
  ];
  // Every file is computed before any is written: a failure leaves the tree untouched.
  for (const [file, text] of writes) writeFileSync(resolve(ROOT, file), text);
  console.error(`release-version: set ${version} in ${writes.map(([f]) => f).join(", ")}`);
}

function gitTags() {
  return execFileSync("git", ["tag", "--list", "v*"], { cwd: ROOT, encoding: "utf8" }).split("\n").filter(Boolean);
}

function main(argv) {
  const [cmd, arg] = argv;
  switch (cmd) {
    case "current":
      console.log(currentVersion());
      break;
    case "next":
      console.log(nextVersion({ tags: gitTags(), current: currentVersion(), level: arg ?? "patch" }));
      break;
    case "set":
      setVersion((arg ?? "").replace(/^v/, ""));
      break;
    default:
      console.error("usage: release-version.mjs current | next <patch|minor|major> | set <X.Y.Z>");
      process.exit(2);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    main(process.argv.slice(2));
  } catch (err) {
    console.error(`release-version: ${err.message}`);
    process.exit(1);
  }
}
