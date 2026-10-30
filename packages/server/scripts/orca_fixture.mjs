import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { Brain0Store, encodeVector } from "@brain0/shared";

const root = process.argv[2];
if (!root) {
  console.error("usage: fixture.mjs <out-dir>");
  process.exit(2);
}
mkdirSync(resolve(root, "payload"), { recursive: true });
const dbPath = resolve(root, "index.db");
const store = new Brain0Store(dbPath);
store.migrate();
const raw = store.raw();
const repo = "demo/brain0-demo";

const insertArtifact = raw.prepare(
  `INSERT INTO artifact_nodes (id, level, repo, qualified_path, lang, parent_id, current_version, risk_apriori, risk_aposteriori)
   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
);
const insertAv = raw.prepare(
  `INSERT INTO artifact_versions (id, artifact_id, timestamp, author_name, author_email, agent_name, agent_version,
     source_kind, source_ref, qualified_path, fingerprint, change_kind, change_from, lines_added, lines_removed, diff_ref)
   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
);
const insertTask = raw.prepare(
  `INSERT INTO task_nodes (id, session_id, agent_name, agent_version, author_name, author_email, created_at,
     current_version, source_adapter, session_cwd, model, reviewers_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
);
const insertTv = raw.prepare(
  `INSERT INTO task_versions (id, task_id, timestamp, prompt_ref, decision_summary_ref, declared_json, drift_json, reads_json, read_secrets_json)
   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
);
const insertEdge = raw.prepare("INSERT OR IGNORE INTO edges (kind, src, dst, attrs_json) VALUES (?, ?, ?, ?)");
const insertEmb = raw.prepare("INSERT OR REPLACE INTO task_embeddings (task_id, dim, vec) VALUES (?, ?, ?)");

const files = [
  ["src/index.ts", 0.72, "fix the retry loop in the graph client"],
  ["src/renderer.ts", 0.35, "add risk colouring to the node renderer"],
  ["src/provider.ts", 0.18, "wire a new model provider into the server"],
  ["src/store.ts", 0.44, "cache task lookups per snapshot"],
];
const modules = ["src"];
const base = Date.parse("2026-10-20T10:00:00Z");

for (const m of modules) {
  insertArtifact.run(`art-mod-${m}`, "module", repo, m, null, `art-repo`, `av-mod-${m}`, 0, 0);
  insertAv.run(`av-mod-${m}`, `art-mod-${m}`, new Date(base).toISOString(), "Demo Dev", "dev@example.com", "demo", "1",
    "git", "aaaa", m, "fp-mod", "modified", null, 4, 1, null);
  insertEdge.run("artifact_contains", "art-repo", `art-mod-${m}`, JSON.stringify({ kind: "artifact_contains", parent: "art-repo", child: `art-mod-${m}` }));
}
insertArtifact.run("art-repo", "repo", repo, repo, null, null, "av-repo", 0.1, 0.1);
insertAv.run("av-repo", "art-repo", new Date(base).toISOString(), "Demo Dev", "dev@example.com", "demo", "1",
  "git", "aaaa", repo, "fp-repo", "modified", null, 9, 2, null);

for (const [i, [path, risk, summary]] of files.entries()) {
  const fileId = `art-file-${i}`;
  const symId = `art-sym-${i}`;
  const ts = new Date(base + i * 3_600_000).toISOString();
  const sha = `c0ffee${i}abcdef`;
  insertArtifact.run(fileId, "file", repo, path, "typescript", "art-mod-src", `av-file-${i}`, risk, risk);
  insertAv.run(`av-file-${i}`, fileId, ts, "Demo Dev", "dev@example.com", "demo-agent", "1", "git", sha, path,
    `fp-file-${i}`, "modified", null, 10 + i, 2, `diff-${i}`);
  insertArtifact.run(symId, "symbol", repo, `${path}::handler${i}`, "typescript", fileId, `av-sym-${i}`, risk * 0.8, risk * 0.8);
  insertAv.run(`av-sym-${i}`, symId, ts, "Demo Dev", "dev@example.com", "demo-agent", "1", "git", sha,
    `${path}::handler${i}`, `fp-sym-${i}`, "modified", null, 6, 1, `diff-${i}`);
  insertEdge.run("artifact_contains", "art-mod-src", fileId, JSON.stringify({ kind: "artifact_contains", parent: "art-mod-src", child: fileId }));
  insertEdge.run("artifact_contains", fileId, symId, JSON.stringify({ kind: "artifact_contains", parent: fileId, child: symId }));

  const taskId = `task-commit-${i}`;
  insertTask.run(taskId, sha, "demo-agent", "1", "Demo Dev", "dev@example.com", ts, `tv-task-${i}`, null,
    "/demo", "orcarouter/auto", null);
  insertTv.run(`tv-task-${i}`, taskId, ts, `prompt-${i}`, null, JSON.stringify([{ path, kind: "modified" }]), null,
    JSON.stringify([path]), null);
  insertEdge.run("task_modifies_artifact", taskId, fileId, JSON.stringify({ kind: "task_modifies_artifact", task: taskId, artifact: fileId }));
  insertEmb.run(taskId, 256, encodeVector(Array.from({ length: 256 }, (_, k) => Math.sin(k + i) * 0.1)));
  writeFileSync(resolve(root, "payload", `diff-${i}`), `--- a/${path}\n+++ b/${path}\n@@ -1,2 +1,3 @@\n-${summary}\n+${summary}\n+// reviewed\n`);
  writeFileSync(resolve(root, "payload", `prompt-${i}`), summary);
}

store.close();
console.log("fixture written to", root);
