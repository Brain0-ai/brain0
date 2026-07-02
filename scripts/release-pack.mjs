#!/usr/bin/env node
// Assemble the publishable npm artifacts for a release (used by CI and locally):
//
//   node scripts/release-pack.mjs [--binary <platform-arch>=<path> …] [--out dist-npm]
//
// 1. Bundles @brain0/server (with its workspace deps) into packages/cli/vendor/server/server.js
//    — a single ESM file, so the published `brain0` package needs no workspace resolution.
// 2. Copies the built GUI (packages/gui/build) into packages/cli/vendor/gui.
// 3. Generates one platform package per provided binary (@brain0/cli-<platform>-<arch>,
//    os/cpu-scoped, containing just the Rust binary).
// 4. Packs everything into <out>/ as npm tarballs, injecting the platform packages as
//    optionalDependencies of `brain0` AT PACK TIME (the committed package.json stays clean, so
//    a plain `pnpm install` in the workspace never tries to fetch unpublished packages).
//
// Prerequisites: `pnpm -r build` (TS dists + GUI build) and the Rust release binaries.

import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, rmSync, writeFileSync, readFileSync, chmodSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const CLI = resolve(ROOT, "packages/cli");

// ── args: --binary linux-x64=/path/to/brain0 (repeatable) ───────────────────
const argv = process.argv.slice(2);
const binaries = new Map();
let out = resolve(ROOT, "dist-npm");
for (let i = 0; i < argv.length; i++) {
  if (argv[i] === "--binary" && argv[i + 1]) {
    const [key, ...pathParts] = argv[++i].split("=");
    const path = pathParts.join("=");
    if (!/^[a-z]+-[a-z0-9]+$/.test(key) || !existsSync(path)) {
      console.error(`release-pack: bad --binary "${key}=${path}" (want <platform-arch>=<existing path>)`);
      process.exit(1);
    }
    binaries.set(key, resolve(path));
  } else if (argv[i] === "--out" && argv[i + 1]) {
    out = resolve(argv[++i]);
  }
}

const pkg = JSON.parse(readFileSync(resolve(CLI, "package.json"), "utf8"));
const version = pkg.version;
const sh = (cmd, args, cwd = ROOT) => execFileSync(cmd, args, { cwd, stdio: "inherit" });

// ── 1. bundle the server (single ESM file; node: builtins stay external) ────
const serverDist = resolve(ROOT, "packages/server/dist/server.js");
if (!existsSync(serverDist)) {
  console.error("release-pack: packages/server/dist/server.js missing — run `pnpm -r build` first");
  process.exit(1);
}
rmSync(resolve(CLI, "vendor"), { recursive: true, force: true });
mkdirSync(resolve(CLI, "vendor/server"), { recursive: true });
sh(resolve(CLI, "node_modules/.bin/esbuild"), [
  serverDist,
  "--bundle",
  "--platform=node",
  "--format=esm",
  "--target=node20",
  `--outfile=${resolve(CLI, "vendor/server/server.js")}`,
]);

// ── 2. bundle the GUI build ──────────────────────────────────────────────────
const guiBuild = resolve(ROOT, "packages/gui/build");
if (!existsSync(resolve(guiBuild, "index.html"))) {
  console.error("release-pack: packages/gui/build missing — run `pnpm --filter @brain0/gui build`");
  process.exit(1);
}
cpSync(guiBuild, resolve(CLI, "vendor/gui"), { recursive: true });

// ── 3. platform packages ─────────────────────────────────────────────────────
const OS_FOR = { linux: "linux", darwin: "darwin", win32: "win32" };
mkdirSync(out, { recursive: true });
const optionalDeps = {};
for (const [key, binPath] of binaries) {
  const [platform, arch] = key.split("-");
  if (!OS_FOR[platform]) {
    console.error(`release-pack: unknown platform "${platform}"`);
    process.exit(1);
  }
  const name = `@brain0/cli-${key}`;
  const dir = resolve(out, `pkg-${key}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const exe = platform === "win32" ? "brain0.exe" : "brain0";
  cpSync(binPath, resolve(dir, exe));
  chmodSync(resolve(dir, exe), 0o755);
  writeFileSync(
    resolve(dir, "package.json"),
    JSON.stringify(
      {
        name,
        version,
        description: `brain0 core binary (${key})`,
        license: "Apache-2.0",
        // npm provenance validates this against the publishing workflow's repository.
        repository: { type: "git", url: "git+https://github.com/Brain0-ai/brain0.git" },
        os: [OS_FOR[platform]],
        cpu: [arch],
        files: [exe],
      },
      null,
      2,
    ),
  );
  sh("npm", ["pack", "--pack-destination", out], dir);
  optionalDeps[name] = version;
  console.log(`release-pack: packed ${name}@${version}`);
}

// ── 4. pack `brain0` with optionalDependencies injected, then restore ───────
const cliPkgPath = resolve(CLI, "package.json");
const original = readFileSync(cliPkgPath, "utf8");
try {
  const patched = JSON.parse(original);
  if (Object.keys(optionalDeps).length) patched.optionalDependencies = optionalDeps;
  delete patched.devDependencies; // esbuild is a release-time tool, not a runtime dep
  writeFileSync(cliPkgPath, JSON.stringify(patched, null, 2));
  sh("npm", ["pack", "--pack-destination", out], CLI);
  console.log(`release-pack: packed brain0@${version} → ${out}`);
} finally {
  writeFileSync(cliPkgPath, original);
}
