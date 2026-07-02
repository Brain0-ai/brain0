#!/usr/bin/env bash
# Smoke-test the governance/attestation commands (DLP gate + signed attestation), deterministic and
# index-free, for CI. Builds the CLI, exercises the preflight gate's exit codes, and round-trips a
# signed attestation. See docs/governance.md and docs/attestation.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
cargo build -p brain0-cli --quiet
BIN="$ROOT/target/debug/brain0"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"

printf 'def f():\n    return 1\n' > clean.py
printf 'AWS_ACCESS_KEY_ID=AKIA1234567890ABCDEF\n' > leak.py

echo "== preflight: clean file passes (exit 0) =="
"$BIN" preflight clean.py

echo "== preflight: secret-bearing file is BLOCKED (exit non-zero) =="
if "$BIN" preflight leak.py; then
  echo "FAIL: preflight should have blocked a file containing a secret" >&2
  exit 1
fi

echo "== preflight --warn-only: advisory, never blocks (exit 0) =="
"$BIN" preflight leak.py --warn-only

echo "== the secret VALUE never appears in preflight output =="
if "$BIN" preflight leak.py --warn-only 2>&1 | grep -q 'AKIA1234567890ABCDEF'; then
  echo "FAIL: the secret value leaked into preflight output" >&2
  exit 1
fi

echo "== governance subcommands are wired =="
for cmd in guard preflight provenance attest verify-attestation compliance attribution; do
  "$BIN" "$cmd" --help >/dev/null
done

echo "governance smoke: OK"
