import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyIntent } from "./intent.js";

test("classifyIntent detects audit-leaning queries", () => {
  const r = classifyIntent("did any secret or credential get read and sent to the model?");
  assert.equal(r.intent, "audit");
  assert.ok(r.confidence > 0);
});

test("classifyIntent detects debug-leaning queries", () => {
  const r = classifyIntent("why did the parser start failing — what introduced this bug?");
  assert.equal(r.intent, "debug");
  assert.ok(r.confidence > 0);
});

test("classifyIntent defaults to debug on neutral/empty input", () => {
  assert.deepEqual(classifyIntent(""), { intent: "debug", confidence: 0 });
  assert.equal(classifyIntent("tell me about this node").intent, "debug");
});
