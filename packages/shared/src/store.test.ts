import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store, cosine, decodeVector, encodeVector } from "./index.js";

test("vector codec round-trips", () => {
  const v = [1.5, -2, 0, 3.25];
  const decoded = decodeVector(encodeVector(v));
  assert.deepEqual(decoded, v);
});

test("cosine basics", () => {
  assert.ok(Math.abs(cosine([1, 0], [1, 0]) - 1) < 1e-6);
  assert.ok(Math.abs(cosine([1, 0], [0, 1])) < 1e-6);
  assert.equal(cosine([1], [1, 2]), 0);
});

function seedTask(store: Brain0Store, id: string, createdAt: string): void {
  store
    .raw()
    .prepare(
      "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) " +
        "VALUES (?,?,?,?,?,?)",
    )
    .run(id, "sess", "claude-code", "Ada", createdAt, "ver_cur");
}

test("vector search ranks by cosine and joins task metadata", () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  seedTask(store, "tsk_a", "2026-06-06T10:00:00Z");
  seedTask(store, "tsk_b", "2026-06-06T11:00:00Z");
  seedTask(store, "tsk_c", "2026-06-06T12:00:00Z");
  store.putTaskEmbedding("tsk_a", [1, 0, 0]);
  store.putTaskEmbedding("tsk_b", [0, 1, 0]);
  store.putTaskEmbedding("tsk_c", [0.9, 0.1, 0]);

  const hits = store.searchTasksByVector([1, 0, 0], 2);
  assert.equal(hits.length, 2);
  assert.equal(hits[0]?.taskId, "tsk_a");
  assert.equal(hits[1]?.taskId, "tsk_c");
  assert.equal(hits[0]?.createdAt, "2026-06-06T10:00:00Z");

  assert.deepEqual(store.tasksMissingEmbeddings(), []);
  store.close();
});

test("reads task node and versions written via raw", () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  seedTask(store, "tsk_a", "2026-06-06T10:00:00Z");
  store
    .raw()
    .prepare(
      "INSERT INTO task_versions (id, task_id, timestamp, declared_json) VALUES (?,?,?,?)",
    )
    .run("ver_1", "tsk_a", "2026-06-06T10:00:00Z", JSON.stringify([{ path: "a.py" }]));

  const task = store.getTask("tsk_a");
  assert.equal(task?.agent.name, "claude-code");
  const versions = store.taskVersions("tsk_a");
  assert.equal(versions.length, 1);
  assert.equal(versions[0]?.declared[0]?.path, "a.py");
  store.close();
});
