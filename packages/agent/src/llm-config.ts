/**
 * Provider selection from the environment. Pure: env → config,
 * no I/O, no provider construction (the server builds the classes). Privacy-first defaults:
 *
 * - **LLM**: explicit `BRAIN0_LLM_PROVIDER` wins; else an API key (Anthropic, then OpenAI) selects
 *   that remote provider; else the LOCAL Ollama default. There is NO offline runtime provider —
 *   `echo` is reachable only by pinning `BRAIN0_LLM_PROVIDER=echo` (tests/debug).
 * - **Embeddings**: default LOCAL always; the remote OpenAI embedder is used ONLY when explicitly
 *   opted in via `BRAIN0_EMBED_PROVIDER=openai` — never merely because `OPENAI_API_KEY` is present.
 *   This closes the "local LLM + silently-remote embeddings" leak.
 */

export type Env = Record<string, string | undefined>;

export type LlmProviderName = "ollama" | "anthropic" | "openai" | "echo";

export interface LlmConfig {
  provider: LlmProviderName;
  model: string;
  endpoint: string;
  apiKey: string; // "" for local
  remote: boolean;
}

export interface EmbedConfig {
  provider: "local" | "openai";
  model: string;
  endpoint: string;
  apiKey: string;
  dim: number;
  remote: boolean;
}

const OLLAMA_ENDPOINT = "http://localhost:11434/v1";
const DEFAULT_OLLAMA_MODEL = "qwen3:4b";

export function resolveLlmConfig(env: Env): LlmConfig {
  const model = env.BRAIN0_LLM_MODEL;
  const endpoint = env.BRAIN0_LLM_ENDPOINT;
  const ollama = (): LlmConfig => ({
    provider: "ollama",
    model: model ?? DEFAULT_OLLAMA_MODEL,
    endpoint: endpoint ?? OLLAMA_ENDPOINT,
    apiKey: "",
    remote: false,
  });
  const anthropic = (): LlmConfig => ({
    provider: "anthropic",
    model: model ?? "claude-sonnet-4-6",
    endpoint: endpoint ?? "https://api.anthropic.com/v1",
    apiKey: env.ANTHROPIC_API_KEY ?? "",
    remote: true,
  });
  const openai = (): LlmConfig => ({
    provider: "openai",
    model: model ?? "gpt-4o-mini",
    endpoint: endpoint ?? "https://api.openai.com/v1",
    apiKey: env.OPENAI_API_KEY ?? "",
    remote: true,
  });

  switch (env.BRAIN0_LLM_PROVIDER?.toLowerCase()) {
    case "ollama":
      return ollama();
    case "anthropic":
      return anthropic();
    case "openai":
      return openai();
    case "echo":
      return { provider: "echo", model: "", endpoint: "", apiKey: "", remote: false };
    default:
      break;
  }
  // No explicit pin: a present API key opts into that remote provider (egress is redacted +
  // surfaced); otherwise default to the local Ollama reasoner.
  if (env.ANTHROPIC_API_KEY) return anthropic();
  if (env.OPENAI_API_KEY) return openai();
  return ollama();
}

export function resolveEmbedderConfig(env: Env): EmbedConfig {
  const dim = Number(env.BRAIN0_EMBED_DIM ?? "256") || 256;
  if (env.BRAIN0_EMBED_PROVIDER?.toLowerCase() === "openai") {
    return {
      provider: "openai",
      model: env.BRAIN0_EMBED_MODEL ?? "text-embedding-3-small",
      endpoint: env.BRAIN0_EMBED_ENDPOINT ?? "https://api.openai.com/v1",
      apiKey: env.OPENAI_API_KEY ?? "",
      dim,
      remote: true,
    };
  }
  // Default + explicit "local": never auto-select remote embeddings just because a key exists.
  return { provider: "local", model: "local-feature-hash", endpoint: "", apiKey: "", dim, remote: false };
}
