/**
 * Cross-language end-to-end check. Reads an index produced by the
 * Rust observer (`brain0 ingest`), backfills embeddings, builds the GUI snapshot, and runs
 * the internal agent's debug — verifying the whole observer→storage→search→agent pipeline.
 *
 * Usage: node --experimental-sqlite dist/e2e.js <db> <payload> <repo> <query>
 */

import assert from "node:assert/strict";
import { Brain0Store, FsPayloadReader, buildGraphSnapshot } from "@brain0/shared";
import { Brain0Agent, EchoLLM, LocalEmbeddingProvider, backfillEmbeddings, type GetText } from "@brain0/agent";

const [db, payloadDir, repo, query] = process.argv.slice(2);
if (!db || !payloadDir || !repo || !query) {
  console.error("usage: e2e <db> <payload> <repo> <query>");
  process.exit(2);
}

const store = new Brain0Store(db);
const payload = new FsPayloadReader(payloadDir);
const getText: GetText = (ref) => payload.getText(ref);
const embedder = new LocalEmbeddingProvider(256);

const indexed = await backfillEmbeddings(store, getText, embedder);
const snapshot = buildGraphSnapshot(store, repo);
const agent = new Brain0Agent({ store, embedder, llm: new EchoLLM(), getText });
const result = await agent.debug(query);

console.log(`E2E: indexed ${indexed} embedding(s); snapshot ${snapshot.nodes.length} nodes / ${snapshot.edges.length} edges`);
console.log(`E2E: highlighted tasks=${JSON.stringify(result.highlights.tasks)} artifacts=${result.highlights.artifacts.length}`);

assert.ok(snapshot.nodes.length >= 4, "expected the repo→module→file→symbol hierarchy");
assert.ok(indexed >= 1, "expected at least one embedding indexed from the observed commits");
assert.ok(result.highlights.tasks.length >= 1, "expected the agent to surface an intent");
assert.ok(result.highlights.artifacts.length >= 1, "expected the agent to surface code by reference");

console.log("E2E OK");
store.close();
