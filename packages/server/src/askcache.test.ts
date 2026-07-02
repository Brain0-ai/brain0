import { test } from "node:test";
import assert from "node:assert/strict";
import { BoundedCache, normalizeQuery } from "./askcache.js";

test("normalizeQuery folds case and whitespace", () => {
  assert.equal(normalizeQuery("  Why   did X\tbreak? "), "why did x break?");
});

test("BoundedCache evicts the oldest beyond max and refreshes on re-set", () => {
  const c = new BoundedCache<number>(2);
  c.set("a", 1);
  c.set("b", 2);
  c.set("a", 10); // refresh order: now b is oldest
  c.set("c", 3); // evicts b
  assert.equal(c.get("b"), undefined);
  assert.equal(c.get("a"), 10);
  assert.equal(c.get("c"), 3);
  assert.equal(c.size, 2);
});
