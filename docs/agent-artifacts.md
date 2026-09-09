# Agent artifact formats (observed schema)

> Source for brain0's **passive** ingest. These formats are *internal to the
> agents and unstable*; this note records what was observed on a dev machine (one partial
> session per agent) so the adapters in `crates/brain0-agentsrc/` can be maintained as the
> formats drift. brain0 only ever **reads** these files.

## Codex (`~/.codex`)

Layout:

```
~/.codex/
  sessions/<YYYY>/<MM>/<DD>/rollout-<ISO-ts>-<uuid>.jsonl   # append-only JSONL, one session per file
  memories/                                                  # persistent memory files
  AGENTS.md                                                  # global instructions (memory)
```

Each JSONL line: `{ "timestamp": <ISO>, "type": <kind>, "payload": {...} }`.

Record kinds (`type`):

- `session_meta` — `payload { id, timestamp, cwd, originator, cli_version, source, model_provider }`.
  **`payload.cwd` = the session's working directory** (project scoping).
- `turn_context` — `payload { turn_id, cwd, model, ... }`. Per-turn cwd, model.
- `event_msg` — `payload.type` ∈ { `task_started` (has `turn_id`), `user_message`, `agent_message`,
  `token_count`, `exec_command_end`, `mcp_tool_call_end`, `task_complete`, `turn_aborted`, `error` }.
  Turn boundaries: `task_started` → `task_complete`.
- `response_item` — `payload.type` ∈:
  - `message` — `{ role: developer|user|assistant, content: [{ type: input_text|output_text, text }] }`.
  - `function_call` — `{ name, arguments: <JSON string>, call_id }`. Tool calls, e.g.
    `name: "exec_command"` (shell), `name: "apply_patch"` (edits; the patch text in `arguments`
    carries the touched file paths), plus MCP tools.
  - `function_call_output` — `{ call_id, output }`.
  - `reasoning` — model reasoning.

**Declared changes** derive from `function_call`: `apply_patch` (parse patch headers for paths) and
`exec_command` (shell that may touch files).

**Reads** derive from `exec_command` / `shell` / `local_shell`: `arguments.command` is a string or
an argv array (`["bash","-lc","<script>"]`), parsed best-effort for reader commands
(`crates/brain0-agentsrc/src/shell.rs`). The matching `function_call_output` (by `call_id`) carries
what the model saw: a plain string, or a JSON envelope `{"output": "...", "metadata": {...}}`. That
content is secret-scanned at ingest and never stored.

## Claude Code (`~/.claude/projects`)

Layout:

```
~/.claude/projects/<ENCODED_CWD>/
  <sessionId>.jsonl                      # append-only JSONL, one session per file (filename = session UUID)
  <sessionId>/subagents/agent-<id>.jsonl # one transcript per subagent (same sessionId + cwd, isSidechain: true)
  <sessionId>/subagents/agent-<id>.meta.json
  <sessionId>/tool-results/<id>.txt      # tool results too large to inline (the JSONL keeps a 2 KB preview)
  memory/                                # per-project persistent memory
```

`ENCODED_CWD` = the project's absolute path with `/` replaced by `-`
(e.g. `/home/nicola/progetti/brain0` → `-home-nicola-progetti-brain0`).

Each JSONL line is a record with top-level keys including:
`type, cwd, gitBranch, sessionId, uuid, parentUuid, timestamp, message, toolUseResult, version`.

- **`cwd` (top-level) = the working directory** on every record (project scoping); the directory
  name also encodes it.
- `type: user` — `message { role: user, content: <string> | [{type:text|tool_result, ...}] }`.
  A user record with textual content is a **prompt** (turn start).
- `type: assistant` — `message { role: assistant, content: [ {type:text, text} | {type:tool_use, id, name, input} ] }`.
- Other `type`s (`ai-title`, `attachment`, `queue-operation`, `last-prompt`, `file-history-snapshot`)
  are ignored.

**Declared changes** derive from `tool_use`: `name ∈ {Edit, Write, MultiEdit, NotebookEdit}` →
`input.file_path`; `name == "Bash"` → `input.command`.

**Reads** derive from `tool_use`: `Read` → `input.file_path`; `NotebookRead` →
`input.notebook_path`; `Grep` with `input.output_mode == "content"` → `input.path` (the repo root
when absent); `Bash` → `input.command` parsed best-effort for reader commands
(`crates/brain0-agentsrc/src/shell.rs`). The content the model saw comes from the matching
`tool_result` (by `tool_use_id`) in the next `user` record, plus that record's top-level
`toolUseResult` (`stdout`/`stderr` for Bash, `file.content` for Read), which stays complete even
when the inline content is a `<persisted-output>` preview pointing at `tool-results/<id>.txt`
(loaded as a fallback, up to 16 MB). Scanned at ingest, never stored.

Subagent transcripts under `<sessionId>/subagents/` are discovered like sessions (walk depth 4);
they carry the parent `sessionId`, so their turns merge into the parent task.

## Notes for adapters

- **Turn** = a user prompt and everything (assistant text + tool calls) until the next prompt.
- **Cursor**: both formats are append-only JSONL → a byte offset per file is a valid resume cursor.
- **Provenance**: keep (adapter, file path, byte offset) on every normalized event.
- **Memory**: Codex → `~/.codex/memories/*` + `AGENTS.md`; Claude Code → `projects/<enc>/memory/*`
  + the repo's `CLAUDE.md`.
