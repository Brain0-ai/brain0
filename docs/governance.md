# brain0 — DLP / egress governance

When you let a cloud coding agent (Claude Code, Codex, …) read your repo, **every file it reads is
sent to a remote model.** brain0 already records those reads passively; this feature turns that
record into a data-loss-prevention control: *which sensitive files, and which secrets, reached which
model — and a way to stop it before it happens.*

It is built on three primitives, in increasing strength:

| Command | Kind | When it runs | Needs an index? |
|---|---|---|---|
| `brain0 guard` | **Detective** | after the fact, over the index | yes |
| `brain0 guard --watch` | **Detective (live)** | continuously, as the index updates | yes |
| `brain0 preflight` | **Preventive** | before an agent run / before a commit | no |

The privacy rule is absolute: brain0 stores and reports **file paths and secret KINDS**
(e.g. `aws_access_key`) — **never the secret value**. This holds across every command and sink.

## What counts as a violation

The policy ([`brain0-policy`](../crates/brain0-policy)) evaluates each read and emits:

- **critical — `secret-in-read`**: the file's content contained a secret (detected by the same
  scanner used for redaction) and it reached a remote model.
- **warn — `sensitive-path`**: the path matched a sensitive glob (`.env`, `*.pem`, `*.key`,
  `credentials`, `.aws/…`, `.ssh/…`, `secrets…`).
- **warn — `external-read`**: the agent read a file outside the observed repo (absolute path).

Configure the sensitive globs with `BRAIN0_DLP_GLOBS` (comma-separated; added to the defaults).

## `brain0 guard` — detective audit

Evaluate every recorded agent read in the index and report what reached a remote model:

```bash
brain0 guard --db .brain0/index.db
#   [critical] secret-in-read  config.py  → claude-opus-4-8  — secret [aws_access_key] …
# egress guard: 3 violation(s), 1 critical
```

Every finding is also written to the append-only `audit_log` (channel `dlp`). For CI, fail the build
on any critical:

```bash
brain0 guard --strict      # exit 1 if any critical violation exists
```

"Which model received the secret" comes from the model id captured per session (Claude
`message.model`); unknown sessions show `unknown-model`.

## `brain0 guard --watch` — live daemon + alerts

Run guard as a sidecar that polls the index (kept fresh by `brain0 dev` / `brain0 observe` / the MCP
ingest) and streams **new** violations as they appear:

```bash
brain0 guard --watch --interval 5 --min-severity critical
# default sink is stderr:
#   ALERT [critical] secret-in-read config.py → claude-opus-4-8 — secret [aws_access_key] …
```

Findings are deduplicated across ticks by `(task, path, rule, model)`, so each violation alerts once.

### Alert sinks

| Flag | Sink | Payload |
|---|---|---|
| *(none)* | stderr | `ALERT [severity] rule path → model — detail` |
| `--webhook <url>` | generic HTTP POST | JSON `{severity, rule, path, model, detail, task}` |
| `--slack <url>` | Slack incoming webhook | `{ "text": "brain0 DLP …" }` |

`--min-severity info|warn|critical` sets the floor (default `critical`). `--once` runs a single poll
and exits (for CI / testing). The secret value is never included in any sink payload.

## `brain0 preflight` — prevention gate

The guard is downstream of egress; `preflight` runs **before** it. Point it at the files an agent is
about to read, or at the git staging area, and it blocks (non-zero exit) on a critical:

```bash
# check explicit files (e.g. an agent harness pre-run hook)
brain0 preflight src/config.py src/main.py

# git pre-commit hook — block committing a secret
brain0 preflight --staged

# advisory mode: report but never block
brain0 preflight --staged --warn-only
```

It needs no index and no payload store — it reads the files directly, scans content for secrets,
checks paths against the sensitive globs, and exits non-zero on a critical (unless `--warn-only`).

Wire it as a git hook:

```bash
# .git/hooks/pre-commit
#!/usr/bin/env bash
exec brain0 preflight --staged
```

## CI integration

`scripts/governance-smoke.sh` (run by the `governance` CI job) exercises the gate end-to-end. In
your own pipeline, the typical gates are:

```bash
brain0 preflight --staged          # block secrets entering a commit (no index needed)
brain0 guard --strict              # fail if the index shows a secret already reached a model
brain0 compliance --strict         # release/PR gate over the whole history (see attestation.md)
```

## Honest limits

- The guard is **detective** — brain0 observes the agent's egress, it does not sit in the data path.
  `preflight` is the realistic prevention primitive; a true inline LLM proxy is a separate, larger
  initiative (not required for this feature's value).
- `--watch` polls the index rather than tailing transcripts directly; a native fs-tail is a future
  refinement. Pair it with `brain0 dev`, which keeps the index fresh.
