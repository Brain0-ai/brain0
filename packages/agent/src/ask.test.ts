import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store } from "@brain0/shared";
import {
  Brain0Agent,
  EchoLLM,
  LocalEmbeddingProvider,
  NullLLMProvider,
  localEmbed,
  type EmbeddingProvider,
  type LlmMessage,
  type LLMProvider,
} from "./index.js";

const DIM = 64;

function seedTask(store: Brain0Store, id: string, reads: string[]): void {
  const db = store.raw();
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version, source_adapter) VALUES (?,?,?,?,?,?,?)",
  ).run(id, "s", "claude-code", "Ada", "2026-06-01T00:00:00Z", `${id}_v`, "claude-code");
  db.prepare(
    "INSERT INTO task_versions (id, task_id, timestamp, prompt_ref, reads_json) VALUES (?,?,?,?,?)",
  ).run(`${id}_v`, id, "2026-06-01T00:00:00Z", `blake3:${id}`, reads.length ? JSON.stringify(reads) : null);
}

function seedArtifact(store: Brain0Store, id: string, path: string, ap: number, post: number): void {
  const db = store.raw();
  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?)",
  ).run(id, "symbol", "repo", path, `${id}_v`, ap, post);
  db.prepare(
    "INSERT INTO artifact_versions (id, artifact_id, timestamp, author_name, agent_name, source_kind, source_ref, qualified_path, fingerprint, change_kind, lines_added, lines_removed) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
  ).run(`${id}_v`, id, "2026-06-01T00:00:00Z", "Ada", "claude-code", "git", "sha", path, "fp", "modified", 3, 1);
}

function link(store: Brain0Store, task: string, artifact: string): void {
  store
    .raw()
    .prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)")
    .run(
      "task_modifies_artifact",
      task,
      artifact,
      JSON.stringify({ kind: "task_modifies_artifact", task, artifact, version: `${artifact}_v` }),
    );
}

async function baseStore(): Promise<{ store: Brain0Store; embedder: LocalEmbeddingProvider }> {
  const store = new Brain0Store(":memory:");
  store.migrate();
  const embedder = new LocalEmbeddingProvider(DIM);
  seedTask(store, "tsk_sec", ["/home/dev/.aws/credentials", "src/util.ts"]);
  seedArtifact(store, "art_parser", "src/parse.py::tokenize", 0.1, 0.9); // gold
  link(store, "tsk_sec", "art_parser");
  store.putTaskEmbedding("tsk_sec", await embedder.embed("rewrite the parser tokenizer"));
  return { store, embedder };
}

const getText = (ref: string): Promise<string | undefined> => {
  const map: Record<string, string> = { "blake3:tsk_sec": "rewrite the parser tokenizer" };
  return Promise.resolve(map[ref]);
};

class FakeLLM implements LLMProvider {
  last = "";
  constructor(
    private readonly json: string,
    readonly descriptor = { name: "fake", model: "m", endpoint: "e", remote: false },
  ) {}
  complete(messages: LlmMessage[]): Promise<string> {
    this.last = messages.map((m) => m.content).join("\n");
    return Promise.resolve(this.json);
  }
}

class CapturingEmbedder implements EmbeddingProvider {
  readonly dimension = DIM;
  last = "";
  constructor(readonly descriptor = { name: "openai", remote: true }) {}
  embed(text: string): Promise<number[]> {
    this.last = text;
    return Promise.resolve(localEmbed(text, this.dimension));
  }
}

test("ask: EchoLLM falls back to retrieval-only highlights (no structured findings)", async () => {
  const { store, embedder } = await baseStore();
  const agent = new Brain0Agent({ store, embedder, llm: new EchoLLM(), getText });
  const r = await agent.ask("the parser tokenizer is broken");
  assert.ok(r.highlights.tasks.includes("tsk_sec"));
  assert.ok(r.highlights.artifacts.includes("art_parser"));
  assert.deepEqual(r.findings, []); // echo output isn't JSON → no findings, retrieval floor stays
  assert.equal(r.egress.zeroEgress, true);
  assert.equal(r.error, undefined);
  store.close();
});

