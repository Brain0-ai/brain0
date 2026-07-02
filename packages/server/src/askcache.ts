/**
 * Cost control for the smart chat: a small bounded cache for full `ask()`
 * results, keyed on the normalized query PLUS the index generation — so a repeat question
 * costs zero LLM tokens, and any new ingest invalidates everything automatically (the
 * generation probe changes). Pure and unit-tested; the generation probe lives in server.ts.
 */

/** Whitespace/case-insensitive form of a query, so trivial rephrasings share a cache slot. */
export function normalizeQuery(q: string): string {
  return q.trim().toLowerCase().replace(/\s+/g, " ");
}

/** Insertion-ordered bounded map: setting beyond `max` evicts the oldest entry. */
export class BoundedCache<V> {
  private readonly map = new Map<string, V>();
  constructor(private readonly max = 100) {}

  get(key: string): V | undefined {
    return this.map.get(key);
  }

  set(key: string, value: V): void {
    if (this.map.has(key)) this.map.delete(key); // refresh insertion order
    this.map.set(key, value);
    if (this.map.size > this.max) {
      const oldest = this.map.keys().next().value;
      if (oldest !== undefined) this.map.delete(oldest);
    }
  }

  get size(): number {
    return this.map.size;
  }
}
