import { test } from "node:test";
import assert from "node:assert/strict";
import type { GraphSnapshot } from "@brain0/shared";
import { ForceLayout, highlightSet, lodLevelForZoom, visibleNodes } from "./graph.js";

const SNAPSHOT: GraphSnapshot = {
  repo: "repo",
  nodes: [
    { id: "art_repo", kind: "artifact", level: "repo", label: "repo" },
    { id: "art_mod", kind: "artifact", level: "module", label: "pkg" },
    { id: "art_file", kind: "artifact", level: "file", label: "pkg/m.py" },
    { id: "art_sym", kind: "artifact", level: "symbol", label: "pkg/m.py::f" },
    { id: "tsk_1", kind: "task", label: "tsk_1" },
  ],
  edges: [
    { kind: "artifact_contains", src: "art_file", dst: "art_sym" },
    { kind: "task_modifies_artifact", src: "tsk_1", dst: "art_sym" },
  ],
};

test("LOD selects the level by zoom", () => {
  assert.equal(lodLevelForZoom(0.3), "repo");
  assert.equal(lodLevelForZoom(0.9), "module");
  assert.equal(lodLevelForZoom(1.5), "file");
  assert.equal(lodLevelForZoom(5), "symbol");
});

test("visibleNodes shows the current level and tasks when zoomed in", () => {
  const zoomedOut = visibleNodes(SNAPSHOT, 0.3).map((n) => n.id);
  assert.deepEqual(zoomedOut, ["art_repo"]);
  const zoomedIn = visibleNodes(SNAPSHOT, 5).map((n) => n.id).sort();
  assert.deepEqual(zoomedIn, ["art_sym", "tsk_1"]);
});

test("highlightSet includes the task and the artifacts it modified", () => {
  const set = highlightSet(SNAPSHOT, ["tsk_1"]);
  assert.ok(set.has("tsk_1"));
  assert.ok(set.has("art_sym"));
  assert.ok(!set.has("art_file"));
});

test("force layout is deterministic and pulls connected nodes together", () => {
  const a = new ForceLayout(SNAPSHOT);
  const b = new ForceLayout(SNAPSHOT);
  a.run(200);
  b.run(200);
  // Deterministic.
  assert.deepEqual(a.position("art_sym"), b.position("art_sym"));
  // All positions finite.
  for (const node of a.nodes) {
    assert.ok(Number.isFinite(node.x) && Number.isFinite(node.y));
  }
  // Connected nodes end up reasonably close (spring + gravity).
  const p1 = a.position("art_file")!;
  const p2 = a.position("art_sym")!;
  const dist = Math.hypot(p1.x - p2.x, p1.y - p2.y);
  assert.ok(dist < 400, `connected nodes distance ${dist}`);
});

test("relax enforces a minimum distance between all nodes (no overlap in any view)", () => {
  const layout = new ForceLayout(SNAPSHOT);
  layout.run(200);
  const MIN = 120;
  layout.relax(MIN, 200);
  const ns = layout.nodes;
  for (let i = 0; i < ns.length; i++) {
    for (let j = i + 1; j < ns.length; j++) {
      const d = Math.hypot(ns[i]!.x - ns[j]!.x, ns[i]!.y - ns[j]!.y);
      assert.ok(d >= MIN - 1, `nodes ${i},${j} too close after relax: ${d.toFixed(1)}`);
    }
  }
});
