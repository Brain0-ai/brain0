# brain0 — Open-core model, licensing, and entitlement architecture

This document defines brain0's open-core boundary, its licensing model, the entitlement
architecture, and an honest threat model.

## 1. The principle (read first)

A license check embedded in open-source code protects nothing: anyone can fork it, delete the
check, and recompile. brain0's protection is therefore **architectural, not
cosmetic**:

- Premium code is **not in the public repo** — not even disabled. There is nothing to re-enable.
- The highest-value capabilities **run only on the vendor's server**; the client calls them over
  an authenticated API. Code the client never receives cannot be cracked.
- The client-side entitlement verifier is **deterrence + a lever on honest customers**, never the
  sole defense.

## 2. Free vs paid boundary

**Free — Apache-2.0, public `brain0` repo, a complete product for a single developer** (never
crippled):

- local single-user mode; git observer + checkpoint engine; the bipartite decision graph; symbol
  identity; Tree-sitter parsing; passive local ingest; local summarizer + embeddings
 ; the PixiJS GUI; debugging + auditing on your own machine;
- **secret redaction** — baseline security stays free: table stakes and a trust signal;
- the **SQLite + sqlite-vec** storage backend **only**.

**Paid — the "governance & scale" layer organizations need** (private `brain0-enterprise` repo):

- **any** PostgreSQL/pgvector backend (the Postgres implementation of the storage abstraction,
  even single-user/self-host); managed SaaS; multi-user collaboration with authz + per-repo RLS
  isolation; SSO/SAML/SCIM; org-level cross-repo audit dashboards; advanced compliance (audit-log
  retention, tamper-evident hash-chaining, compliance export, KMS integration, data residency);
  long-term managed storage; support + SLA; private adapters for proprietary agents.

**Storage boundary (fixed):** the public repo ships **only** the SQLite backend. **Any**
Postgres/pgvector backend is private. The storage *abstraction* (`brain0_storage::Storage`) stays
public; its Postgres *implementation* does not.

## 3. Repository & crate layout

| Repo | Visibility | License | Contents |
|---|---|---|---|
| `brain0` | public | Apache-2.0 | engine + local product + the stable extension interfaces |
| `brain0-enterprise` | private | AGPLv3 + commercial | premium/team/server components + entitlement subsystem |

`brain0-enterprise` depends on `brain0` as a library and implements its extension interfaces.
**`brain0` never depends on `brain0-enterprise`.**

### The extension interface that stays open

The public, documented seam premium backends plug into (, DoD):

- `brain0_storage::Storage` — the trait every backend implements (SQLite in the open;
  Postgres in enterprise).
- `brain0_storage::backend` — column lists + column⇄model codecs + the shingle blob codec, so
  every backend serializes the graph identically.
- `brain0_storage::StorageError::Backend` / `StorageError::backend()` — lets an out-of-tree
  backend flatten its driver errors into the shared error type (the orphan rule prevents a direct
  `From` impl in the foreign crate).
- `brain0_storage::require_tls` — the backend-agnostic TLS policy (`sslmode=verify-full`).

Premium crates (in `brain0-enterprise`):

| Crate | Role |
|---|---|
| `brain0-ent-storage` | `PgStorage` implementing `brain0_storage::Storage` over Postgres + pgvector |
| `brain0-ent-license` | Ed25519 signed entitlements, public-key verification, fail-closed gate |
| `brain0-ent-server` | entitlement server (issue/revoke/heartbeat) + server-side-only capabilities |

## 4. Licensing model

- **Engine + local product (`brain0`): Apache-2.0.** Permissive, maximizes adoption — it is the
  free product that builds the user base.
- **Server/team components (`brain0-enterprise`): AGPLv3.** Network copyleft neutralizes the
  "someone hosts our server in competition and gives nothing back" scenario. Combined with
  copyright aggregation via the CLA, AGPL also enables **dual-licensing**: a commercial license is
  available for those who want the server components without the AGPL obligations.
- The engine stays Apache-2.0; only the server components are AGPLv3.

### CLA

The public repo requires a **CLA** (not just a DCO) because its Apache-2.0 code feeds the
dual-licensed commercial product; re-licensing requires a sublicense grant a DCO does not convey.
- Applies only to `brain0`, only to external (non-founder) contributors; founders/bots are
  allowlisted; `brain0-enterprise` is exempt.
- Apache-based ICLA/CCLA drafts live in [`../cla/`](../cla/) (pending legal review).
- A CLA-Assistant bot gates every PR and records signatures in an auditable registry.
- **Critical ordering:** the gate must be active *before* the first external PR is accepted — an
  unsigned merged contribution cannot be re-licensed retroactively without tracing every author.

## 5. Entitlement architecture

The entitlement subsystem is **external** to the public repo and **hardened**.

1. **Entitlement server** (vendor-controlled, `brain0-ent-server`) issues and revokes
   **asymmetrically signed** entitlements and holds the **private** signing key.
2. **Signed entitlements** (Ed25519) declare customer, tier, capabilities, instance binding,
   `key_id`, and validity window. Closed components verify with the **public key only** — they can
   *verify* but cannot *mint*. Verification is offline-capable; **revocation** comes from a list
   refreshed by a periodic online check.
3. **Heartbeat/activation**: the server re-issues a **short-lived,
   instance-bound** activation token, limiting license sharing. This is not telemetry (§8).
4. **Fail-closed gate**: premium runs only with a verified entitlement carrying
   the capability; missing/expired/revoked/instance-mismatched ⇒ denied. The **free product never
   calls this** and knows nothing of the subsystem.

### Server-side-only value (the primary, uncrackable defense —)

The highest-value capabilities are **not delivered at all**; they run on the vendor backend and
the client calls them via authenticated API. Example shipped here: **cross-org audit
aggregation** (`aggregate_cross_org_audit` in `brain0-ent-server`) — gated by entitlement and
present only server-side. Without an account/entitlement the capability does not exist locally, so
there is nothing to crack. The design moves as much value here as possible.

## 6. License-subsystem security

- The **signing private key** lives in an HSM / secret manager, injected at deploy time via an env
  var (`Signer::from_env`), **never** in any repo or log. `Signer`'s `Debug` redacts key material;
  enterprise CI greps for committed key material and fails on any.
- The entitlement server has a minimal surface, treats untrusted input carefully (size-bounded,
  no panics, no injection), keeps an **append-only audit log** of license events (issue/revoke/
  activate), and uses **TLS verify-full** (`require_secure_endpoint`, `brain0_storage::require_tls`).
- **Signed builds + integrity-verify-at-startup** are release-process requirements for the closed
  components; key **rotation** is supported via `key_id` in the entitlements.

### License channel privacy

The license check is a **separate, minimal, documented** channel. It transmits only
entitlement/instance identifiers — **never** transcripts, payload, prompts, or graph metadata. It
does not weaken the no-egress guarantee on user data. The **free product makes no
license check at all** and never contacts the entitlement server. This is covered by tests.

## 7. Out of scope (explicit —)

No license check in open code; no telemetry on user data; no crippling of the free product; no
premium code in the public repo, even disabled.

## 8. Honest threat model

Anything running on the customer's machine is ultimately inspectable and bypassable. Client-side
verification raises the cost of an attack and leverages honest customers (legal risk, audits,
support needs) — it is **not** a cryptographic guarantee. The only by-construction-uncrackable
boundary is what we **do not ship**: the server-side capabilities (§5). The architecture is
designed to keep as much value there as possible.
