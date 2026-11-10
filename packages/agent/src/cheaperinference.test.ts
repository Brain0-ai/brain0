import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveLlmConfig, resolveEmbedderConfig } from "./llm-config.js";
import {
  CHEAPERINFERENCE_API_BASE,
  CHEAPERINFERENCE_DEFAULT_MODEL,
} from "./cheaperinference.js";
import { providerEntry } from "./provider-registry.js";

// A placeholder key. Real keys never appear in tests.
const FAKE_KEY = "test-key-0000";

test("Cheaper Inference is a named provider with its own endpoint and default model", () => {
  const c = resolveLlmConfig({ BRAIN0_LLM_PROVIDER: "cheaperinference" });
  assert.equal(c.provider, "cheaperinference");
  assert.equal(c.endpoint, CHEAPERINFERENCE_API_BASE);
  assert.equal(c.endpoint, "https://api.cheaperinference.com/v1");
  assert.equal(c.model, CHEAPERINFERENCE_DEFAULT_MODEL);
  assert.equal(c.model, "gpt-5.4-mini");
  assert.equal(c.remote, true);
});

test("Cheaper Inference reads CHEAPER_INFERENCE_API_KEY, and never auto-selects itself", () => {
  const pinned = resolveLlmConfig({
    BRAIN0_LLM_PROVIDER: "cheaperinference",
    CHEAPER_INFERENCE_API_KEY: FAKE_KEY,
  });
  assert.equal(pinned.apiKey, FAKE_KEY);
  // A key alone must not silently reroute prompts away from the local default.
  assert.equal(resolveLlmConfig({ CHEAPER_INFERENCE_API_KEY: FAKE_KEY }).provider, "ollama");
  assert.equal(resolveEmbedderConfig({ CHEAPER_INFERENCE_API_KEY: FAKE_KEY }).provider, "local");
});

test("model and endpoint overrides apply to Cheaper Inference", () => {
  const c = resolveLlmConfig({
    BRAIN0_LLM_PROVIDER: "cheaperinference",
    BRAIN0_LLM_MODEL: "claude-sonnet-5",
    BRAIN0_LLM_ENDPOINT: "https://relay.internal.example/v1",
  });
  assert.equal(c.model, "claude-sonnet-5");
  assert.equal(c.endpoint, "https://relay.internal.example/v1");
});

test("the provider registry lists Cheaper Inference as its own entry", () => {
  const entry = providerEntry("cheaperinference");
  assert.ok(entry, "cheaperinference must be registered");
  assert.equal(entry?.name, "Cheaper Inference");
  assert.equal(entry?.endpoint, "https://api.cheaperinference.com/v1");
  assert.equal(entry?.defaultModel, "gpt-5.4-mini");
  assert.equal(entry?.remote, true);
  assert.equal(entry?.catalog, false);
  assert.equal(entry?.envKey, "CHEAPER_INFERENCE_API_KEY");
});
