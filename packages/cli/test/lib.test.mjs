import { test } from "node:test";
import assert from "node:assert/strict";
import { inferRepoId, parseArgs, REPO_RE, sanitizeRepoId } from "../src/lib.mjs";

test("inferRepoId parses the common git remote shapes", () => {
  assert.equal(inferRepoId("git@github.com:owner/repo.git", "x"), "owner/repo");
  assert.equal(inferRepoId("https://github.com/owner/repo.git", "x"), "owner/repo");
  assert.equal(inferRepoId("https://github.com/owner/repo", "x"), "owner/repo");
  assert.equal(inferRepoId("ssh://git@github.com/owner/repo.git", "x"), "owner/repo");
  // GitLab nesting keeps the last two segments (enough identity, no host noise).
  assert.equal(inferRepoId("https://gitlab.com/group/sub/repo.git", "x"), "sub/repo");
});

test("inferRepoId falls back to the directory name, sanitized", () => {
  assert.equal(inferRepoId("", "brain0"), "brain0");
  assert.equal(inferRepoId(undefined, "my project!"), "my-project-");
  assert.equal(inferRepoId("not a url at all", "fallback"), "fallback");
  assert.equal(inferRepoId("", ""), "repo");
});

test("inferRepoId output always satisfies REPO_RE", () => {
  for (const [url, dir] of [
    ["git@github.com:o/r.git", "d"],
    ["https://x.dev/a/b", "d"],
    ["", "spaced name à"],
    ["garbage", "--dashes"],
  ]) {
    const id = inferRepoId(url, dir);
    assert.ok(REPO_RE.test(id) && !id.startsWith("-"), `bad id: ${id}`);
  }
});

test("sanitizeRepoId strips illegal chars and leading dashes", () => {
  assert.equal(sanitizeRepoId("a b/c"), "a-b/c");
  assert.equal(sanitizeRepoId("---x"), "x");
  assert.equal(sanitizeRepoId("###"), "repo");
});

test("parseArgs splits flags, valued options and positionals", () => {
  const { flags, opts, positional } = parseArgs(
    ["up", "--path", "/tmp/x", "--no-open", "--port", "9000", "extra"],
    ["path", "port", "repo"],
  );
  assert.deepEqual(positional, ["up", "extra"]);
  assert.equal(opts.get("path"), "/tmp/x");
  assert.equal(opts.get("port"), "9000");
  assert.ok(flags.has("no-open"));
  assert.ok(!flags.has("path"));
});
