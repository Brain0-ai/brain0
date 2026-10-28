import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MAX_CATALOG_ITEMS,
  ORCAROUTER_SEED_CATALOG,
  discoverModels,
  filterChatModels,
  parseCatalogBody,
  parseCatalogModel,
  seedChatModels,
} from "./models-catalog.js";

// Fixtures mirror the real `/v1/models` envelope: a flat `data[]` with `supported_endpoint_types`
// and `architecture.input_modalities`. Capability is never guessed from the model name.
const TEXT_ONLY = {
  id: "deepseek/deepseek-v4-pro",
  name: "DeepSeek V4 Pro",
  context_length: 160_000,
  architecture: { input_modalities: ["text"] },
  supported_endpoint_types: ["openai"],
};

const IMAGE_INPUT_CHAT = {
  id: "anthropic/claude-opus-4.8",
  name: "Claude Opus 4.8",
  context_length: 200_000,
  architecture: { input_modalities: ["text", "image"] },
  supported_endpoint_types: ["anthropic", "openai"],
};

const EMBEDDING = {
  id: "openai/text-embedding-3-large",
  name: "Text Embedding 3 Large",
  architecture: { input_modalities: ["text"] },
  supported_endpoint_types: ["embeddings"],
};

const IMAGE_GENERATION = {
  id: "openai/gpt-image-1",
  name: "GPT Image 1",
  architecture: { input_modalities: ["text"] },
  supported_endpoint_types: ["image-generation"],
};

const VIDEO = {
  id: "google/veo-3",
  name: "Veo 3",
  architecture: { input_modalities: ["text", "image"] },
  supported_endpoint_types: ["openai-video"],
};

const RERANK = {
  id: "jina/jina-reranker-v3",
  name: "Jina Reranker v3",
  architecture: { input_modalities: ["text"] },
  supported_endpoint_types: ["jina-rerank"],
};

const ALL = [TEXT_ONLY, IMAGE_INPUT_CHAT, EMBEDDING, IMAGE_GENERATION, VIDEO, RERANK];

test("the chat filter keeps text chat models and drops every non-text endpoint", () => {
  const parsed = parseCatalogBody({ object: "list", data: ALL });
  const chat = filterChatModels(parsed).map((m) => m.id);
  assert.deepEqual(chat, ["deepseek/deepseek-v4-pro", "anthropic/claude-opus-4.8"]);
  // Explicitly excludes the four non-chat shapes.
  for (const id of ["openai/text-embedding-3-large", "openai/gpt-image-1", "google/veo-3", "jina/jina-reranker-v3"]) {
    assert.ok(!chat.includes(id), `${id} must not be a chat option`);
  }
});

test("an undeclared modality fails closed for a multimodal filter", () => {
  const parsed = parseCatalogBody({ data: ALL });
  const vision = filterChatModels(parsed, "image").map((m) => m.id);
  assert.deepEqual(vision, ["anthropic/claude-opus-4.8"]);
  // A text-only chat model is dropped, never assumed to accept an image.
  assert.ok(!vision.includes("deepseek/deepseek-v4-pro"));
});

test("a model advertised with no endpoint types is unusable, not guessed", () => {
  const chat = filterChatModels(parseCatalogBody({ data: [{ id: "mystery/model" }] }));
  assert.equal(chat.length, 0);
  assert.equal(parseCatalogModel({ id: "" }), undefined);
  assert.equal(parseCatalogModel({ id: "x" }), undefined);
  assert.equal(parseCatalogModel(null), undefined);
});

test("parsing keeps capability metadata and is bounded", () => {
  const parsed = parseCatalogBody({
    data: [
      { ...IMAGE_INPUT_CHAT, supported_parameters: ["tools", "reasoning_effort"] },
      ...Array.from({ length: MAX_CATALOG_ITEMS + 50 }, (_, i) => ({ ...TEXT_ONLY, id: `x/m${i}` })),
    ],
  });
  assert.equal(parsed.length, MAX_CATALOG_ITEMS);
  const claude = parsed.find((m) => m.id === "anthropic/claude-opus-4.8");
  assert.deepEqual(claude?.inputModalities, ["text", "image"]);
  assert.equal(claude?.contextLength, 200_000);
  assert.deepEqual(claude?.reasoningEfforts, ["low", "medium", "high", "xhigh"]);
});

test("the verified seed only contains compatible chat models with real metadata", () => {
  const seed = seedChatModels();
  const ids = seed.map((m) => m.id);
  assert.deepEqual(ids, [
    "openai/gpt-5.5",
    "anthropic/claude-opus-4.8",
    "google/gemini-3.5-flash",
    "deepseek/deepseek-v4-pro",
    "orcarouter/auto",
    "orcarouter/fusion",
  ]);
  const gpt = seed.find((m) => m.id === "openai/gpt-5.5");
  assert.deepEqual(gpt?.reasoningEfforts, ["low", "medium", "high", "xhigh"]);
  assert.ok(gpt?.inputModalities.includes("image"));
  // The seed is a frozen, verified list — not a copy of the live catalog.
  assert.equal(ORCAROUTER_SEED_CATALOG.length, seed.length);
});

test("a successful live fetch is authoritative — no seed entries are mixed in", async () => {
  const fetchImpl = (async () =>
    new Response(
      JSON.stringify({
        data: [
          { ...TEXT_ONLY, id: "acme/only-model" },
          EMBEDDING,
        ],
      }),
      { status: 200, headers: { "content-type": "application/json" } },
    )) as typeof fetch;
  const result = await discoverModels({ apiKey: "", fetchImpl });
  assert.equal(result.source, "live");
  assert.equal(result.degraded, false);
  assert.deepEqual(result.models.map((m) => m.id), ["acme/only-model"]);
});

test("a failed fetch degrades to the verified seed, with the reason surfaced", async () => {
  const fetchImpl = (async () => new Response("upstream unavailable", { status: 503 })) as typeof fetch;
  const result = await discoverModels({ apiKey: "sk-orca-fake", fetchImpl });
  assert.equal(result.source, "fallback");
  assert.equal(result.degraded, true);
  assert.match(result.error ?? "", /503/);
  assert.deepEqual(result.models.map((m) => m.id), seedChatModels().map((m) => m.id));
});

test("a network error and an empty catalog both fall back rather than yielding free text", async () => {
  const throwing = (async () => {
    throw new Error("connect ECONNREFUSED");
  }) as typeof fetch;
  const failed = await discoverModels({ fetchImpl: throwing });
  assert.equal(failed.source, "fallback");
  assert.match(failed.error ?? "", /ECONNREFUSED/);

  const empty = (async () => new Response(JSON.stringify({ data: [] }), { status: 200 })) as typeof fetch;
  const emptyResult = await discoverModels({ fetchImpl: empty });
  assert.equal(emptyResult.source, "fallback");
  assert.equal(emptyResult.degraded, true);
  assert.ok(emptyResult.models.length > 0);
});

test("the catalog request goes to the configured base with a Bearer key", async () => {
  const seen: Array<{ url: string; auth?: string }> = [];
  const fetchImpl = (async (url: string | URL | Request, init?: RequestInit) => {
    seen.push({
      url: String(url),
      auth: (init?.headers as Record<string, string> | undefined)?.authorization,
    });
    return new Response(JSON.stringify({ data: [TEXT_ONLY] }), { status: 200 });
  }) as unknown as typeof fetch;
  await discoverModels({ apiKey: "sk-orca-fake", fetchImpl });
  assert.equal(seen[0]?.url, "https://api.orcarouter.ai/v1/models?capability=chat");
  assert.equal(seen[0]?.auth, "Bearer sk-orca-fake");
});
