<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/brain0-wordmark-dark.svg">
  <img alt="brain0" src="docs/assets/brain0-wordmark-light.svg" width="380">
</picture>

<br/>

**The black box for AI-written code.**

`git` tells you *what* changed. brain0 tells you *why*: which prompt wrote it,
what the agent **read** to write it, and whether you can trust it.

[![CI](https://github.com/Brain0-ai/brain0/actions/workflows/ci.yml/badge.svg)](https://github.com/Brain0-ai/brain0/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](./LICENSE)
[![npm](https://img.shields.io/npm/v/brain0?color=cb3837&logo=npm)](https://www.npmjs.com/package/brain0)
[![Rust](https://img.shields.io/badge/core-Rust-e43717?logo=rust)](./crates)
[![TypeScript](https://img.shields.io/badge/gui-TypeScript-3178c6?logo=typescript&logoColor=white)](./packages)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-27a644.svg)](./CONTRIBUTING.md)

[Quickstart](#quickstart) · [What it answers](#the-questions-your-repo-cant-answer-today) ·
[Give your agent memory](#give-your-agent-the-why-layer-mcp) · [How it works](#how-it-works) ·
[Comparison](#how-it-compares) · [Docs](./docs)

</div>

---

Coding agents now write most of the diff: continuously, in parallel, and opaquely.
brain0 **passively** builds a decision graph of your repository: every **commit** linked to
the **agent intents** behind it, down to the single **function**, with dated history, drift
detection, a DLP audit of what agents *read*, and a two-dimensional **risk score** rendered
green → red. No hooks, no agent cooperation, no code changes: it reads git and the
transcripts your agents already write to disk.

> Dogfooded from day one: brain0's own development is tracked by brain0.

## Quickstart

```bash
npx brain0 up
```

That's it. From any repo, `up` infers the repo id from your git remote, indexes the git
history (the **facts**), passively ingests your coding-agent sessions (the **intents**,
with Codex and Claude Code auto-discovered), and opens the GUI at `http://localhost:8787`:
an explorable graph of your codebase, from repo to module, file and symbol, where clicking a
commit reveals the prompts behind it, per-file diffs, and risk at a glance.

Then make it a habit:

```bash
brain0 today             # morning triage: what agents did, riskiest first
brain0 report            # the accountability report (add --md to share it)
brain0 query "why did the parser break"   # root-cause debug over the graph
```

## What you need

The only hard requirement is **Node.js ≥ 20**. brain0 is offline-first: with nothing else
installed it still works end to end: deterministic summaries, local feature-hash embeddings,
zero egress. Local models make it *better*, never *required*.

| Piece | Needed for | Without it |
|---|---|---|
| **Node.js ≥ 20** | everything (`npx brain0 up`) | (required) |
| **git** | commit history (the facts side) | filesystem checkpoint mode (`brain0 watch`) |
| **A coding agent**: Codex (`~/.codex`) or Claude Code (`~/.claude/projects`), auto-discovered | the *why* layer: prompts, drift, reads/DLP | graph of commits + code only |
| **[Ollama](https://ollama.com)** + models (below) | model-written summaries · semantic search · GUI smart chat | deterministic summaries · feature-hash embeddings · retrieval-only answers |
| `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` (opt-in) | a hosted LLM for the GUI smart chat | local Ollama (default) |

### Recommended local models

```bash
ollama pull qwen3:4b               # summarizer (default)
ollama pull qwen3-embedding:0.6b   # embeddings (default; nomic-embed-text is the auto-fallback)
```

Both run on modest hardware. On a small GPU (≈4 GB VRAM) use a lighter summarizer for the
first, cache-populating run. Summaries are content-cached in `~/.cache/brain0/`, so every
later rebuild costs zero model calls (and even an interrupted run keeps its work):

```bash
BRAIN0_SUMMARIZER_MODEL=qwen3:1.7b npx brain0 up
```

If a configured model is missing, brain0 tells you exactly what to pull and degrades
gracefully (embedder falls back to `nomic-embed-text`, then local). Every choice is
overridable: `BRAIN0_SUMMARIZER_{PROVIDER,MODEL,ENDPOINT}`, `BRAIN0_EMBED_{PROVIDER,MODEL,ENDPOINT,DIM}`,
`BRAIN0_LLM_PROVIDER`. Details in [`docs/models.md`](./docs/models.md).

## The questions your repo can't answer today

- *Which prompt introduced this bug?*
- *Who (agent or human) touched this function, when, and **why**?*
- *The agent claimed it "changed little." **Did it?***
- *Did any session read `.env` or a private key before pushing code?*
- *This change looked harmless. Did it later prove dangerous?*

brain0 answers all five, by construction. Three of those answers exist nowhere else:

### ① Drift: declared vs. done
Agents narrate what they changed; git records what actually changed. brain0 reconciles the
two and scores the gap. From brain0's own report:

```
drift — declared vs done (41)
  · [1.00] claude-code — changed but not declared: crates/brain0-cli/src/main.rs,
           packages/gui/src/main.ts … +12 more
```

### ② Sensitive reads: DLP for agent context
brain0 records which files each session **read** (paths and secret *kinds* only, never
values), so you know what reached a possibly-remote model:

```
sensitive reads — DLP (7)
  · claude-code read crates/brain0-agentsrc/src/driver.rs [env_secret]
  · 8 read(s) outside the repo reached the model's context
```

### ③ Risk that learns: green → red
Every artifact carries an **a-priori** score (blast radius, churn, drift, diff size) fused
with an **a-posteriori** score fed by evidence (reverts, immediate fixes). A change that
*looked safe but later proved dangerous* is flagged as a **gold signal**: the pattern
worth studying.

## Give your agent the "why layer" (MCP)

The same graph is a query channel your coding agent can call **before touching code**:
provenance-aware context, by reference, nothing heavy pushed through MCP.

```bash
claude mcp add brain0 -- brain0 mcp
```

| Tool | What the agent learns |
|---|---|
| `brain0_context` | a file/symbol's risk, recent history, and the intents (agent · model · drift) behind it |
| `brain0_blame` | *which intent wrote this line*: `file:line` resolves to its symbol via live parsing |
| `brain0_debug` | root-cause candidates for an issue, recency- and risk-aware |
| `brain0_audit` | repo risk distribution, gold signals, riskiest nodes |

Your agent stops re-breaking what it can't remember. Uninstalling brain0 makes it
forget again.

## How it works

brain0 is a **pure observer**: it never writes to your repository.

```
            ┌───────────────────────── GUI (PixiJS / TS) ─────────────────────────┐
            │   bipartite graph · LOD lens · risk color · timeline · search bar    │
            └───────────────▲─────────────────────────────────────▲───────────────┘
                            │ references / highlights              │ on-demand
            ┌───────────────┴────────────┐                         │ hydration
            │  Internal agent (TS)        │                         │
            │  RAG-on-graph · your LLM    │                         │
            └───────────────▲────────────┘                         │
                            │ index queries                        │
   ┌────────────────────────┴─────────────────────────────────────┴──────────────┐
   │             Abstract storage (one `Storage` trait, pluggable backends)        │
   │  light index + embeddings   |   heavy payload (dedicated store)               │
   │  SQLite + sqlite-vec (local, open core)  |  Postgres + pgvector (enterprise)  │
   └────────────────────────▲──────────────────────────────────────────────────────┘
                            │ append-only (client-server)
   ┌────────────────────────┴─────────────────────────────────────────────────────┐
   │                          Core observer (Rust)                                 │
   │  git reader  +  fs-watcher / checkpoint engine (no-git fallback)  [FACT]       │
   │  passive transcript + memory ingest (Codex, Claude Code, …)        [DECLARED]  │
   │  Tree-sitter (symbol extraction + AST-fingerprint identity)                   │
   │  declared↔done reconciliation (gap-filling + drift)                           │
   │  a-priori risk  ·  a-posteriori risk hooks (event-driven)                     │
   └────────────────────────▲─────────────────────────────────────────────────────┘
                            │ passive read-only (no agent cooperation)
                  ┌──────────┴───────────┐
                  │   Coding agents      │  write transcripts + memory to disk;
                  │  (e.g. Codex, Claude)│  brain0 reads them, like it reads git
                  └──────────────────────┘
```

Principles that don't bend:

1. **Passive observation**: never modifies your repo, never commits/checks out, never interferes with git.
2. **The magnifying glass**: zooming descends into the *same* object (repo → module → file → symbol), never loads different nodes.
3. **No unjustified new nodes**: an entity that evolves stays the same node (deterministic, cross-machine symbol identity with rename/move tracking), with a dated chain of versions.
4. **Light index, lazy heavy payload**: the navigable graph is small and lasts for years; diffs, messages and summaries hydrate on demand.
5. **Open source and composable**: bring your own database and your own LLM keys, or run fully offline.

## How it compares

| | `git blame` | Vendor dashboards¹ | Git AI | **brain0** |
|---|:---:|:---:|:---:|:---:|
| Who committed a line | ✅ | ✗ | ✅ | ✅ |
| Which **prompt/intent** produced a change | ✗ | ✗ | ✅ | ✅ |
| Cross-agent, vendor-neutral | ✅ | ✗ | ✅ | ✅ |
| **Drift**: declared vs. actually done | ✗ | ✗ | ✗ | ✅ |
| What the agent **read** (incl. secrets, DLP) | ✗ | ✗ | ✗ | ✅ |
| **Risk score** per file/symbol, evidence-driven | ✗ | ✗ | ✗ | ✅ |
| Explorable **graph GUI** with time travel | ✗ | ✗ | ✗ | ✅ |
| Zero integration (no hooks in your agents) | ✅ | ✗ | ✗ | ✅ |
| Local-first, offline by default | ✅ | ✗ | ✅ | ✅ |

¹ Copilot Metrics API, Cursor team analytics, Claude Code analytics: per-vendor aggregates
(sessions, LoC accepted), no line/symbol-level provenance, no causality.

## Commands

| Command | What it does |
|---|---|
| `brain0 up` | index + observe + serve the GUI, one shot |
| `brain0 today` | last-24h triage, attention first (`--since 7d`) |
| `brain0 report` | drift · sensitive reads · top risk · agent footprint (`--md`) |
| `brain0 query "<question>"` | root-cause debug over the graph, by reference |
| `brain0 mcp` | serve the query channel (context/blame/debug/audit) over stdio |
| `brain0 ingest` / `observe` / `watch` | the underlying fact/intent observers |
| `brain0 rewind` | restore the working tree from a recorded checkpoint (the `watch` safety net) |
| `brain0 guard` / `preflight` | DLP: flag secret reads that reached a remote model · block them pre-commit/pre-run |
| `brain0 provenance` / `attribution` / `attest` / `compliance` | AI provenance per commit · per-hunk attribution · signed (Ed25519, in-toto) attestations · auditor pack |
| `brain0 verify` / `audit` / `purge` / `reembed` | integrity, audit log, crypto-shred, re-embedding |

## Building from source

Prerequisites: **Rust** (see `rust-toolchain.toml`), **Node 20+**, **pnpm**.

```bash
cargo build && cargo test          # Rust core (14 crates)
pnpm install && pnpm -r build && pnpm -r test   # TS workspace (shared · agent · gui · server)
./scripts/e2e.sh                   # cross-language end-to-end check

# All-in-one dev loop (build → ingest → observe → server :8787 → GUI :5173)
pnpm dev --repo myorg/myrepo
```

The GUI's **Refresh** button re-runs the same passive observer and re-embeds, live; it is
served same-origin on `127.0.0.1` only.

```
crates/      Rust core (model, parser, identity, storage, observer, reconcile, risk,
             agentsrc, crypto, models, policy, attest, mcp, cli)
packages/    TypeScript (shared, agent, gui, server, cli)
schema/      SQL DDL for the local SQLite backend
docs/        Documentation (agent-artifact schemas, security, models, open-core, governance)
```

## Privacy & security, by default

- **Offline by default**: no keys ⇒ local embeddings + local models (Ollama optional);
  nothing leaves the machine. Remote LLM/embeddings are an explicit opt-in, with context
  redacted before egress.
- **Secret scanning at ingest**: transcripts are scanned and redacted (typed placeholders,
  never values) *before* anything is stored, summarized, or embedded; `BRAIN0_EXCLUDE` /
  `BRAIN0_REDACT` add your own rules. The raw prompt text is **not persisted**; only the
  model-generated summary.
- **Encrypted at rest**: envelope-encrypted payload store (ChaCha20-Poly1305, KEK rotation,
  crypto-shred purge) with restrictive file permissions.
- **Append-only, content-addressed**: the graph is tamper-evident (`brain0 verify`), with
  an append-only security audit log.

Threat model and honest limitations: [`docs/security.md`](./docs/security.md).

## Open core

Everything in this repository is free and open source under [Apache-2.0](./LICENSE) and
fully functional on its own, with **no license checks, feature flags, or crippled code paths**.
Team/hosted capabilities (a PostgreSQL + pgvector backend implementing the same public
`brain0_storage::Storage` trait, signed entitlements, a multi-tenant server) live in a
separate **brain0-enterprise** repository (AGPLv3 + commercial). brain0 never depends on
brain0-enterprise. Full boundary and rationale: [`docs/open-core.md`](./docs/open-core.md).

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). By participating you agree to the
[Code of Conduct](./CODE_OF_CONDUCT.md). Contributions are accepted under our CLA
(see [`cla/`](./cla/)), which keeps the open-core dual-licensing sustainable.

## License

[Apache-2.0](./LICENSE) © The brain0 Authors.
