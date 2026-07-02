# Contributor License Agreement (CLA)

This directory holds brain0's CLA process. It applies **only** to the public `brain0` repository
and **only** to external (non-founder) contributors.

## Why a CLA (and not just a DCO)

brain0 is **open core**: the public Apache-2.0 code here feeds into the commercial
`brain0-enterprise` product, which is dual-licensed (AGPLv3 + commercial). To re-license
contributions into the commercial product, the project must hold a license grant broad enough to
sublicense — a plain DCO sign-off does **not** convey that right, but a CLA does.
Contributors **retain their copyright**; this is a license, not an assignment.

This adds friction, which we accept consciously: without it, dual-licensing is not possible
(a trade-off we take on record).

## The agreements

- **Individual** contributors sign the [ICLA](./icla.md).
- Contributions made **on behalf of an employer** are covered by a [CCLA](./ccla.md) signed by
  the company, with a CLA Manager maintaining the list of designated employees.

Both are based on the Apache ICLA/CCLA models, adapted to brain0's open-core setup.

## How signing works (enforcement)

- The [CLA Assistant](https://github.com/contributor-assistant/github-action) bot runs as a
  **required status check** on every pull request (`.github/workflows/cla.yml`).
- A PR from a contributor who has not signed is **not mergeable** until they sign — the gate
  carries the same weight as CI.
- To sign, a contributor comments on their PR:
  `I have read the CLA Document and I hereby sign the CLA`
- Signatures are recorded by the bot in an **auditable registry** kept in a separate,
  private repository (`brain0-cla-signatures`) — so signatories' emails never live in the
  public repo. The registry file is `signatures.json` (GitHub username, name, email,
  timestamp, agreement version).

## Allowlist (exempt from the gate)

Founders and project bots are exempt. The authoritative allowlist is the
`allowlist` input of `.github/workflows/cla.yml`; keep this list in sync with it:

- Founders: `nicolalessi`, `AntonioVerdiglione1996`.
- Bots: `dependabot[bot]`, `github-actions[bot]`, `brain0-bot`.

`brain0-enterprise` is authored only by the copyright holders and therefore requires **no** CLA.

## Critical ordering

The CLA gate must be **active before the first external PR is accepted**:
a contribution merged without a CLA cannot be re-licensed later without tracking down and getting
a signature from every author — the same irreversibility logic as publishing under Apache-2.0.
