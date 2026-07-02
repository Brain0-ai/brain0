import { test } from "node:test";
import assert from "node:assert/strict";
import { redactFreeText, redactStructural, redactBundle, isExternal } from "./redact.js";

test("redactFreeText scrubs named secrets and high-entropy tokens", () => {
  // Fake keys are built by concatenation so no token-shaped literal lives in the source blob
  // (GitHub push protection blocks realistic-looking tokens, fake or not).
  const antKey = "sk-ant-" + "abcdefghijklmnopqrstuvwxyz123";
  const out = redactFreeText(
    `key ${antKey} and AKIA1234567890ABCDEF and ghp_` + "a".repeat(36),
  );
  assert.ok(!out.includes(antKey), "anthropic key gone");
  assert.ok(!out.includes("AKIA1234567890ABCDEF"), "aws key gone");
  assert.ok(out.includes("[REDACTED:anthropic_key]"));
  assert.ok(out.includes("[REDACTED:aws_access_key]"));
  // A high-entropy opaque token (no named prefix) is caught in free text.
  const token = "Zx9Qw3Er7Ty1Up5Ia2Sd6Fg8Hj4Kl0Mn3Bv7Cx2";
  assert.ok(redactFreeText(`internal=${token}`).includes("[REDACTED"), "entropy token caught");
});

test("redactFreeText redacts absolute/out-of-repo read paths", () => {
  const out = redactFreeText("read /home/u/.aws/credentials and src/main.ts");
  assert.ok(!out.includes("/home/u/.aws/credentials"), "external path stripped");
  assert.ok(out.includes("[REDACTED:path]"));
  assert.ok(out.includes("src/main.ts"), "in-repo relative path kept");
});

test("redactStructural keeps ids / git refs (no entropy over-redaction)", () => {
  const ref = "tsk_2952c9d46afa9b1c0d3e4f5a6b7c8d9e0f1a2b3c"; // long, hex-ish, id-like
  const out = redactStructural(`Task ${ref} risk=0.82 path=packages/gui/src/main.ts`);
  assert.ok(out.includes(ref), "structural keeps the id/ref intact");
  assert.ok(out.includes("packages/gui/src/main.ts"), "in-repo path kept");
});

test("redactStructural still scrubs a named secret embedded in the bundle", () => {
  const out = redactStructural("summary: leaked AKIA1234567890ABCDEF in code");
  assert.ok(!out.includes("AKIA1234567890ABCDEF"));
});

test("redactBundle redacts the query (entropy) and the structural part (named only)", () => {
  const out = redactBundle("paste AKIA1234567890ABCDEF here", "Task tsk_keepme risk=0.5");
  assert.ok(!out.includes("AKIA1234567890ABCDEF"));
  assert.ok(out.includes("tsk_keepme"), "structural id preserved");
});

test("isExternal flags absolute paths only", () => {
  assert.equal(isExternal("/etc/passwd"), true);
  assert.equal(isExternal("C:\\secrets\\x"), true);
  assert.equal(isExternal("packages/gui/src/main.ts"), false);
});