test("ask: structured findings union retrieval and drop hallucinated ids", async () => {
  const { store, embedder } = await baseStore();
  const json = JSON.stringify({
    intent: "debug",
    highlights: [
      { id: "art_parser", kind: "artifact", reason: "root cause", severity: "critical", verdict: "regression" },
      { id: "ghost_id", kind: "artifact", reason: "x", severity: "warn", verdict: null },
    ],
    explanation: "the tokenizer change broke it",
  });
  const agent = new Brain0Agent({ store, embedder, llm: new FakeLLM(json), getText });
  const r = await agent.ask("why did the parser break");
  assert.equal(r.intent, "debug");
  assert.equal(r.findings.length, 1, "ghost id dropped");
  assert.equal(r.findings[0]!.id, "art_parser");
  assert.equal(r.findings[0]!.severity, "critical");
  assert.equal(r.findings[0]!.path, "src/parse.py::tokenize");
  assert.ok(r.highlights.artifacts.includes("art_parser"));
  store.close();
});

test("ask: REMOTE llm + embedder get redacted context; LOCAL gets it raw", async () => {
  const json = JSON.stringify({ intent: "audit", highlights: [], explanation: "ok" });
  const secretQuery = "did we leak AKIA1234567890ABCDEF ?";

  // Remote: secret + external read path must NOT reach either channel.
  {
    const { store } = await baseStore();
    const llm = new FakeLLM(json, { name: "openai", model: "m", endpoint: "e", remote: true });
    const emb = new CapturingEmbedder();
    const agent = new Brain0Agent({ store, embedder: emb, llm, getText, repo: "repo" });
    const r = await agent.ask(secretQuery);
    assert.ok(!llm.last.includes("AKIA1234567890ABCDEF"), "secret redacted from LLM");
    assert.ok(!llm.last.includes("/home/dev/.aws/credentials"), "external read path redacted from LLM");
    assert.ok(!emb.last.includes("AKIA1234567890ABCDEF"), "secret redacted from embedder");
    assert.equal(r.egress.redacted, true);
    assert.equal(r.egress.zeroEgress, false);
    store.close();
  }
  // Local: same context flows unredacted (zero egress, nothing left the machine).
  {
    const { store, embedder } = await baseStore();
    const llm = new FakeLLM(json); // local descriptor
    const agent = new Brain0Agent({ store, embedder, llm, getText, repo: "repo" });
    const r = await agent.ask(secretQuery);
    assert.ok(llm.last.includes("AKIA1234567890ABCDEF"), "local: query passes through");
    assert.ok(llm.last.includes("/home/dev/.aws/credentials"), "local: external read shown");
    assert.equal(r.egress.zeroEgress, true);
    store.close();
  }
});

test("ask: audit intent computes repo-wide distribution, not the retrieved subset", async () => {
  const { store, embedder } = await baseStore();
  seedArtifact(store, "art_green", "g.py::a", 0.0, 0.0);
  seedArtifact(store, "art_yellow", "y.py::b", 0.5, 0.0);
  // art_parser is red+gold. Repo-wide scan should see all three, though only art_parser was retrieved.
  const json = JSON.stringify({ intent: "audit", highlights: [], explanation: "audit" });
  const agent = new Brain0Agent({ store, embedder, llm: new FakeLLM(json), getText, repo: "repo" });
  const r = await agent.ask("audit what reached the model");
  assert.equal(r.intent, "audit");
  assert.ok(r.distribution, "distribution present");
  assert.equal(r.distribution!.green + r.distribution!.yellow + r.distribution!.red, 3, "repo-wide, not subset");
  assert.deepEqual(r.goldSignals, ["art_parser"]);
  store.close();
});

