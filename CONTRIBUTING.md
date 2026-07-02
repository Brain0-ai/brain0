# Contributing to brain0

Thanks for your interest in brain0! This document explains how to build, test, and
contribute changes.

## Ground rules

- **Be passive about repos.** brain0 observes; it never writes to a user's git repo.
  Any code that touches an observed repo must be read-only.
- **The graph is append-only.** Never mutate or delete existing versions; add new ones.
- **One schema, open core.** This repo ships the local **SQLite** backend (`schema/sqlite.sql`).
  The Postgres backend and other premium/team capabilities live in the separate, private
  `brain0-enterprise` repo and implement the public `brain0_storage::Storage` trait. **Never add
  premium code or any license-check logic to this repo**. See
  [docs/open-core.md](./docs/open-core.md).
- **Test the first-class features.** Symbol identity, rename/move tracking, drift
  reconciliation, and the two risk scores must keep their explicit test coverage.

## Development setup

Prerequisites: Rust (via `rust-toolchain.toml`), Node 20+, pnpm.

```bash
# Rust
cargo build
cargo test
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings

# TypeScript
pnpm install
pnpm -r build
pnpm -r test
pnpm -r lint
```

## Workflow

1. Fork and branch from `main`.
2. Keep changes focused; write tests for new behavior.
3. Ensure `cargo fmt`, `cargo clippy`, and all tests pass; same for the TS workspace.
4. Open a PR with a clear description of *intent* (brain0 is, after all, about intent).
5. **Sign the CLA** (see below) — the CLA check is a required gate on every PR.

## Contributor License Agreement (CLA)

brain0 is **open core**: the Apache-2.0 code here flows into the dual-licensed
`brain0-enterprise` product, so we need the right to re-license contributions. Before your first
PR can be merged you must sign our CLA (an Apache-based ICLA, or be covered by a corporate CCLA).
You keep your copyright — this is a license, not an assignment.

The [CLA Assistant](https://github.com/contributor-assistant/github-action) bot comments on your
PR with a one-line signing instruction and records your signature in an auditable registry. See
[`cla/`](./cla/) for the full text and process. Founders and project bots are
exempt. (`brain0-enterprise` requires no CLA — it has no external contributors.)

## Commit messages

Use clear, imperative subject lines (e.g. "Add Jaccard rename matcher").

## Code style

- **Rust**: idiomatic, `#![forbid(unsafe_code)]` where practical, no `unwrap()`/`expect()`
  in library code paths that can fail at runtime — return typed errors.
- **TypeScript**: strict mode, no `any` without justification, explicit module boundaries.

## License

This repository is licensed under [Apache-2.0](./LICENSE). By contributing you agree your
contributions are provided under Apache-2.0 **and** under the terms of the [CLA](./cla/), which
additionally grants the right to sublicense them within the commercial `brain0-enterprise`
product.
