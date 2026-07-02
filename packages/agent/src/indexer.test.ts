import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store } from "@brain0/shared";
import { LocalEmbeddingProvider, backfillEmbeddings } from "./index.js";

test("indexer backfills embeddings from prompt payloads", async () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  const db = store.raw();
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) VALUES (?,?,?,?,?,?)",
  ).run("tsk_1", "s", "claude-code", "Ada", "2026-06-06T10:00:00Z", "ver_1");
  db.prepare(
    "INSERT INTO task_versions (id, task_id, timestamp, prompt_ref) VALUES (?,?,?,?)",
  ).run("ver_1", "tsk_1", "2026-06-06T10:00:00Z", "blake3:p1");

  // Payload hydration stub.
  const texts: Record<string, string> = { "blake3:p1": "fix the parser bug" };
  const getText = (ref: string) => Promise.resolve(texts[ref]);

  assert.deepEqual(store.tasksMissingEmbeddings(), ["tsk_1"]);
  const n = await backfillEmbeddings(store, getText, new LocalEmbeddingProvider(64));
  assert.equal(n, 1);
  assert.deepEqual(store.tasksMissingEmbeddings(), []);

  // The new embedding is searchable.
  const provider = new LocalEmbeddingProvider(64);
  const query = await provider.embed("parser bug");
  const hits = store.searchTasksByVector(query, 5);
  assert.equal(hits[0]?.taskId, "tsk_1");
  store.close();
});
