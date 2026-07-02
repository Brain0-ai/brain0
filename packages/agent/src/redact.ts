/**
 * TS-side redaction for LLM/embedding egress.
 *
 * Payload summaries are already redacted at ingest (Rust SecretScanner), so this is
 * defense-in-depth over newly-assembled context plus a path-egress policy that Rust does not
 * apply (it records read paths "regardless of exclude"). High-signal named patterns are ported
 * from `crates/brain0-agentsrc/src/secret.rs`; the high-entropy detector is applied ONLY to
 * untrusted free text (the user query, summaries) — never to the id/ref-heavy structural bundle,
 * where it would over-redact.
 */

/** Out-of-repo / absolute path classifier (ported from the GUI's `isExternal`, main.ts). */
export function isExternal(path: string): boolean {
  return path.startsWith("/") || /^[A-Za-z]:[\\/]/.test(path);
}

function entropy(s: string): number {
  if (!s) return 0;
  const counts = new Map<string, number>();
  for (const ch of s) counts.set(ch, (counts.get(ch) ?? 0) + 1);
  let e = 0;
  for (const n of counts.values()) {
    const p = n / s.length;
    e -= p * Math.log2(p);
  }
  return e;
}

/** A long, mixed, high-entropy token is likely a secret/opaque token. */
function isHighEntropy(token: string): boolean {
  return token.length >= 32 && /[0-9]/.test(token) && /[A-Za-z]/.test(token) && entropy(token) >= 4.0;
}

// Named whole-match detectors → replaced entirely with [REDACTED:<kind>]. Order: specific first.
const WHOLE: Array<{ kind: string; re: RegExp }> = [
  { kind: "private_key", re: /-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----/g },
  { kind: "jwt", re: /\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g },
  { kind: "anthropic_key", re: /\bsk-ant-[A-Za-z0-9_-]{20,}\b/g },
  { kind: "openai_key", re: /\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}\b/g },
  { kind: "aws_access_key", re: /\bAKIA[0-9A-Z]{16}\b/g },
  { kind: "gcp_api_key", re: /\bAIza[0-9A-Za-z_-]{35}\b/g },
  { kind: "github_token", re: /\b(?:gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,})\b/g },
  { kind: "slack_token", re: /\bxox[baprs]-[A-Za-z0-9-]{10,}\b/g },
];

/** Named-pattern scrub (no entropy, no paths). Safe for both free text and the structural bundle. */
export function secretScrub(text: string): string {
  let out = text;
  for (const { kind, re } of WHOLE) out = out.replace(re, `[REDACTED:${kind}]`);
  // user:password@host → redact the password only.
  out = out.replace(/(:\/\/[^:@/\s]+:)([^@/\s]+)(@)/g, (_m, a: string, _pw: string, c: string) => `${a}[REDACTED:url_credentials]${c}`);
  // KEY=value / TOKEN: "value" → redact the value only.
  out = out.replace(
    /\b([a-z0-9_]*(?:key|token|secret|password|passwd|pwd|credential)[a-z0-9_]*)\s*([:=])\s*["']?([^"'\s]{6,})["']?/gi,
    (_m, k: string, sep: string, _v: string) => `${k}${sep}[REDACTED:env_secret]`,
  );
  return out;
}

/** Redact absolute / out-of-repo filesystem paths (in-repo relative paths pass through). */
export function redactPaths(text: string): string {
  return text
    // unix absolute path with ≥2 segments, not part of a URL (`://`).
    .replace(/(?<![:\w])\/(?:[A-Za-z0-9._-]+\/)+[A-Za-z0-9._-]+/g, "[REDACTED:path]")
    // windows drive path.
    .replace(/\b[A-Za-z]:\\(?:[A-Za-z0-9._-]+\\)*[A-Za-z0-9._-]+/g, "[REDACTED:path]");
}

function redactEntropy(text: string): string {
  return text.replace(/[A-Za-z0-9+/=_-]{32,}/g, (m) => (isHighEntropy(m) ? "[REDACTED:high_entropy]" : m));
}

/**
 * Untrusted free text (user query, decision summaries): named scrub + high-entropy + path policy.
 * Catches opaque tokens that match no named prefix — the most likely leak in a pasted query.
 */
export function redactFreeText(text: string): string {
  return redactPaths(redactEntropy(secretScrub(text)));
}

/**
 * The id/ref-heavy structural bundle: named scrub + path policy, but NO entropy (entropy would
 * shred legitimate node ids / git refs / fingerprints).
 */
export function redactStructural(text: string): string {
  return redactPaths(secretScrub(text));
}

/**
 * The last transform before remote egress: the (free-text) query gets the entropy detector, the
 * structural evidence does not, and both get the named scrub + path-egress sweep.
 */
export function redactBundle(query: string, structural: string): string {
  return `${redactFreeText(query)}\n\n${redactStructural(structural)}`;
}
