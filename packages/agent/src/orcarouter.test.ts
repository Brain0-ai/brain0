import { test } from "node:test";
import assert from "node:assert/strict";
import { resolveLlmConfig, resolveEmbedderConfig } from "./llm-config.js";
import {
  ORCAROUTER_API_BASE,
  ORCAROUTER_DEFAULT_MODEL,
  looksLikeOrcaRouterKey,
  maskOrcaRouterKey,
  resolveOrcaRouterBase,
} from "./orcarouter.js";
import {
  CredentialGenerations,
  SessionCredentialSource,
  classifyOrcaRouterStatus,
  resolveOrcaRouterCredential,
} from "./orcarouter-credentials.js";
import { PROVIDER_REGISTRY, providerEntry } from "./provider-registry.js";

// A shape-valid fake key. Real keys never appear in tests.
const FAKE_KEY = "sk-orca-test-0000000000000000abcd";

test("OrcaRouter is a named provider with its own endpoint and default model", () => {
  const c = resolveLlmConfig({ BRAIN0_LLM_PROVIDER: "orcarouter" });
  assert.equal(c.provider, "orcarouter");
  assert.equal(c.endpoint, ORCAROUTER_API_BASE);
  assert.equal(c.endpoint, "https://api.orcarouter.ai/v1");
  assert.equal(c.model, ORCAROUTER_DEFAULT_MODEL);
  assert.equal(c.model, "orcarouter/auto");
  assert.equal(c.remote, true);
});

test("OrcaRouter reads its key from ORCAROUTER_API_KEY, and never auto-selects itself", () => {
  const pinned = resolveLlmConfig({ BRAIN0_LLM_PROVIDER: "orcarouter", ORCAROUTER_API_KEY: FAKE_KEY });
  assert.equal(pinned.apiKey, FAKE_KEY);
  // A key alone must not silently reroute prompts away from the local default.
  assert.equal(resolveLlmConfig({ ORCAROUTER_API_KEY: FAKE_KEY }).provider, "ollama");
});

test("an explicit API endpoint override wins; the shared default stays canonical", () => {
  assert.equal(resolveOrcaRouterBase({}), "https://api.orcarouter.ai/v1");
  assert.equal(
    resolveOrcaRouterBase({ BRAIN0_ORCAROUTER_ENDPOINT: "https://relay.internal.example/v1/" }),
    "https://relay.internal.example/v1",
  );
  const c = resolveLlmConfig({
    BRAIN0_LLM_PROVIDER: "orcarouter",
    BRAIN0_ORCAROUTER_ENDPOINT: "https://relay.internal.example/v1",
  });
  assert.equal(c.endpoint, "https://relay.internal.example/v1");
});

test("embedding defaults are untouched by the new provider", () => {
  assert.equal(resolveEmbedderConfig({ ORCAROUTER_API_KEY: FAKE_KEY }).provider, "local");
});

test("key shape check is a lightweight format check, not validation", () => {
  assert.equal(looksLikeOrcaRouterKey(FAKE_KEY), true);
  assert.equal(looksLikeOrcaRouterKey("sk-orca-"), false);
  assert.equal(looksLikeOrcaRouterKey("sk-openai-123456789012"), false);
  assert.equal(maskOrcaRouterKey(FAKE_KEY), "sk-orca-…abcd");
  assert.equal(maskOrcaRouterKey("nonsense"), "••••");
  assert.ok(!maskOrcaRouterKey(FAKE_KEY).includes("test-0000"));
});

test("one credential resolver serves the session adapter and the environment adapter", () => {
  const session = new SessionCredentialSource();
  assert.equal(resolveOrcaRouterCredential({}, session), undefined);

  const fromEnv = resolveOrcaRouterCredential({ ORCAROUTER_API_KEY: FAKE_KEY }, session);
  assert.equal(fromEnv?.key, FAKE_KEY);
  assert.equal(fromEnv?.source, "env");

  session.set("sk-orca-session-1111111111111111wxyz");
  const fromSession = resolveOrcaRouterCredential({ ORCAROUTER_API_KEY: FAKE_KEY }, session);
  assert.equal(fromSession?.source, "session");
  assert.equal(fromSession?.masked, "sk-orca-…wxyz");

  session.clear();
  assert.equal(resolveOrcaRouterCredential({ ORCAROUTER_API_KEY: FAKE_KEY }, session)?.source, "env");
});

test("an obviously malformed key is rejected before it is stored", () => {
  const session = new SessionCredentialSource();
  assert.throws(() => session.set("not-a-key"), /sk-orca/);
  assert.equal(session.read(), undefined);
});

test("gateway failures are classified; a 401 is terminal reauthentication", () => {
  assert.equal(classifyOrcaRouterStatus(401), "needs-reauth");
  assert.equal(classifyOrcaRouterStatus(403), "needs-reauth");
  assert.equal(classifyOrcaRouterStatus(429), "rate-limited");
  assert.equal(classifyOrcaRouterStatus(502), "unreachable");
  assert.equal(classifyOrcaRouterStatus(undefined), "unreachable");
  assert.equal(classifyOrcaRouterStatus(400), "provider-error");
});

test("a stale failure cannot mark a newly installed credential", () => {
  const gens = new CredentialGenerations();
  const first = gens.current();
  assert.equal(first.generation, 0);

  gens.replace(); // new key installed
  const second = gens.current();
  assert.equal(second.generation, 1);

  // A late 401 from a request issued under generation 0 must not touch generation 1.
  const afterStale = gens.fail(first.generation, "old key rejected");
  assert.equal(afterStale.needsReauth, false);
  assert.equal(afterStale.generation, 1);

  const afterCurrent = gens.fail(second.generation, "key revoked");
  assert.equal(afterCurrent.needsReauth, true);
  assert.match(afterCurrent.reason ?? "", /revoked/);

  gens.replace(); // reconnect clears the terminal state
  assert.equal(gens.current().needsReauth, false);
});

test("the provider registry exposes OrcaRouter as a first-class, catalogued entry", () => {
  const entry = providerEntry("orcarouter");
  assert.ok(entry, "orcarouter must be registered");
  assert.equal(entry?.name, "OrcaRouter Cyber Inference");
  assert.equal(entry?.endpoint, "https://api.orcarouter.ai/v1");
  assert.equal(entry?.defaultModel, "orcarouter/auto");
  assert.equal(entry?.catalog, true);
  assert.equal(entry?.envKey, "ORCAROUTER_API_KEY");
  assert.match(entry?.description ?? "", /security teams/);
  assert.match(entry?.docsUrl ?? "", /orcarouter\.ai\/cyber/);
  // Every built-in provider keeps its own name — OrcaRouter is not a generic base-URL option.
  assert.deepEqual(
    PROVIDER_REGISTRY.map((p) => p.id),
    ["ollama", "anthropic", "openai", "orcarouter", "echo"],
  );
});
