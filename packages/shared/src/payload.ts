/**
 * Read-side of the heavy payload store. Mirrors the Rust `FsPayloadStore` on-disk layout
 * (content-addressed, sharded by the first two hex chars), so the agent can hydrate
 * prompts/summaries/diffs written by the core.
 */

import { readFile } from "node:fs/promises";
import { join } from "node:path";

const REF_SCHEME = "blake3:";

// A BLAKE3 digest as the core writes it: 64 lowercase hex chars. Refs reach the reader from
// HTTP query strings, so this is the only shape ever turned into a path; anything else (`..`,
// separators, NUL, short or uppercase hex) can never name a file outside the store.
const DIGEST_RE = /^[0-9a-f]{64}$/;

export class FsPayloadReader {
  constructor(private readonly root: string) {}

  private pathFor(ref: string): string | undefined {
    const hex = ref.startsWith(REF_SCHEME) ? ref.slice(REF_SCHEME.length) : ref;
    if (!DIGEST_RE.test(hex)) return undefined;
    return join(this.root, hex.slice(0, 2), `${hex.slice(2)}.blob`);
  }

  /** Fetch a payload as text, or `undefined` if it is not present or `ref` is malformed. */
  async getText(ref: string): Promise<string | undefined> {
    const path = this.pathFor(ref);
    if (path === undefined) return undefined;
    try {
      return await readFile(path, "utf8");
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "ENOENT") return undefined;
      throw err;
    }
  }
}
