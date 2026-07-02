import { test } from "node:test";
import assert from "node:assert/strict";
import { recencyAwareRank } from "./index.js";

const NOW = new Date("2026-06-06T00:00:00Z");

test("at equal semantic relevance, the more recent change ranks first", () => {
  const ranked = recencyAwareRank(
    [
      { taskId: "old", cosine: 0.8, createdAt: "2026-01-01T00:00:00Z" },
      { taskId: "new", cosine: 0.8, createdAt: "2026-06-01T00:00:00Z" },
    ],
    { now: NOW },
  );
  assert.equal(ranked[0]?.taskId, "new");
});

test("strong semantic match still beats a slightly newer weak match", () => {
  const ranked = recencyAwareRank(
    [
      { taskId: "weak-new", cosine: 0.1, createdAt: "2026-06-05T00:00:00Z" },
      { taskId: "strong-old", cosine: 0.95, createdAt: "2026-05-01T00:00:00Z" },
    ],
    { now: NOW },
  );
  assert.equal(ranked[0]?.taskId, "strong-old");
});

test("risk contributes to the score", () => {
  const ranked = recencyAwareRank(
    [
      { taskId: "risky", cosine: 0.5, createdAt: "2026-06-01T00:00:00Z", risk: 1 },
      { taskId: "safe", cosine: 0.5, createdAt: "2026-06-01T00:00:00Z", risk: 0 },
    ],
    { now: NOW },
  );
  assert.equal(ranked[0]?.taskId, "risky");
});
