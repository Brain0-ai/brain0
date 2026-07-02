#!/usr/bin/env bash
# End-to-end check across the Rust observer and the TypeScript agent:
#   git repo → `brain0 ingest` (Rust) → SQLite index → indexer + agent.debug (TS).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> building brain0 CLI and TS packages"
cargo build -q -p brain0-cli
( cd "$ROOT" && pnpm -r --filter '@brain0/shared' --filter '@brain0/agent' --filter '@brain0/server' run build >/dev/null )

echo "==> creating a sample git repo"
WORK="$TMP/work"; mkdir -p "$WORK"; cd "$WORK"
git init -q
git config user.name "CI Bot"
git config user.email "ci@example.com"
printf 'def add(a, b):\n    return a + b\n' > calc.py
git add -A && git commit -qm "add calc"
printf 'def add(a, b):\n    if a is None:\n        return b\n    return a + b\n' > calc.py
git add -A && git commit -qm "guard None in add"
cd "$ROOT"

echo "==> ingesting with the Rust observer (read-only)"
# The TypeScript reader hydrates the *unencrypted* payload store; encrypted-at-rest payload
# is consumed via the Rust query path. So this cross-language demo uses --no-encrypt-payload.
"$ROOT/target/debug/brain0" ingest \
  --repo demo/calc --path "$WORK" \
  --db "$TMP/index.db" --payload "$TMP/payload" --no-encrypt-payload

echo "==> running the TypeScript agent against the produced index"
node --experimental-sqlite "$ROOT/packages/server/dist/e2e.js" \
  "$TMP/index.db" "$TMP/payload" demo/calc "add returns the wrong value when a is None"

echo "==> E2E passed"
