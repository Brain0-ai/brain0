/**
 * Heuristic intent classifier for the smart chat.
 *
 * The LLM self-classifies debug vs audit in its structured answer; this lexicon scorer is the
 * **fallback** intent used when the model output cannot be parsed, and seeds the low-confidence
 * UI hint. It is NEVER used to branch retrieval — retrieval is always intent-agnostic — so a
 * misclassification only affects presentation.
 */

import type { Intent } from "./structured.js";

const AUDIT_TERMS = [
  "audit",
  "secret",
  "secrets",
  "credential",
  "credentials",
  "leak",
  "leaked",
  "exposed",
  "exfiltrat",
  "egress",
  "privacy",
  "sensitive",
  "password",
  "token",
  "api key",
  "apikey",
  ".env",
  "read",
  "reads",
  "reached the model",
  "sent to",
  "compliance",
  "pii",
];

const DEBUG_TERMS = [
  "debug",
  "bug",
  "error",
  "fail",
  "failing",
  "broke",
  "broken",
  "crash",
  "regression",
  "root cause",
  "introduced",
  "stack trace",
  "exception",
  "why",
  "wrong",
  "issue",
  "fix",
  "not working",
];

function score(haystack: string, terms: string[]): number {
  let n = 0;
  for (const t of terms) if (haystack.includes(t)) n += 1;
  return n;
}

/**
 * Classify a query as `debug` or `audit` with a 0..1 confidence (the score margin). Ties and
 * empty input default to `debug` (brain0's historical default) at confidence 0.
 */
export function classifyIntent(query: string): { intent: Intent; confidence: number } {
  const q = query.toLowerCase();
  const audit = score(q, AUDIT_TERMS);
  const debug = score(q, DEBUG_TERMS);
  if (audit === 0 && debug === 0) return { intent: "debug", confidence: 0 };
  const intent: Intent = audit > debug ? "audit" : "debug";
  const total = audit + debug;
  const confidence = total === 0 ? 0 : Math.abs(audit - debug) / total;
  return { intent, confidence };
}
