import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store } from "@brain0/shared";
import { Brain0Agent, EchoLLM, LocalEmbeddingProvider } from "./index.js";

const DIM = 64;

function seedTask(store: Brain0Store, id: string, createdAt: string, promptRef: string): void {
  const db = store.raw();
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) VALUES (?,?,?,?,?,?)",
  ).run(id, "s", "claude-code", "Ada", createdAt, `${id}_v`);
  db.prepare("INSERT INTO task_versions (id, task_id, timestamp, prompt_ref) VALUES (?,?,?,?)").run(
    `${id}_v`,
    id,
    createdAt,
    promptRef,
  );
}

function seedArtifact(
  store: Brain0Store,
  id: string,
  path: string,
  apriori: number,
  aposteriori: number,
): void {
  const db = store.raw();
  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?)",
  ).run(id, "symbol", "repo", path, `${id}_v`, apriori, aposteriori);
  db.prepare(
    "INSERT INTO artifact_versions (id, artifact_id, timestamp, author_name, agent_name, source_kind, source_ref, qualified_path, fingerprint, change_kind, lines_added, lines_removed) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
  ).run(`${id}_v`, id, "2026-06-01T00:00:00Z", "Ada", "claude-code", "git", "sha", path, "fp", "modified", 3, 1);
}

function link(store: Brain0Store, task: string, artifact: string): void {
  const attrs = JSON.stringify({
    kind: "task_modifies_artifact",
    task,
    artifact,
    version: `${artifact}_v`,
    change_kind: { kind: "modified" },
    lines_added: 3,
    lines_removed: 1,
  });
  store
    .raw()
    .prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)")
    .run("task_modifies_artifact", task, artifact, attrs);
}

test("debug surfaces the most relevant+recent intent and works by reference", async () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  const embedder = new LocalEmbeddingProvider(DIM);

  seedTask(store, "tsk_new", "2026-06-01T00:00:00Z", "blake3:new");
  seedTask(store, "tsk_old", "2026-01-01T00:00:00Z", "blake3:old");
  seedArtifact(store, "art_parser", "src/parse.py::tokenize", 0.1, 0.9); // gold signal
  seedArtifact(store, "art_docs", "README.md", 0.0, 0.0);
  link(store, "tsk_new", "art_parser");
  link(store, "tsk_old", "art_docs");

  // Embeddings for the two tasks (same space as the query).
  store.putTaskEmbedding("tsk_new", await embedder.embed("rewrite the parser tokenizer"));
  store.putTaskEmbedding("tsk_old", await embedder.embed("update the readme documentation"));

  const texts: Record<string, string> = {
    "blake3:new": "rewrite the parser tokenizer",
    "blake3:old": "update the readme documentation",
  };
  let hydrations = 0;
  const getText = (ref: string) => {
    hydrations += 1;
    return Promise.resolve(texts[ref]);
  };

  const agent = new Brain0Agent({ store, embedder, llm: new EchoLLM(), getText, maxHydrate: 1 });
  const result = await agent.debug("the parser has a tokenizer bug");

  // The relevant, recent, risky intent is highlighted first.
  assert.equal(result.highlights.tasks[0], "tsk_new");
  assert.ok(result.highlights.artifacts.includes("art_parser"));
  // Operated by reference: only the single hydrated task's payload was fetched.
  assert.equal(hydrations, 1);
  assert.equal(result.ranked.length, 1);
  assert.ok(result.explanation.length > 0);
  store.close();
});

test("audit reports risk distribution and gold signals", async () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  seedArtifact(store, "art_a", "a.py::f", 0.0, 0.0); // green
  seedArtifact(store, "art_b", "b.py::g", 0.5, 0.0); // yellow
  seedArtifact(store, "art_c", "c.py::h", 0.1, 0.9); // red + gold signal

  const agent = new Brain0Agent({
    store,
    embedder: new LocalEmbeddingProvider(DIM),
    llm: new EchoLLM(),
  });
  const result = await agent.audit({ repo: "repo", level: "symbol" });

  assert.equal(result.distribution.green, 1);
  assert.equal(result.distribution.yellow, 1);
  assert.equal(result.distribution.red, 1);
  assert.deepEqual(result.goldSignals, ["art_c"]);
  assert.equal(result.topRisky[0]?.id, "art_c");
  assert.ok(result.explanation.length > 0);
  store.close();
});
