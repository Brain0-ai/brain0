import { test } from "node:test";
import assert from "node:assert/strict";
import { parseStructured } from "./structured.js";

const known = new Set(["tsk_1", "art_1"]);

test("parseStructured reads a clean JSON object", () => {
  const raw = JSON.stringify({
    intent: "debug",
    highlights: [{ id: "tsk_1", kind: "task", reason: "root cause", severity: "critical", verdict: "likely cause" }],
    explanation: "the parser change broke it",
  });
  const r = parseStructured(raw, known, "audit");
  assert.equal(r.intent, "debug");
  assert.equal(r.explanation, "the parser change broke it");
  assert.equal(r.highlights.length, 1);
  assert.deepEqual(r.highlights[0], {
    id: "tsk_1",
    kind: "task",
    reason: "root cause",
    severity: "critical",
    verdict: "likely cause",
  });
});

test("parseStructured extracts JSON from a ```json fence with surrounding prose", () => {
  const raw = "Here you go:\n```json\n" + JSON.stringify({ intent: "audit", highlights: [], explanation: "ok" }) + "\n```\nthanks";
  const r = parseStructured(raw, known, "debug");
  assert.equal(r.intent, "audit");
  assert.equal(r.explanation, "ok");
});

test("parseStructured drops hallucinated ids and clamps bad severity", () => {
  const raw = JSON.stringify({
    intent: "debug",
    highlights: [
      { id: "art_1", kind: "artifact", reason: "x", severity: "nonsense", verdict: null },
      { id: "ghost", kind: "artifact", reason: "y", severity: "warn", verdict: "" },
    ],
    explanation: "e",
  });
  const r = parseStructured(raw, known, "audit");
  assert.equal(r.highlights.length, 1, "ghost id dropped");
  assert.equal(r.highlights[0]!.id, "art_1");
  assert.equal(r.highlights[0]!.severity, "info", "bad severity clamped");
  assert.equal(r.highlights[0]!.verdict, undefined, "null verdict → absent");
});

test("parseStructured falls back to prose on non-JSON output", () => {
  const r = parseStructured("I think the tokenizer is fine, no JSON here.", known, "debug");
  assert.equal(r.intent, "debug");
  assert.deepEqual(r.highlights, []);
  assert.equal(r.explanation, "I think the tokenizer is fine, no JSON here.");
});

test("parseStructured RECOVERS complete findings from a truncated answer", () => {
  // Token-truncated: two complete highlight objects, then the explanation is cut mid-string.
  const truncated =
    '```json\n{ "intent":"audit", "highlights":[ {"id":"tsk_1","kind":"task","reason":"a","severity":"gold","verdict":null}, {"id":"art_1","kind":"artifact","reason":"b","severity":"critical","verdict":"x"} ], "explanation":"The question is an audit query about which files were rea';
  const r = parseStructured(truncated, known, "debug");
  assert.equal(r.intent, "audit", "intent recovered");
  assert.equal(r.highlights.length, 2, "both complete findings recovered");
  assert.equal(r.highlights[0]!.id, "tsk_1");
  assert.ok(!r.explanation.includes('"highlights"'), "raw JSON blob is NOT shown");
  assert.match(r.explanation, /cut off/i);
});

test("parseStructured shows a clean note when nothing is recoverable", () => {
  const truncated = '```json\n{ "intent":"debug", "highlights":[ {"id":"tsk_1","kind":"task","reason":"a"'; // first obj incomplete
  const r = parseStructured(truncated, known, "debug");
  assert.deepEqual(r.highlights, []);
  assert.ok(!r.explanation.includes('"highlights"'));
  assert.match(r.explanation, /malformed|cut off/i);
});

test("parseStructured uses the fallback intent when intent is missing/unknown", () => {
  const raw = JSON.stringify({ intent: "weird", highlights: [], explanation: "e" });
  assert.equal(parseStructured(raw, known, "audit").intent, "audit");
});
