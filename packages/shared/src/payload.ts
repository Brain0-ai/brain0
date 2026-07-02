/**
 * Read-side of the heavy payload store. Mirrors the Rust `FsPayloadStore` on-disk layout
 * (content-addressed, sharded by the first two hex chars), so the agent can hydrate
 * prompts/summaries/diffs written by the core.
 */

import { readFile } from "node:fs/promises";
import { join } from "node:path";

const REF_SCHEME = "blake3:";

export class FsPayloadReader {
  constructor(private readonly root: string) {}

  private pathFor(ref: string): string {
    const hex = ref.startsWith(REF_SCHEME) ? ref.slice(REF_SCHEME.length) : ref;
    const shard = hex.slice(0, 2);
    const rest = hex.slice(2);
    return join(this.root, shard, `${rest}.blob`);
  }

  /** Fetch a payload as text, or `undefined` if it is not present. */
  async getText(ref: string): Promise<string | undefined> {
    try {
      return await readFile(this.pathFor(ref), "utf8");
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "ENOENT") return undefined;
      throw err;
    }
  }
}
