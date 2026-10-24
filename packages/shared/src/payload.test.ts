import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { FsPayloadReader } from "./payload.js";

const HEX = "ab".repeat(32);

function fixture(): { base: string; reader: FsPayloadReader } {
  const base = mkdtempSync(join(tmpdir(), "brain0-payload-"));
  mkdirSync(join(base, "payload", "ab"), { recursive: true });
  writeFileSync(join(base, "payload", "ab", `${HEX.slice(2)}.blob`), "a real diff");
  // Outside the payload root: must never be reachable through a ref.
  writeFileSync(join(base, "secret.blob"), "OUTSIDE_SECRET_MARKER");
  mkdirSync(join(base, "payload", "aa"));
  return { base, reader: new FsPayloadReader(join(base, "payload")) };
}

test("getText reads a well-formed ref, with or without the scheme", async () => {
  const { base, reader } = fixture();
  try {
    assert.equal(await reader.getText(`blake3:${HEX}`), "a real diff");
    assert.equal(await reader.getText(HEX), "a real diff");
    assert.equal(await reader.getText(`blake3:${"cd".repeat(32)}`), undefined); // well-formed, absent
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});

test("getText refuses refs that are not a BLAKE3 digest (GHSA-8vg9-5mf9-5c84)", async () => {
  const { base, reader } = fixture();
  try {
    for (const ref of [
      "aa/../../secret", // the reported PoC: resolves to <base>/secret.blob
      "blake3:aa/../../secret",
      `aa/../../secret${"0".repeat(48)}`, // padded to 64 chars
      "../secret",
      "..\\..\\secret",
      `${HEX.slice(0, 62)}/.`,
      HEX.toUpperCase(),
      `${HEX}\0`,
      HEX.slice(0, 63),
      `${HEX}0`,
      "",
      "blake3:",
    ]) {
      assert.equal(await reader.getText(ref), undefined, `ref ${JSON.stringify(ref)}`);
    }
  } finally {
    rmSync(base, { recursive: true, force: true });
  }
});
