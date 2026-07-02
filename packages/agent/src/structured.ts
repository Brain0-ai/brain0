/**
 * Structured LLM output for the smart chat.
 *
 * The {@link LLMProvider} returns a string, so the universal floor is *prompt-and-parse*: ask the
 * model for a JSON object, then parse it tolerantly here. Capable providers may enforce the same
 * shape natively (OpenAI `response_format`, Anthropic forced tool_use) but BOTH paths funnel
 * through {@link parseStructured} so there is a single validation + fallback codepath.
 */

export type Intent = "debug" | "audit";
export type Severity = "info" | "warn" | "critical" | "gold";

const SEVERITIES: readonly Severity[] = ["info", "warn", "critical", "gold"];
const KINDS = new Set(["task", "artifact", "version"]);

/** One node the model chose to highlight, with its reason. Ids are validated against the bundle. */
export interface StructuredFinding {
  id: string;
  kind: "task" | "artifact" | "version";
  reason: string;
  severity: Severity;
  /** Optional short verdict (e.g. "secret read", "likely root cause"). */
  verdict?: string;
}

export interface StructuredResult {
  intent: Intent;
  highlights: StructuredFinding[];
  explanation: string;
}

/**
 * JSON schema for providers that support native structured output. Per OpenAI strict mode every
 * key must be in `required` and `additionalProperties` must be false, so `verdict` is modeled as
 * `string | null` (the tolerant parser treats null/"" as absent).
 */
export const STRUCTURED_SCHEMA = {
  type: "object",
  additionalProperties: false,
  properties: {
    intent: { type: "string", enum: ["debug", "audit"] },
    highlights: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        properties: {
          id: { type: "string" },
          kind: { type: "string", enum: ["task", "artifact", "version"] },
          reason: { type: "string" },
          severity: { type: "string", enum: ["info", "warn", "critical", "gold"] },
          verdict: { type: ["string", "null"] },
        },
        required: ["id", "kind", "reason", "severity", "verdict"],
      },
    },
    explanation: { type: "string" },
  },
  required: ["intent", "highlights", "explanation"],
} as const;

/** Pull the first JSON object out of a raw model response (handles ```json fences and prose). */
function extractJson(raw: string): string | undefined {
  let s = raw.trim();
  // Strip a leading/trailing code fence if present.
  const fence = s.match(/```(?:json)?\s*([\s\S]*?)```/i);
  if (fence?.[1]) s = fence[1].trim();
  const start = s.indexOf("{");
  const end = s.lastIndexOf("}");
  if (start === -1 || end === -1 || end < start) return undefined;
  return s.slice(start, end + 1);
}

function clampSeverity(value: unknown): Severity {
  return SEVERITIES.includes(value as Severity) ? (value as Severity) : "info";
}

/** Validate one highlight object; returns undefined for hallucinated/unknown/ill-formed entries. */
function validateHighlight(h: unknown, knownIds: Set<string>): StructuredFinding | undefined {
  if (typeof h !== "object" || h === null) return undefined;
  const hi = h as Record<string, unknown>;
  const id = typeof hi.id === "string" ? hi.id : "";
  const kind = typeof hi.kind === "string" ? hi.kind : "";
  if (!id || !knownIds.has(id) || !KINDS.has(kind)) return undefined;
  const verdict = typeof hi.verdict === "string" && hi.verdict.trim() ? hi.verdict.trim() : undefined;
  return {
    id,
    kind: kind as StructuredFinding["kind"],
    reason: typeof hi.reason === "string" ? hi.reason : "",
    severity: clampSeverity(hi.severity),
    verdict,
  };
}

function validateHighlights(arr: unknown[], knownIds: Set<string>): StructuredFinding[] {
  const out: StructuredFinding[] = [];
  for (const h of arr) {
    const f = validateHighlight(h, knownIds);
    if (f) out.push(f);
  }
  return out;
}

/**
 * Salvage the COMPLETE `{...}` objects from a (possibly truncated) `"highlights": [ … ]` array by
 * scanning with string/escape awareness. A trailing object cut off by a token limit is skipped, so
 * a truncated answer still yields the findings that fully arrived. Returns parsed objects.
 */
function recoverHighlightObjects(text: string): unknown[] {
  const key = text.indexOf('"highlights"');
  if (key < 0) return [];
  const open = text.indexOf("[", key);
  if (open < 0) return [];
  const out: unknown[] = [];
  let depth = 0;
  let objStart = -1;
  let inStr = false;
  let esc = false;
  for (let i = open + 1; i < text.length; i++) {
    const ch = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (ch === "\\") esc = true;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === '"') inStr = true;
    else if (ch === "{") {
      if (depth === 0) objStart = i;
      depth += 1;
    } else if (ch === "}") {
      depth -= 1;
      if (depth === 0 && objStart >= 0) {
        try {
          out.push(JSON.parse(text.slice(objStart, i + 1)));
        } catch {
          /* skip a malformed object */
        }
        objStart = -1;
      }
    } else if (ch === "]" && depth === 0) {
      break;
    }
  }
  return out;
}

/**
 * Tolerantly parse a model response into a {@link StructuredResult}: strip fences, extract the JSON
 * object, validate (dropping hallucinated ids, clamping severity). If strict parse fails (usually a
 * token-truncated answer), recover the complete highlight objects so the user still gets cards;
 * only if nothing is recoverable show a clean note. Genuine non-JSON prose is shown verbatim.
 */
export function parseStructured(
  raw: string,
  knownIds: Set<string>,
  fallbackIntent: Intent,
): StructuredResult {
  const trimmed = raw.trim();
  const looksJson = trimmed.startsWith("{") || /^```(?:json)?/i.test(trimmed);
  const intentOf = (v: unknown): Intent => (v === "audit" ? "audit" : v === "debug" ? "debug" : fallbackIntent);

  const recover = (): StructuredResult => {
    const recovered = validateHighlights(recoverHighlightObjects(raw), knownIds);
    if (recovered.length > 0) {
      const im = raw.match(/"intent"\s*:\s*"(debug|audit)"/);
      return {
        intent: intentOf(im?.[1]),
        highlights: recovered,
        explanation: "The model's answer was cut off (too long) — showing the findings recovered before the cut.",
      };
    }
    return {
      intent: fallbackIntent,
      highlights: [],
      explanation:
        "The model's structured answer was malformed or cut off. Try a more specific question, or a stronger / higher-token model.",
    };
  };

  const jsonText = extractJson(raw);
  if (!jsonText) {
    if (!looksJson) return { intent: fallbackIntent, highlights: [], explanation: trimmed }; // genuine prose
    return recover();
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(jsonText);
  } catch {
    return recover(); // truncated/broken JSON → salvage what completed
  }
  if (typeof parsed !== "object" || parsed === null) return recover();
  const obj = parsed as Record<string, unknown>;

  return {
    intent: intentOf(obj.intent),
    highlights: Array.isArray(obj.highlights) ? validateHighlights(obj.highlights, knownIds) : [],
    explanation: typeof obj.explanation === "string" ? obj.explanation : "",
  };
}
