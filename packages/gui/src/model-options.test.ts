import { test } from "node:test";
import assert from "node:assert/strict";
import type { ModelOption } from "./model-options.js";
import { catalogNote, chatModelOptions, reconcileSelection } from "./model-options.js";

const text: ModelOption = {
  id: "orcarouter/auto",
  name: "OrcaRouter Auto",
  inputModalities: ["text"],
  supportedEndpointTypes: ["openai"],
};
const vision: ModelOption = {
  id: "orca/orcacyber-ultra-1.0",
  name: "OrcaCyber Ultra",
  contextLength: 200_000,
  inputModalities: ["text", "image"],
  supportedEndpointTypes: ["openai", "anthropic"],
};
const embedding: ModelOption = {
  id: "openai/text-embedding-3-large",
  name: "Embedding",
  inputModalities: ["text"],
  supportedEndpointTypes: ["embeddings"],
};
const imageGen: ModelOption = {
  id: "openai/gpt-image-1",
  name: "Image",
  inputModalities: ["text"],
  supportedEndpointTypes: ["image-generation"],
};

const catalog = (models: ModelOption[], degraded = false) => ({
  source: (degraded ? "fallback" : "live") as "live" | "fallback",
  degraded,
  models,
});

test("the chat selector never offers embedding or image-generation routes", () => {
  const options = chatModelOptions(catalog([text, vision, embedding, imageGen]));
  assert.deepEqual(options.map((o) => o.id), ["orcarouter/auto", "orca/orcacyber-ultra-1.0"]);
});

test("adding an image attachment leaves only models that declare image input", () => {
  const options = chatModelOptions(catalog([text, vision]), "image");
  assert.deepEqual(options.map((o) => o.id), ["orca/orcacyber-ultra-1.0"]);
  // A text-only model is not guessed into the multimodal list.
  assert.ok(!options.some((o) => o.id === "orcarouter/auto"));
});

test("a selection that is no longer compatible is replaced, not silently kept", () => {
  const before = reconcileSelection("orcarouter/auto", chatModelOptions(catalog([text, vision])));
  assert.equal(before.changed, false);

  const afterImage = reconcileSelection("orcarouter/auto", chatModelOptions(catalog([text, vision]), "image"));
  assert.equal(afterImage.changed, true);
  assert.equal(afterImage.model, "orca/orcacyber-ultra-1.0");

  // Preference wins when it survives the filter; otherwise the first option.
  const preferred = reconcileSelection("gone/model", chatModelOptions(catalog([text, vision])), "orcarouter/auto");
  assert.equal(preferred.model, "orcarouter/auto");
  assert.equal(reconcileSelection("gone/model", []).model, "");
});

test("an empty catalog yields no free-text fallback and an explicit note", () => {
  assert.deepEqual(chatModelOptions(undefined), []);
  assert.deepEqual(chatModelOptions(catalog([])), []);
  assert.match(catalogNote(undefined, 0), /unavailable/);
  assert.match(catalogNote(catalog([], true), 0), /verified fallback/);
  assert.equal(catalogNote(catalog([text, vision]), 2), "2 compatible chat models from the live catalog.");
});
