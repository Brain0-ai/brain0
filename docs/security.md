# brain0 — Threat model & security controls

This documents brain0's security posture. brain0 ingests raw agent
transcripts, which contain *everything* (secrets, keys, PII), so security is not optional.

## Data tiers

| Tier | Content | Treatment |
|---|---|---|
| **T0** | Detected secrets (keys, tokens, private keys, `.env`) | Never persisted in clear — redacted at ingest. |
| **T1** | Payload (decision summaries, commit messages, diffs) | Encryptable at rest; purgeable; never logged. **Raw prompts are not persisted at all** — only the model-generated summary of each turn survives ingest. |
| **T2** | Embeddings (invertible!) | Same protection as payload; computed only on redacted text. |
| **T3** | Index (metadata, refs, risk) | Protected, not public; no raw content. |

**Iron rule:** embeddings and `decision_summary` are computed **only on redacted text**.
The driver redacts each turn before payload write, summary, or embedding.

## Deployments & threat models

- **Local (SQLite + payload dir).** Single user. Threats: device theft, cloud-synced
  backups, file permissions, secrets at rest, plaintext credentials. Controls: secret
  redaction (default-on), envelope-encrypted payload (default-on), `0700/0600` permissions,
  optional SQLCipher DB encryption, content-addressed integrity.
- **Remote/team (Postgres + pgvector + payload store).** N clients. Adds: interception,
  authn/authz, isolation, server-side append-only, residency. Controls: TLS `verify-full`
  (plaintext refused), per-user least-privilege roles, Row-Level Security scoping, append-only
  triggers, client-side payload encryption (server never sees payload plaintext). This backend
  and its server-side controls ship in the **brain0-enterprise** repo; it
  implements the same public `brain0_storage::Storage` trait and reuses the open-core redaction,
  envelope-encryption, and integrity primitives documented below.

## Controls (where implemented)

- **Redaction / secret scanning** — `brain0-agentsrc` `secret.rs` (`SecretScanner`), default-on
  via `Redactor`; typed placeholders `[REDACTED:<kind>]`; `RedactionEvent`s audited (kind only).
  Opt-out: `BRAIN0_DISABLE_SECRET_SCAN`. Extra patterns: `BRAIN0_REDACT`. Exclusions: `BRAIN0_EXCLUDE`.
- **At-rest encryption** — `brain0-crypto` envelope (ChaCha20-Poly1305, DEK-per-blob wrapped by a
  KEK). `EncryptedPayloadStore` (CLI default). KEK from `BRAIN0_KEK` (64 hex) or an auto-generated
  `0600` key file; fail-closed if a required key is missing. KEK rotation re-wraps DEKs only.
  Full DB encryption via `--features sqlcipher` (`SqliteStorage::open_encrypted`).
- **In-transit** — the open-core `brain0_storage::require_tls` policy helper enforces
  `sslmode=verify-full` (fail-closed; plaintext/`prefer`/`require` refused). The enterprise
  Postgres backend consumes it on connect and pairs it with a rustls/ring TLS connector over
  the OS trust store.
- **Egress** — only the explicitly-configured LLM/embeddings API and the configured remote DB.
  No telemetry. The GUI smart chat (`@brain0/server` `/api/debug`) has **two egress channels** —
  the LLM completion *and* the embedding of the user's query — and zero-egress holds only when
  **both** are local:

  | channel | LOCAL (default) | REMOTE (Anthropic / OpenAI) |
  |---|---|---|
  | LLM prompt | full enriched bundle, no network | redacted (named-secret scrub everywhere + high-entropy on the free-text query + absolute/out-of-repo read-path stripping) before `fetch` |
  | Embedding of the query | local feature-hash embedder, no network | the query is redacted before `embed()` |

  Defaults are privacy-first: the LLM is the **local Ollama** reasoner (a present API key opts into
  that remote provider; redacted + surfaced in the UI), and embeddings stay **local** even when
  `OPENAI_API_KEY` is set — the remote embedder is used only via `BRAIN0_EMBED_PROVIDER=openai`.
  There is no offline reasoner at runtime (`EchoLLM` is test-only); an unreachable provider yields a
  clear "no LLM" state, never a silent echo. The active provider + a truthful aggregate egress state
  are returned to the GUI. The query itself is part of egress.
- **Remote authz** — provided by the enterprise Postgres backend (roles `brain0_writer`
  (append-only), `brain0_reader` (read-only), `brain0_admin`; RLS by `brain0.repo` /
  `brain0.project`; append-only triggers blocking UPDATE/DELETE on versions/edges and DELETE on
  nodes). Lives in brain0-enterprise alongside the Postgres schema.
- **Integrity & audit** — `verify_payload` (content-addressed) + CLI `verify`; append-only
  `audit_log` + CLI `audit` (records redactions and purges, never values).
- **Purge / crypto-shred** — `PayloadStore::shred` destroys the blob (its only wrapped DEK ⇒
  irrecoverable); `payload_purged` tombstone keeps the graph topology; derived embeddings are
  invalidated. CLI `purge --task` and retention `--older-than-days`.
- **Hardening** — `#![forbid(unsafe_code)]` (workspace), `cargo deny` (advisories + licenses +
  bans) and `pnpm audit` in CI, committed lockfiles, untrusted-input limits in the JSONL reader
  (oversized lines skipped; session ids are hashed, never used as filesystem paths).

## Honest limitations / follow-ups

- The **PostgreSQL** backend, TLS, and RLS live in brain0-enterprise; the open-core repo keeps
  only the backend-agnostic `require_tls` policy helper. Live enforcement requires a running
  server.
- **SQLCipher** DB encryption is behind a feature flag (needs the SQLCipher/OpenSSL toolchain);
  the high-sensitivity payload is encrypted regardless via the envelope store.
- The **TypeScript** GUI/agent hydrate the *unencrypted* payload store; for encrypted-at-rest
  deployments, payload-bearing queries go through the Rust query path (which decrypts).
  TS-side decryption is a documented follow-up.
- The **persistent summary cache** (`~/.cache/brain0/summary-cache.db`) stores turn summaries
  in plaintext OUTSIDE the (encryptable) payload store, so index rebuilds cost zero model
  calls. Summaries are computed on already-redacted text (T0 never reaches them), but if the
  cache location worries you, point it elsewhere or disable it: `BRAIN0_SUMMARY_CACHE=path|off`.
- Test fixtures use only **fake** secrets (e.g. AWS's documented `AKIAIOSFODNN7EXAMPLE`).
