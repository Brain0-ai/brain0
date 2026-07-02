# Security Policy

brain0 handles sensitive material by design — agent transcripts, prompts summaries, code
history — so security reports get priority treatment.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Use GitHub's
[private vulnerability reporting](../../security/advisories/new) on this repository.
Include reproduction steps and the affected component (crate/package). You will get an
acknowledgment within 72 hours.

## Scope

- The Rust core (`crates/`): observer, storage, crypto, redaction/secret-scanning, CLI.
- The TypeScript workspace (`packages/`): server, GUI, agent, npm CLI.
- The release pipeline (`.github/workflows/`, `scripts/release-pack.mjs`).

Out of scope: the private `brain0-enterprise` repository (report through its own channel),
and vulnerabilities in third-party coding agents whose transcripts brain0 reads.

## What we consider a vulnerability

Anything that breaks brain0's documented guarantees, e.g.:

- secret **values** persisted anywhere (index, payload, logs, audit) — only typed
  placeholders and secret *kinds* are ever allowed;
- payload encryption bypass or key material leakage;
- the observer writing to the observed repository;
- path traversal / RCE through the local server or the MCP channel;
- egress that is not the explicitly configured LLM/embeddings endpoint.

Threat model and known limitations: [`docs/security.md`](./docs/security.md).
