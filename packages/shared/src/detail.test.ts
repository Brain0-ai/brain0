import { test } from "node:test";
import assert from "node:assert/strict";
import { Brain0Store } from "./index.js";
import { buildNodeDetail } from "./detail.js";

function seed(): Brain0Store {
  const store = new Brain0Store(":memory:");
  store.migrate();
  const db = store.raw();
  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?)",
  ).run("art_file", "file", "repo", "src/m.py", "v1", 0.1, 0);
  db.prepare(
    "INSERT INTO artifact_versions (id, artifact_id, timestamp, author_name, author_email, agent_name, agent_version, source_kind, source_ref, qualified_path, fingerprint, change_kind, lines_added, lines_removed, diff_ref) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
  ).run(
    "v1", "art_file", "2026-06-06T10:00:00Z", "Nicola", "n@x", "human", "", "git", "abcdef123456",
    "src/m.py", "fp", "modified", 12, 3, "ref_diff",
  );
  // A symbol inside the file: stores no own diff (parser-derived), parent = the file.
  db.prepare(
    "INSERT INTO artifact_nodes (id, level, repo, qualified_path, parent_id, current_version, risk_apriori, risk_aposteriori) VALUES (?,?,?,?,?,?,?,?)",
  ).run("art_sym", "symbol", "repo", "src/m.py::f", "art_file", "sv1", 0.1, 0);
  db.prepare(
    "INSERT INTO artifact_versions (id, artifact_id, timestamp, author_name, author_email, agent_name, agent_version, source_kind, source_ref, qualified_path, fingerprint, change_kind, lines_added, lines_removed, diff_ref) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
  ).run(
    "sv1", "art_sym", "2026-06-06T10:00:00Z", "Nicola", "n@x", "human", "", "git", "abcdef123456",
    "src/m.py::f", "fp2", "added", 12, 0, null,
  );

  // An agent-prompt task (source adapter set) that modified the file.
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version, source_adapter) VALUES (?,?,?,?,?,?,?)",
  ).run("tsk_agent", "s1", "claude-code", "agent", "2026-06-06T09:00:00Z", "tv1", "claude-code");
  db.prepare(
    "INSERT INTO task_versions (id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json, reads_json, read_secrets_json) VALUES (?,?,?,?,?,?,?,?,?)",
  ).run(
    "tv1", "tsk_agent", "2026-06-06T09:00:00Z", "ref_prompt", "ref_summary",
    JSON.stringify([{ path: "src/m.py" }]),
    JSON.stringify({ undeclared: ["src/other.py"], phantom: [] }),
    // Files this session READ (audit) — incl. an out-of-repo secret.
    JSON.stringify(["src/secret.ts", "/home/dev/.env"]),
    // DLP: the .env read's content held a secret (kind only).
    JSON.stringify([{ path: "/home/dev/.env", kinds: ["env_secret"] }]),
  );

  // A commit/observer task (no source adapter; session id = commit SHA) that changed the file.
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version) VALUES (?,?,?,?,?,?)",
  ).run("tsk_commit", "abcdef123456", "human", "Nicola", "2026-06-06T10:00:00Z", "cv1");
  db.prepare(
    "INSERT INTO task_versions (id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json) VALUES (?,?,?,?,?,?,?)",
  ).run("cv1", "tsk_commit", "2026-06-06T10:00:00Z", null, "ref_msg", "[]", null);

  // The agent intent and the commit both modified the file. Crucially, the reconciler correlated
  // the agent turn to the SAME observed version the commit produced ("v1") — that shared version
  // is what ties the prompt to this commit.
  db.prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)").run(
    "task_modifies_artifact", "tsk_agent", "art_file",
    JSON.stringify({ kind: "task_modifies_artifact", task: "tsk_agent", artifact: "art_file", version: "v1" }),
  );
  db.prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)").run(
    "task_modifies_artifact", "tsk_commit", "art_file",
    JSON.stringify({ kind: "task_modifies_artifact", task: "tsk_commit", artifact: "art_file", version: "v1" }),
  );

  // A STALE agent session that also touched the same file, but in a DIFFERENT (earlier) version
  // "v_old" this commit did NOT produce — i.e. a week-old session. It must NOT appear behind this
  // commit (the bug attached it via the shared file regardless of version).
  db.prepare(
    "INSERT INTO task_nodes (id, session_id, agent_name, author_name, created_at, current_version, source_adapter) VALUES (?,?,?,?,?,?,?)",
  ).run("tsk_stale", "s_old", "claude-code", "agent", "2026-06-01T09:00:00Z", "tvold", "claude-code");
  db.prepare(
    "INSERT INTO task_versions (id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json, reads_json) VALUES (?,?,?,?,?,?,?,?)",
  ).run(
    "tvold", "tsk_stale", "2026-06-01T09:00:00Z", null, "ref_summary",
    JSON.stringify([{ path: "src/m.py" }]), null,
    JSON.stringify(["src/stale-only.ts"]),
  );
  db.prepare("INSERT INTO edges (kind, src, dst, attrs_json) VALUES (?,?,?,?)").run(
    "task_modifies_artifact", "tsk_stale", "art_file",
    JSON.stringify({ kind: "task_modifies_artifact", task: "tsk_stale", artifact: "art_file", version: "v_old" }),
  );
  return store;
}

