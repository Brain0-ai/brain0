import { test } from "node:test";
import assert from "node:assert/strict";
import {
  CredentialGenerations,
  ORCAROUTER_SEED_CATALOG,
  SessionCredentialSource,
  discoverModels,
  type CatalogResult,
} from "@brain0/agent";
import { createProviderApi, providerApiStatus } from "./providers.js";

const FAKE_KEY = "sk-orca-test-0000000000000000abcd";

function fakeDiscover(result: CatalogResult): typeof discoverModels {
  return (async () => result) as typeof discoverModels;
}

function liveCatalog(): CatalogResult {
  return {
    source: "live",
    degraded: false,
    models: [
      {
        id: "orcarouter/auto",
        name: "OrcaRouter Auto",
        inputModalities: ["text"],
        supportedEndpointTypes: ["openai"],
      },
      {
        id: "orca/orcacyber-ultra-1.0",
        name: "OrcaCyber Ultra",
        contextLength: 200_000,
        inputModalities: ["text", "image"],
        supportedEndpointTypes: ["openai", "anthropic"],
      },
    ],
  };
}

function harness(overrides: { credential?: string } = {}) {
  const session = new SessionCredentialSource();
  const generations = new CredentialGenerations();
  let model = "orcarouter/auto";
  let reloads = 0;
  let discoverCalls: Array<{ apiKey?: string; baseUrl?: string; capability?: string; requiredModality?: string }> = [];
  const env: Record<string, string | undefined> = {
    BRAIN0_LLM_PROVIDER: "orcarouter",
    ...(overrides.credential ? { ORCAROUTER_API_KEY: overrides.credential } : {}),
  };
  const api = createProviderApi({
    env,
    session,
    generations,
    active: () => ({ provider: "orcarouter", model }),
    applyModel: (m) => {
      model = m;
    },
    reload: async () => {
      reloads += 1;
      return { ok: true };
    },
    discover: (async (options: { apiKey?: string; baseUrl?: string; capability?: string; requiredModality?: string }) => {
      discoverCalls.push(options);
      return liveCatalog();
    }) as unknown as typeof discoverModels,
  });
  return { api, session, generations, env, model: () => model, reloads: () => reloads, discoverCalls };
}

test("provider status exposes the registry and a masked credential — never the key", async () => {
  const h = harness({ credential: FAKE_KEY });
  const status = await h.api.status();
  assert.equal(status.active.provider, "orcarouter");
  assert.equal(status.orcarouter.displayName, "OrcaRouter Cyber Inference");
  assert.equal(status.orcarouter.endpoint, "https://api.orcarouter.ai/v1");
  assert.equal(status.orcarouter.defaultModel, "orcarouter/auto");
  assert.equal(status.orcarouter.fallbackModel, "orcarouter/fusion");
  assert.equal(status.orcarouter.credential.present, true);
  assert.equal(status.orcarouter.credential.masked, "sk-orca-…abcd");
  assert.ok(!JSON.stringify(status).includes(FAKE_KEY), "the raw key must never reach the GUI");
  assert.equal(status.orcarouter.catalog.source, "live");
  assert.equal(status.orcarouter.catalog.count, 2);
  assert.equal(status.orcarouter.links.cyber, "https://www.orcarouter.ai/cyber");
});

test("the catalog is fetched server-side with the resolved key", async () => {
  const h = harness({ credential: FAKE_KEY });
  const catalog = await h.api.models({ capability: "chat" });
  assert.equal(catalog.models.length, 2);
  assert.equal(h.discoverCalls[0]?.apiKey, FAKE_KEY);
  assert.equal(h.discoverCalls[0]?.baseUrl, "https://api.orcarouter.ai/v1");
  assert.equal(h.discoverCalls[0]?.capability, "chat");
  // The browser only receives metadata: ids, names and declared modalities.
  assert.equal(catalog.models[0]?.id, "orcarouter/auto");
  assert.equal(catalog.source, "live");
});

