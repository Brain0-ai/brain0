/**
 * Live connectivity check against the real OrcaRouter gateway.
 *
 * It runs through the code paths this PR adds — {@link OrcaRouterProvider} for inference and
 * {@link discoverModels} for the catalog — so a green result means the integration itself works,
 * not that a standalone HTTP client does. It is skipped (never passed) when no key is present.
 *
 * The key is read from `ORCAROUTER_API_KEY` only; nothing is hard-coded and nothing is logged.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { OrcaRouterProvider, discoverModels } from "@brain0/agent";

const key = process.env.ORCAROUTER_API_KEY ?? "";
const live = key.length > 0;

test("live: the OrcaRouter provider completes a real chat request", { skip: !live }, async () => {
  // The verification key may not be entitled to the cyber tier; fall back to a public model this
  // key can call. The recommendation shown to users is unaffected (orcarouter/auto by default).
  const attempts = ["orca/orcacyber-ultra-1.0", "deepseek/deepseek-v4.1-flash"];
  const provider = new OrcaRouterProvider(key, attempts[0]!, "https://api.orcarouter.ai/v1");
  let answer: string;
  try {
    answer = await provider.complete([{ role: "user", content: "Reply with the single word ok." }]);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    if (!/40[13]/.test(message)) throw err;
    const fallback = new OrcaRouterProvider(key, attempts[1]!, "https://api.orcarouter.ai/v1");
    answer = await fallback.complete([{ role: "user", content: "Reply with the single word ok." }]);
  }
  assert.ok(answer.trim().length > 0, "the provider must return content");
  assert.ok(!answer.includes(key), "the response must never echo the key");
  assert.equal(provider.descriptor?.name, "orcarouter");
  assert.equal(provider.descriptor?.endpoint, "https://api.orcarouter.ai/v1");
});

test("live: discovery returns the real chat catalog, filtered to compatible routes", { skip: !live }, async () => {
  const result = await discoverModels({ apiKey: key });
  assert.equal(result.source, "live");
  assert.equal(result.degraded, false);
  assert.ok(result.models.length > 0, "the live catalog must not be empty");
  const ids = result.models.map((m) => m.id);
  assert.ok(ids.includes("orcarouter/auto"), "orcarouter/auto is the provider default");
  assert.ok(
    result.models.every((m) =>
      m.supportedEndpointTypes.some((t) => ["openai", "openai-response", "anthropic", "gemini"].includes(t)),
    ),
    "every advertised model must speak an endpoint type this client can call",
  );
  // A multimodal request narrows to models that explicitly declare image input.
  const vision = await discoverModels({ apiKey: key, requiredModality: "image" });
  assert.ok(vision.models.every((m) => m.inputModalities.includes("image")));
  assert.ok(vision.models.length < result.models.length);
});