const getText = async (ref: string): Promise<string | undefined> =>
  ({ ref_prompt: "fix the bug", ref_summary: "fixed it", ref_msg: "first commit" })[ref];

test("buildNodeDetail gives an artifact's history (lazy diff ref) + real intent authorship", async () => {
  const store = seed();
  const d = await buildNodeDetail(store, getText, "art_file");
  assert.ok(d);
  assert.equal(d.kind, "artifact");
  assert.equal(d.path, "src/m.py");
  const v = d.versions[0]!;
  assert.equal(v.changeKind, "modified");
  assert.equal(v.linesAdded, 12);
  assert.equal(v.committer, "Nicola"); // git committer, not an assumed "human" agent
  assert.equal(v.source, "git:abcdef1234");
  assert.equal(v.diffRef, "ref_diff"); // reference only — diff is lazy-loaded
  // The true authors are the linked agent intents only — the commit task is the committer, not
  // listed. Both agent sessions that touched this file appear (a file's authorship is all-time).
  assert.equal(d.intents?.length, 2);
  assert.equal(d.intents?.[0]?.agent, "claude-code");
});

test("buildNodeDetail falls back to the containing file's diff for a symbol", async () => {
  const store = seed();
  const d = await buildNodeDetail(store, getText, "art_sym");
  assert.ok(d);
  assert.equal(d.kind, "artifact");
  const v = d.versions[0]!;
  assert.equal(v.diffRef, "ref_diff"); // the parent file's diff for the same commit
  assert.equal(v.diffOfPath, "src/m.py"); // labeled as the containing file
});

test("buildNodeDetail gives an agent task its summary, declared + drift (no raw prompt)", async () => {
  const store = seed();
  const d = await buildNodeDetail(store, getText, "tsk_agent");
  assert.ok(d);
  assert.equal(d.kind, "task");
  assert.equal(d.agent, "claude-code");
  const v = d.versions[0]!;
  assert.equal(v.summary, "fixed it");
  assert.deepEqual(v.declared, ["src/m.py"]);
  assert.deepEqual(v.driftUndeclared, ["src/other.py"]);
});

test("buildNodeDetail builds a commit detail: message, changed files, and the prompts behind it", async () => {
  const store = seed();
  const d = await buildNodeDetail(store, getText, "tsk_commit");
  assert.ok(d);
  assert.equal(d.kind, "commit");
  assert.equal(d.message, "first commit");
  assert.equal(d.source, "git:abcdef1234");
  // Files changed (git-style), with per-file facts and a lazy diff ref.
  assert.equal(d.changedFiles?.length, 1);
  const f = d.changedFiles![0]!;
  assert.equal(f.path, "src/m.py");
  assert.equal(f.changeKind, "modified");
  assert.equal(f.linesAdded, 12);
  assert.equal(f.diffRef, "ref_diff");
  // Prompts behind the commit = agent tasks the reconciler tied to a version THIS commit produced
  // (shared-version join), summaries only — NOT every session that ever touched the file.
  assert.equal(d.prompts?.length, 1);
  const p = d.prompts![0]!;
  assert.equal(p.taskId, "tsk_agent");
  assert.equal(p.summary, "fixed it");
  assert.equal(p.agent, "claude-code");
  // DLP surfaces on the commit card: the secret-bearing read with its kinds, never values.
  assert.deepEqual(d.readSecrets, [{ path: "/home/dev/.env", kinds: ["env_secret"] }]);
  // The week-old session (same file, different version) is correctly excluded.
  assert.ok(!d.prompts?.some((q) => q.taskId === "tsk_stale"));
  // Audit: files READ by the joined session surface here (incl. the out-of-repo secret), sorted;
  // the excluded stale session's reads do NOT leak in.
  assert.deepEqual(d.reads, ["/home/dev/.env", "src/secret.ts"]);
  assert.ok(!d.reads?.includes("src/stale-only.ts"));
});

test("buildNodeDetail flags encrypted (unreadable) task payload instead of garbage", async () => {
  const store = seed();
  const encGetText = async (): Promise<string> => "��\x01\x02�\x00garbage��";
  const d = await buildNodeDetail(store, encGetText, "tsk_agent");
  assert.ok(d);
  assert.equal(d.versions[0]!.summary, undefined);
  assert.match(d.note ?? "", /encrypted/);
});

test("buildNodeDetail returns undefined for an unknown id", async () => {
  const store = seed();
  assert.equal(await buildNodeDetail(store, getText, "nope"), undefined);
});
