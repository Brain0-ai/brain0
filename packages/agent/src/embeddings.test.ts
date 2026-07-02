import { test } from "node:test";
import assert from "node:assert/strict";
import { LocalEmbeddingProvider, localEmbed } from "./index.js";

test("local embeddings are deterministic and unit-normalized", async () => {
  const a = localEmbed("fix the parser bug", 256);
  const b = localEmbed("fix the parser bug", 256);
  assert.deepEqual(a, b);
  const norm = Math.sqrt(a.reduce((s, x) => s + x * x, 0));
  assert.ok(Math.abs(norm - 1) < 1e-6);
});

function cos(a: number[], b: number[]): number {
  let d = 0;
  for (let i = 0; i < a.length; i++) d += (a[i] ?? 0) * (b[i] ?? 0);
  return d; // both unit-normalized
}

test("golden vector matches the Rust local_embed (shared vector space)", () => {
  // Must equal the Rust `local_embed_golden` test in crates/brain0-storage.
  const v = localEmbed("brain zero", 8).map((x) => Math.round(x * 1000) / 1000);
  assert.deepEqual(v, [0, 0, 0, -1, 0, 0, 0, 0]);
});

test("similar text is closer than unrelated text", async () => {
  const provider = new LocalEmbeddingProvider(256);
  const q = await provider.embed("fix the parser bug");
  const similar = await provider.embed("parser bug fix needed");
  const unrelated = await provider.embed("banana smoothie recipe");
  assert.ok(cos(q, similar) > cos(q, unrelated));
});
