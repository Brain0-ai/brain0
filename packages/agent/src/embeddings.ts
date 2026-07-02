/**
 * Embedding providers. The user picks the provider; a deterministic
 * local provider is always available for offline use and tests, and an OpenAI-backed
 * provider is offered for production-quality embeddings.
 */

/** Identifies an embedding provider + whether calling it egresses (for the truthful GUI notice). */
export interface EmbedDescriptor {
  name: string;
  remote: boolean;
}

export interface EmbeddingProvider {
  readonly dimension: number;
  /** Defaults to local/non-remote when absent (back-compat for test constructors). */
  readonly descriptor?: EmbedDescriptor;
  embed(text: string): Promise<number[]>;
}

/** FNV-1a 32-bit hash. */
function fnv1a(text: string): number {
  let hash = 0x811c9dc5;
  for (let i = 0; i < text.length; i++) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

function tokenize(text: string): string[] {
  return text
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((t) => t.length > 0);
}

/** Deterministic, dependency-free feature-hashing embedding (signed buckets, L2-normalized). */
export function localEmbed(text: string, dimension: number): number[] {
  const vec = new Array<number>(dimension).fill(0);
  for (const token of tokenize(text)) {
    const bucket = fnv1a(token) % dimension;
    const sign = (fnv1a(`sign:${token}`) & 1) === 1 ? 1 : -1;
    vec[bucket] = (vec[bucket] ?? 0) + sign;
  }
  let norm = 0;
  for (const x of vec) norm += x * x;
  norm = Math.sqrt(norm);
  if (norm === 0) return vec;
  return vec.map((x) => x / norm);
}

/** Deterministic local embeddings — no network, used by default and in tests. */
export class LocalEmbeddingProvider implements EmbeddingProvider {
  readonly descriptor: EmbedDescriptor = { name: "local", remote: false };

  constructor(public readonly dimension = 256) {}

  embed(text: string): Promise<number[]> {
    return Promise.resolve(localEmbed(text, this.dimension));
  }
}

/** OpenAI embeddings (e.g. `text-embedding-3-small`). Requires an API key. */
export class OpenAIEmbeddingProvider implements EmbeddingProvider {
  readonly dimension: number;
  readonly descriptor: EmbedDescriptor = { name: "openai", remote: true };

  constructor(
    private readonly apiKey: string,
    private readonly model = "text-embedding-3-small",
    dimension = 1536,
    private readonly baseUrl = "https://api.openai.com/v1",
  ) {
    this.dimension = dimension;
  }

  async embed(text: string): Promise<number[]> {
    const res = await fetch(`${this.baseUrl}/embeddings`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${this.apiKey}`,
      },
      body: JSON.stringify({ model: this.model, input: text }),
    });
    if (!res.ok) {
      throw new Error(`OpenAI embeddings failed: ${res.status} ${await res.text()}`);
    }
    const json = (await res.json()) as { data: Array<{ embedding: number[] }> };
    const embedding = json.data[0]?.embedding;
    if (!embedding) throw new Error("OpenAI embeddings: empty response");
    return embedding;
  }
}
