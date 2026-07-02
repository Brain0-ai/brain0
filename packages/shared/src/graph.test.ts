import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store, buildGraphSnapshot } from "./index.js";

test("buildGraphSnapshot draws only commits as intent nodes (agent prompts are not graph nodes)", () => {
  const store = new Brain0Store(":memory:");
  store.migrate();
  const db = store.raw();

  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?)",
  ).run("art_file", "file", "repo", "m.py", "v", 0, 0);
  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, parent_id, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?,?)",
  ).run("art_sym", "symbol", "repo", "m.py::f", "art_file", "v", 0.1, 0.9);

  // A commit/observer task (no source adapter; session id = commit SHA) and an agent-prompt task
  // (source adapter set). Both modify the same symbol.
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) VALUES (?,?,?,?,?,?)",
  ).run("tsk_commit", "abc123sha", "human", "Nicola", "2026-06-06T00:00:00Z", "v");
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version, source_adapter) VALUES (?,?,?,?,?,?,?)",
  ).run("tsk_agent", "sess-1", "claude-code", "agent", "2026-06-06T00:00:00Z", "v", "claude-code");

  const contains = JSON.stringify({ kind: "artifact_contains", parent: "art_file", child: "art_sym" });
  db.prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)").run(
    "artifact_contains",
    "art_file",
    "art_sym",
    contains,
  );
  for (const task of ["tsk_commit", "tsk_agent"]) {
    const modifies = JSON.stringify({
      kind: "task_modifies_artifact",
      task,
      artifact: "art_sym",
      version: "v",
      change_kind: { kind: "modified" },
      lines_added: 1,
      lines_removed: 0,
    });
    db.prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)").run(
      "task_modifies_artifact",
      task,
      "art_sym",
      modifies,
    );
  }

  const snapshot = buildGraphSnapshot(store, "repo");

  const artifacts = snapshot.nodes.filter((n) => n.kind === "artifact");
  const tasks = snapshot.nodes.filter((n) => n.kind === "task");
  assert.equal(artifacts.length, 2);
  // Only the commit is a graph node; the agent prompt is not.
  assert.equal(tasks.length, 1);
  assert.equal(tasks[0]?.id, "tsk_commit");
  assert.equal(tasks[0]?.author, "Nicola");
  assert.equal(tasks[0]?.ref, "abc123sha"); // the commit SHA, for the lite header

  // The gold-signal symbol is colored toward red.
  const sym = snapshot.nodes.find((n) => n.id === "art_sym");
  assert.ok(sym?.colorHex);

  assert.ok(snapshot.edges.some((e) => e.kind === "artifact_contains"));
  // The drawn modifies-edge comes from the commit; the agent prompt's edge is not drawn.
  const modifiesEdges = snapshot.edges.filter((e) => e.kind === "task_modifies_artifact");
  assert.equal(modifiesEdges.length, 1);
  assert.equal(modifiesEdges[0]?.src, "tsk_commit");
  store.close();
});
