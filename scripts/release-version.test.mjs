// Tests for scripts/release-version.mjs — run with `node --test scripts/*.test.mjs`.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  bump,
  compare,
  latestTag,
  nextVersion,
  stampCargoLock,
  stampCargoToml,
  stampPackageJson,
} from "./release-version.mjs";

test("bump follows semver", () => {
  assert.equal(bump("0.1.3", "patch"), "0.1.4");
  assert.equal(bump("0.1.3", "minor"), "0.2.0");
  assert.equal(bump("0.1.3", "major"), "1.0.0");
  assert.throws(() => bump("0.1.3", "huge"), /unknown bump level/);
  assert.throws(() => bump("0.1", "patch"), /X\.Y\.Z/);
});

test("compare is numeric, not lexical", () => {
  assert.ok(compare("0.1.10", "0.1.9") > 0);
  assert.ok(compare("1.0.0", "0.99.99") > 0);
  assert.equal(compare("2.3.4", "2.3.4"), 0);
});

test("latestTag ignores pre-releases and foreign tags", () => {
  assert.equal(latestTag(["v0.1.0", "v0.1.10", "v0.1.9", "v0.2.0-rc.1", "release-9", "9.9.9"]), "0.1.10");
  assert.equal(latestTag([]), null);
});

test("next bumps the latest tag even when the manifests are stale", () => {
  const tags = ["v0.1.0", "v0.1.3", "v0.1.7"];
  assert.equal(nextVersion({ tags, current: "0.1.3", level: "patch" }), "0.1.8");
  assert.equal(nextVersion({ tags, current: "0.1.3", level: "minor" }), "0.2.0");
  assert.equal(nextVersion({ tags, current: "0.1.3", level: "major" }), "1.0.0");
});

test("next honours a version raised by hand in the manifests", () => {
  const tags = ["v0.1.3"];
  assert.equal(nextVersion({ tags, current: "0.2.0", level: "patch" }), "0.2.0");
  assert.equal(nextVersion({ tags, current: "0.1.4", level: "patch" }), "0.1.4");
  // ...but a label asking for more still wins.
  assert.equal(nextVersion({ tags, current: "0.1.4", level: "minor" }), "0.2.0");
  // A manifest version that was already released is bumped past.
  assert.equal(nextVersion({ tags: ["v0.2.0"], current: "0.2.0", level: "patch" }), "0.2.1");
});

test("next releases the manifest version when there is no tag yet", () => {
  assert.equal(nextVersion({ tags: [], current: "0.1.0", level: "minor" }), "0.1.0");
});

const CARGO_TOML = `[workspace]
resolver = "2"
members = ["crates/a"]

[workspace.package]
version = "0.1.3"
edition = "2021"
rust-version = "1.89"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
a = { path = "crates/a" }
`;

test("stampCargoToml rewrites only [workspace.package].version", () => {
  const out = stampCargoToml(CARGO_TOML, "0.2.0");
  assert.match(out, /\[workspace\.package\]\nversion = "0\.2\.0"\n/);
  assert.match(out, /rust-version = "1\.89"/);
  assert.match(out, /serde = \{ version = "1"/);
  assert.equal(out.replace('"0.2.0"', '"0.1.3"'), CARGO_TOML);
  assert.throws(() => stampCargoToml("[package]\nversion = \"1.0.0\"\n", "0.2.0"), /no \[workspace\.package\]/);
  assert.throws(() => stampCargoToml("[workspace.package]\nedition = \"2021\"\n", "0.2.0"), /has no version/);
});

const CARGO_LOCK = `version = 4

[[package]]
name = "brain0-cli"
version = "0.1.3"
dependencies = [
 "brain0-model",
 "serde",
]

[[package]]
name = "brain0-model"
version = "0.1.3"

[[package]]
name = "serde"
version = "0.1.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
`;

test("stampCargoLock rewrites workspace members and leaves registry packages alone", () => {
  const out = stampCargoLock(CARGO_LOCK, "0.2.0", new Set(["brain0-cli", "brain0-model"]));
  assert.match(out, /name = "brain0-cli"\nversion = "0\.2\.0"/);
  assert.match(out, /name = "brain0-model"\nversion = "0\.2\.0"/);
  assert.match(out, /name = "serde"\nversion = "0\.1\.3"\nsource/);
  assert.equal(out.replaceAll('"0.2.0"', '"0.1.3"'), CARGO_LOCK);
});

test("stampCargoLock fails when a member has no lock entry", () => {
  assert.throws(
    () => stampCargoLock(CARGO_LOCK, "0.2.0", new Set(["brain0-cli", "brain0-missing"])),
    /brain0-missing/,
  );
  // A registry package that shares a member's name does not count as the member.
  assert.throws(() => stampCargoLock(CARGO_LOCK, "0.2.0", new Set(["serde"])), /serde/);
});

test("stampPackageJson keeps the formatting and rewrites only the top-level version", () => {
  const pkg = '{\n  "name": "brain0",\n  "version": "0.1.3",\n  "description": "a \\u2014 b",\n  "devDependencies": { "x": "0.1.3" }\n}\n';
  const out = stampPackageJson(pkg, "0.2.0");
  assert.equal(out, pkg.replace('"version": "0.1.3"', '"version": "0.2.0"'));
  assert.equal(JSON.parse(out).devDependencies.x, "0.1.3");
  assert.throws(() => stampPackageJson('{ "name": "x" }', "0.2.0"), /could not set the version/);
});
