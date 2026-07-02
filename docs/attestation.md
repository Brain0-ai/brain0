# brain0 — AI provenance & attestation

For any commit, brain0 can answer: **which agent and model produced it, what files (and lines) it
changed, what it read, whether a secret was among those reads, whether a human reviewed it** — and
then emit a **signed, verifiable** attestation of that record. This is the evidence an auditor or a
supply-chain policy needs for AI-authored code.

It reuses the existing decision graph (no new capture): the commit task, the agent sessions joined to
it by shared artifact versions, the captured reads + secret kinds, the model id, and the review
trailers. Five commands expose it.

## `brain0 provenance <sha>` — the record

```bash
brain0 provenance a0c7c90
```

```jsonc
{
  "commit": "a0c7c90bd1…",
  "author": "Nicola",
  "timestamp": "2026-06-21T19:20:03+00:00",
  "reviewed": false,
  "reviewers": [],
  "changed_files": ["crates/…/claude.rs", …],
  "agents": [
    {
      "task": "tsk_…",
      "agent": "claude-code",
      "model": "claude-opus-4-8",
      "reads": ["…"],
      "read_secrets": [{ "path": "config.py", "kinds": ["aws_access_key"] }],
      "drift_undeclared": ["…"],
      "drift_phantom": ["…"]
    }
  ]
}
```

The agent sessions are found via the same shared-version join the GUI uses (an agent task linked to
a version this commit produced). `read_secrets` carries the secret KIND only — never the value.

## `brain0 attribution <sha>` — per-file / per-hunk

```bash
brain0 attribution a0c7c90 --no-encrypt-payload
```

For each changed file it parses the new-side hunk ranges from the stored unified diff:

```jsonc
{
  "commit": "a0c7c90bd1…",
  "reviewed": false,
  "models": ["claude-opus-4-8"],
  "files": [
    { "file": "crates/…/claude.rs", "lines_added": 3, "lines_removed": 1,
      "hunks": [{ "start": 279, "lines": 9, "end": 287 }] }
  ]
}
```

Granularity is per-file/hunk; per-symbol/line is a future refinement. Note the payload store must be
readable — if it is unencrypted (no `keyring.key`) pass `--no-encrypt-payload`.

## Review status — git trailers

"Was the AI change human-reviewed?" is answered offline from commit-message trailers. brain0 parses
`Reviewed-by:` and `Acked-by:` (case-insensitive) at observe time and stores them on the commit:

```
feat: thing

Reviewed-by: Grace Hopper <grace@navy.mil>
Acked-by: Linus
```

→ `reviewed: true`, `reviewers: ["Grace Hopper <grace@navy.mil>", "Linus"]`, surfaced in provenance,
bound into the signed attestation, and counted in `compliance`. (A GitHub/GitLab PR-review provider
via `gh` is a documented future addition.)

## `brain0 attest` / `brain0 verify-attestation` — signed

`attest` wraps the provenance in an [in-toto](https://in-toto.io) Statement (subject = the commit,
predicate = the provenance) and signs the serialized statement with Ed25519:

```bash
brain0 attest a0c7c90 > a0c7c90.att.json
```

```jsonc
{
  "statement": "{…in-toto Statement JSON…}",   // subject=git+commit, predicate=provenance
  "keyid": "3ee2a8a7283cb2fd",
  "publicKey": "3ee2a8a7…",
  "signature": "a5798672…"                       // 128-hex Ed25519 over the statement bytes
}
```

```bash
brain0 verify-attestation a0c7c90.att.json                 # embedded key (internal consistency)
brain0 verify-attestation a0c7c90.att.json --pubkey <hex>  # against a TRUSTED key
# tampering the statement → "attestation INVALID", exit 1
```

### Signing keys

The signing key is a 32-byte Ed25519 seed (64 hex). Provide it via `BRAIN0_ATTEST_KEY` (env) or
`--key-file` (default `.brain0/attest.key`, generated `0600` if absent). The verifier needs only the
public key — it can confirm an attestation without the ability to mint one. The private key is
redacted in all logs / Debug output.

> Note: the envelope signs the literal statement string (the verifier re-checks those exact bytes).
> A strict [DSSE](https://github.com/secure-systems-lab/dsse)/base64 envelope is a documented
> refinement; the current form is verifiable and self-contained.

## `brain0 compliance` — the auditor pack

Aggregate provenance across the whole index:

```bash
brain0 compliance              # human report
brain0 compliance --json       # summary + per-commit rows
brain0 compliance --strict     # exit non-zero if any secret-bearing read exists (release/PR gate)
```

```
brain0 compliance report
  commits indexed:          8
  AI-assisted commits:      6
  └─ unreviewed (no human): 6
  models seen:              claude-opus-4-8
  commits w/ secret reads:  0  (total 0 secret-file read(s))
  undeclared drift events:  453
```

It surfaces the two findings auditors care about most: **commits where a secret reached a remote
model**, and **AI-assisted commits that no human reviewed**.