test("ask: a SHA in the query scopes to THAT commit, not fuzzy results", async () => {
  const { store, embedder } = await baseStore(); // seeds tsk_sec → art_parser (the fuzzy match)
  const db = store.raw();
  // A commit task identified by its git SHA (session_id), changing a distinct artifact.
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) VALUES (?,?,?,?,?,?)",
  ).run("tsk_commit", "a0c7c90bd19512ab993a8240fc26b2d8ce97aedc", "human", "Nicola", "2026-06-20T00:00:00Z", "cv1");
  db.prepare("INSERT INTO task_versions (id, task_id, timestamp) VALUES (?,?,?)").run(
    "cv1",
    "tsk_commit",
    "2026-06-20T00:00:00Z",
  );
  seedArtifact(store, "art_commit", "scripts/dev.mjs", 0.2, 0.0);
  link(store, "tsk_commit", "art_commit");

  const json = JSON.stringify({
    intent: "audit",
    highlights: [{ id: "art_commit", kind: "artifact", reason: "changed", severity: "info", verdict: null }],
    explanation: "commit a0c7c90 changed dev.mjs",
  });
  const agent = new Brain0Agent({ store, embedder, llm: new FakeLLM(json), getText, repo: "repo" });
  const r = await agent.ask("cosa ha modificato il commit a0c7c90bd19512ab993a8240fc26b2d8ce97aedc ?");
  assert.ok(r.highlights.tasks.includes("tsk_commit"), "scoped to the named commit");
  assert.ok(r.highlights.artifacts.includes("art_commit"), "its changed file is highlighted");
  assert.ok(!r.highlights.artifacts.includes("art_parser"), "unrelated fuzzy result excluded");
  store.close();
});

test("ask: no LLM reachable returns error + retrieval floor, never throws", async () => {
  const { store, embedder } = await baseStore();
  const agent = new Brain0Agent({ store, embedder, llm: new NullLLMProvider(), getText });
  const r = await agent.ask("the parser broke");
  assert.equal(r.error, "no-llm");
  assert.ok(r.highlights.artifacts.includes("art_parser"), "retrieval floor still highlights");
  assert.deepEqual(r.findings, []);
  assert.equal(r.egress.llm.name, "none");
  store.close();
});

test("hybrid retrieval: a query naming a file surfaces it even when embeddings miss", async () => {
  const { store, embedder } = await baseStore();
  // A second artifact+task whose embedding is about something unrelated.
  seedTask(store, "tsk_graph", []);
  seedArtifact(store, "art_graph", "packages/gui/src/graph.ts::ForceLayout.relax", 0.3, 0);
  link(store, "tsk_graph", "art_graph");
  store.putTaskEmbedding("tsk_graph", await embedder.embed("timeline slider colors"));

  const agent = new Brain0Agent({ store, embedder, llm: new EchoLLM(), getText, repo: "repo" });
  const r = await agent.ask("why is `graph.ts` risky?");
  assert.ok(r.highlights.artifacts.includes("art_graph"), `artifacts: ${r.highlights.artifacts}`);
  assert.ok(r.highlights.tasks.includes("tsk_graph"), `tasks: ${r.highlights.tasks}`);
});

test("retrieve: phase-1 gives highlights in ms with ZERO LLM involvement", async () => {
  const { store, embedder } = await baseStore();
  // NullLLMProvider throws on any completion — retrieve must never touch it.
  const agent = new Brain0Agent({ store, embedder, llm: new NullLLMProvider(), getText, repo: "repo" });
  const r = await agent.retrieve("the parser tokenizer is broken");
  assert.ok(r.highlights.tasks.includes("tsk_sec"));
  assert.ok(r.highlights.artifacts.includes("art_parser"));
  assert.equal(r.explanation, "");
  assert.equal(r.error, undefined);
});

test("evidence budget: long summaries are truncated before reaching the LLM", async () => {
  const { store, embedder } = await baseStore();
  const longText = `the parser tokenizer ${"x".repeat(900)}`;
  const longGetText = (): Promise<string | undefined> => Promise.resolve(longText);
  const fake = new FakeLLM("not-json");
  const agent = new Brain0Agent({ store, embedder, llm: fake, getText: longGetText });
  await agent.ask("the parser tokenizer is broken");
  assert.ok(fake.last.includes("… (truncated)"), "summary must be capped in the evidence");
  assert.ok(!fake.last.includes("x".repeat(600)), "the full 900-char tail must not be sent");
});
