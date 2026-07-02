import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveLlmConfig, resolveEmbedderConfig } from "./llm-config.js";

test("LLM defaults to local Ollama with zero egress", () => {
  const c = resolveLlmConfig({});
  assert.equal(c.provider, "ollama");
  assert.equal(c.remote, false);
  assert.equal(c.endpoint, "http://localhost:11434/v1");
  assert.equal(c.model, "qwen3:4b");
});

test("a present API key opts the LLM into that remote provider", () => {
  assert.equal(resolveLlmConfig({ ANTHROPIC_API_KEY: "k" }).provider, "anthropic");
  assert.equal(resolveLlmConfig({ ANTHROPIC_API_KEY: "k" }).remote, true);
  assert.equal(resolveLlmConfig({ OPENAI_API_KEY: "k" }).provider, "openai");
});

test("pinning BRAIN0_LLM_PROVIDER=ollama overrides a stale API key", () => {
  const c = resolveLlmConfig({ BRAIN0_LLM_PROVIDER: "ollama", ANTHROPIC_API_KEY: "stale" });
  assert.equal(c.provider, "ollama");
  assert.equal(c.remote, false);
});

test("embeddings stay LOCAL even when OPENAI_API_KEY is present (closes the silent leak)", () => {
  const e = resolveEmbedderConfig({ OPENAI_API_KEY: "k" });
  assert.equal(e.provider, "local");
  assert.equal(e.remote, false);
});

test("embeddings go remote only on explicit opt-in", () => {
  const e = resolveEmbedderConfig({ BRAIN0_EMBED_PROVIDER: "openai", OPENAI_API_KEY: "k" });
  assert.equal(e.provider, "openai");
  assert.equal(e.remote, true);
});

test("echo provider is reachable only by explicit pin", () => {
  assert.equal(resolveLlmConfig({ BRAIN0_LLM_PROVIDER: "echo" }).provider, "echo");
});