test("a multimodal request filters the catalog by declared input modality", async () => {
  const h = harness({ credential: FAKE_KEY });
  await h.api.models({ capability: "chat", modality: "image" });
  assert.equal(h.discoverCalls[0]?.requiredModality, "image");
});

test("a degraded catalog is reported as degraded instead of pretending to be live", async () => {
  const session = new SessionCredentialSource();
  const api = createProviderApi({
    env: { BRAIN0_LLM_PROVIDER: "orcarouter" },
    session,
    generations: new CredentialGenerations(),
    active: () => ({ provider: "orcarouter", model: "orcarouter/auto" }),
    applyModel: () => {},
    reload: async () => ({ ok: false }),
    discover: fakeDiscover({
      models: ORCAROUTER_SEED_CATALOG.map((m) => ({ ...m })),
      source: "fallback",
      degraded: true,
      error: "models endpoint returned 503",
    }),
  });
  const catalog = await api.models({ capability: "chat" });
  assert.equal(catalog.degraded, true);
  assert.match(catalog.error ?? "", /503/);
  assert.ok(catalog.models.length > 0, "the verified fallback keeps the picker usable");
  const status = await api.status();
  assert.equal(status.orcarouter.catalog.degraded, true);
});

test("saving a session key stores nothing in the browser and bumps the generation", async () => {
  const h = harness();
  assert.equal((await h.api.status()).orcarouter.credential.present, false);
  const saved = await h.api.saveKey(FAKE_KEY);
  assert.equal(saved.masked, "sk-orca-…abcd");
  assert.equal(saved.reachable, true);
  assert.equal(h.generations.current().generation, 1);
  assert.equal(h.reloads(), 1);
  const status = await h.api.status();
  assert.equal(status.orcarouter.credential.source, "session");
  assert.equal(status.orcarouter.credential.masked, "sk-orca-…abcd");

  const cleared = await h.api.clearKey();
  assert.equal(cleared.present, false);
  assert.equal(h.generations.current().generation, 2);
});

test("a malformed key is rejected with a 400-class error and is never stored", async () => {
  const h = harness();
  await assert.rejects(() => h.api.saveKey("sk-openai-nope"), /sk-orca/);
  assert.equal(h.session.read(), undefined);
  assert.equal(h.reloads(), 0);
  assert.equal(providerApiStatus(new Error("that does not look like an OrcaRouter key")), 400);
});

test("selecting a model requires it to be in the compatible catalog", async () => {
  const h = harness({ credential: FAKE_KEY });
  await h.api.models({ capability: "chat" });
  const chosen = await h.api.selectModel("orca/orcacyber-ultra-1.0");
  assert.equal(chosen.model, "orca/orcacyber-ultra-1.0");
  assert.equal(h.model(), "orca/orcacyber-ultra-1.0");
  await assert.rejects(() => h.api.selectModel("acme/not-listed"), /not in the compatible catalog/);
  assert.equal(providerApiStatus(new Error("model 'x' is not in the compatible catalog")), 400);
});

test("a 401 from the gateway marks the exact credential generation for reauthentication", async () => {
  const h = harness({ credential: FAKE_KEY });
  await h.api.status();
  const generation = h.generations.current().generation;
  h.api.noteFailure(generation, 401);
  const state = h.generations.current();
  assert.equal(state.needsReauth, true);
  assert.match(state.reason ?? "", /reconnect/);
  const status = await h.api.status();
  assert.equal(status.orcarouter.authState.needsReauth, true);

  // A 429 or a 5xx is not a credential problem.
  const clean = new CredentialGenerations();
  const api = createProviderApi({
    env: {},
    session: new SessionCredentialSource(),
    generations: clean,
    active: () => ({ provider: "orcarouter", model: "orcarouter/auto" }),
    applyModel: () => {},
    reload: async () => ({ ok: true }),
    discover: fakeDiscover(liveCatalog()),
  });
  api.noteFailure(0, 429);
  assert.equal(clean.current().needsReauth, false);

  // After a new key is installed, a stale failure for the old generation is ignored.
  clean.replace();
  api.noteFailure(0, 401);
  assert.equal(clean.current().needsReauth, false);
});
