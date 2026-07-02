import { test } from "node:test";
import assert from "node:assert/strict";
import { sep } from "node:path";
import { cacheControlFor, contentTypeFor, safeJoin } from "./staticfiles.js";

test("contentTypeFor maps the vite build outputs", () => {
  assert.equal(contentTypeFor("/x/index.html"), "text/html; charset=utf-8");
  assert.equal(contentTypeFor("/x/assets/index-abc.js"), "text/javascript; charset=utf-8");
  assert.equal(contentTypeFor("/x/assets/index-abc.css"), "text/css; charset=utf-8");
  assert.equal(contentTypeFor("/x/brain0-logo.svg"), "image/svg+xml");
  assert.equal(contentTypeFor("/x/assets/index.js.map"), "application/json");
  assert.equal(contentTypeFor("/x/unknown.bin"), "application/octet-stream");
  assert.equal(contentTypeFor("/x/noext"), "application/octet-stream");
});

test("safeJoin resolves inside the root and refuses traversal", () => {
  const root = `${sep}srv${sep}gui`;
  assert.equal(safeJoin(root, "/index.html"), `${root}${sep}index.html`);
  assert.equal(safeJoin(root, "/assets/a.js"), `${root}${sep}assets${sep}a.js`);
  // Strict policy: ANY `..` segment is refused (encoded or not), plus NUL and bad encodings.
  assert.equal(safeJoin(root, "/../etc/passwd"), undefined);
  assert.equal(safeJoin(root, "/%2e%2e/etc/passwd"), undefined);
  assert.equal(safeJoin(root, "/a/../../etc/passwd"), undefined);
  assert.equal(safeJoin(root, "/a/../index.html"), undefined);
  assert.equal(safeJoin(root, "/%00.html"), undefined);
  assert.equal(safeJoin(root, "/%zz"), undefined); // malformed encoding
  // Dotfiles and dot-prefixed names that are NOT ".." remain reachable.
  assert.equal(safeJoin(root, "/.well-known/x"), `${root}${sep}.well-known${sep}x`);
});

test("cacheControlFor: immutable for hashed assets, no-cache elsewhere", () => {
  assert.equal(cacheControlFor("/assets/index-abc.js"), "public, max-age=31536000, immutable");
  assert.equal(cacheControlFor("/index.html"), "no-cache");
  assert.equal(cacheControlFor("/"), "no-cache");
});
