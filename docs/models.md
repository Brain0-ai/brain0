# brain0 — Local models (summarizer + embedder)

Both models run **locally by default** (Ollama), so ingest has **zero egress**. The powerful LLM is used only
at query time, by reference. Both operate **only on redacted text**.

## Defaults

| Role | Default | Profile (the binding requirement) |
|---|---|---|
| Summarizer (`decision_summary`) | `qwen3:4b` (Q4) via Ollama | instruct, non-multimodal, ~2–8B, code-aware, Apache-2.0/MIT |
| Embedder (semantic search) | `qwen3-embedding:0.6b` via Ollama, dim **1024** | local embedding, multilingual + code-aware, Apache-2.0/MIT |
| Embedder low-end fallback | `nomic-embed-text` (v1.5) | runs anywhere |
| Offline fallback (no Ollama) | deterministic feature-hash embedder (dim 256) + deterministic summaries | zero dependency |

> **Pin the profile, not the tag.** The defaults are concrete tags, but the requirement is the
> *profile*: prefer the latest small version that runs natively in the chosen runtime. Changing
> the tag is a config edit, not a code change.

## Configuration (override)

Defaults live in `ModelConfig` (the config layer), overridable by an optional JSON config file
and by environment variables — no recompile, nothing hardcoded in logic:

| Env var | Meaning |
|---|---|
| `BRAIN0_SUMMARIZER_PROVIDER` | `ollama` (default) or `deterministic` |
| `BRAIN0_SUMMARIZER_MODEL` | e.g. `qwen3:4b`, `qwen3:8b` |
| `BRAIN0_SUMMARIZER_ENDPOINT` | runtime endpoint (default `http://localhost:11434`) |
| `BRAIN0_EMBED_PROVIDER` | `ollama` (default) or `local` |
| `BRAIN0_EMBED_MODEL` | e.g. `qwen3-embedding:0.6b`, `nomic-embed-text` |
| `BRAIN0_EMBED_ENDPOINT` | runtime endpoint |
| `BRAIN0_EMBED_DIM` | output dimension (must match the model / store) |

The endpoint can point at any OpenAI-compatible / llama.cpp / LM Studio runtime.

## Runtime behavior (what actually happens)

- **Embedding endpoint**: brain0 calls Ollama's modern `/api/embed` first and falls back to the
  legacy `/api/embeddings` automatically — newer embedding models only serve the former.
- **Embedder fallback chain**: if the configured model fails, brain0 prints the real reason plus
  the fix (`ollama pull <model>`), then tries `nomic-embed-text` (dim 768) before degrading to
  the offline feature-hash embedder. Switching dimensions on an existing store requires
  `brain0 reembed` (the error message says so).
- **Lazy summarizer probe**: the summarizer model is loaded only at the *first uncached turn* —
  a run fully served by the summary cache never touches the model server.
- **Persistent summary cache**: summaries are content-keyed and cached in
  `~/.cache/brain0/summary-cache.db` (override with `BRAIN0_SUMMARY_CACHE=path`, disable with
  `off`). Full index rebuilds — and even interrupted runs — never re-pay model calls for turns
  already summarized, on any repo.
- **Cost controls**: turns with no declared changes and under 240 chars get a deterministic
  summary (no model call); model input is capped at the first 2000 chars of a turn (prefill
  dominates local-GPU latency; the cache key still hashes the full text).
- **Small GPUs (≈4 GB)**: populate the cache with a lighter model first —
  `BRAIN0_SUMMARIZER_MODEL=qwen3:1.7b` — then switch back; cached summaries stay valid.

## Vector dimension & store

- **Fixed dimension per store.** All vectors in a store share one dimension; mixing is rejected.
- The store persists `embedding_model` + `embedding_dim` in its `meta` table; `put_task_embedding`
  refuses a vector whose length disagrees (mismatch detection).
- **Local ↔ remote coherence:** use the same model/dimension on both backends.

### Matryoshka trade-off

`qwen3-embedding` supports adjustable output dimensions. Reducing the dimension (e.g. 1024 → 512
or 256) shrinks storage and speeds search at some precision cost. Declare the reduced dimension
in `BRAIN0_EMBED_DIM`; it is persisted and enforced per store. Choose based on corpus size and
recall needs.

## Changing models (migration)

- **Summarizer:** non-destructive. Cached summaries (content-hashed) stay valid; recompute is
  optional/selective. Changing `BRAIN0_SUMMARIZER_MODEL` does not invalidate anything.
- **Embedder / dimension:** destructive for vectors. Run the migration:
  ```bash
  BRAIN0_EMBED_MODEL=<new> BRAIN0_EMBED_DIM=<dim> brain0 reembed
  ```
  `reembed` clears the old vectors, records the new `(model, dim)`, and re-embeds the whole
  corpus from the already-redacted payload, then reports a consistency summary. The same
  invalidation pipeline is shared with purge/crypto-shred.

## Fail-safe

If the model runtime is unavailable, brain0 never blocks: the summarizer falls back to a
deterministic summary and the embedder falls back to the local feature-hash embedder (with a
warning). Tasks are still ingested and can be re-embedded later.
