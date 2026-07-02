# brain0 — the black box for AI-written code

`git` tells you *what* changed. **brain0** tells you *why* — which prompt wrote it, what the
agent **read** to write it, and whether you can trust it.

```bash
npx brain0 up
```

One command, any repo: brain0 passively indexes your git history (the **facts**) and your
coding-agent sessions (the **intents** — Codex and Claude Code auto-discovered), then opens
an explorable decision graph at `http://localhost:8787`. Clicking a commit reveals the
prompts behind it, per-file diffs, what the agents read, and risk at a glance.

- **Drift** — the agent said "I only touched the parser". brain0 fact-checks it against git.
- **DLP reads** — which files (and which *secrets*) reached the model's context.
- **Risk that learns** — evidence-driven scoring; "looked safe, proved dangerous" gets flagged.
- **MCP why-layer** — `claude mcp add brain0 -- brain0 mcp` gives your agent long-term memory
  of why the code is the way it is.
- **Offline by default** — no hooks, no signup, no telemetry, zero egress without keys.

Daily loop:

```bash
brain0 today             # morning triage: what agents did, riskiest first
brain0 report            # the accountability report (add --md to share it)
brain0 rewind            # the agent wrecked your tree? restore any checkpoint
```

Requirements: Node ≥ 20. Local models via [Ollama](https://ollama.com) are optional and
make summaries/search better — never required.

Full documentation, GUI guide, configuration and troubleshooting:
**https://github.com/Brain0-ai/brain0**

Apache-2.0 · © The brain0 Authors
