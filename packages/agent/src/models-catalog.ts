/**
 * OrcaRouter model discovery.
 *
 * The authoritative catalog is `GET <base>/models` on the configured inference origin. Live
 * discovery improves the provider entry but must never make it unusable: when the request fails,
 * is slow, or returns something unexpected, the verified cold-start seed below is used instead and
 * the caller surfaces a degraded state.
 *
 * Parsing is deliberately defensive — bounded item count, bounded response size, and every field
 * read by shape — so a hostile or broken catalog response cannot exhaust memory.
 */

import {
  ORCAROUTER_API_BASE,
  ORCAROUTER_DEFAULT_MODEL,
  ORCAROUTER_FALLBACK_MODEL,
} from "./orcarouter.js";

/** Endpoint types this client can actually speak (OpenAI-compatible chat). */
const TEXT_ENDPOINT_TYPES = ["openai", "openai-response", "anthropic", "gemini"];

/** Endpoint types that are never a chat completion, whatever the model is called. */
const NON_TEXT_ENDPOINT_TYPES = ["image-generation", "openai-video", "jina-rerank", "embeddings"];

export interface CatalogModel {
  id: string;
  name: string;
  contextLength?: number;
  /** Declared input modalities, lower-cased. Empty means "not declared". */
  inputModalities: string[];
  reasoningEfforts?: string[];
  supportedEndpointTypes: string[];
}

export interface CatalogResult {
  models: CatalogModel[];
  /** `live` = fetched from the provider; `fallback` = verified seed / last known good. */
  source: "live" | "fallback";
  degraded: boolean;
  error?: string;
}

/**
 * Small verified seed. Every entry is a real OrcaRouter route with the metadata this client
 * relies on; `orcarouter/auto` is the provider default, `orcarouter/fusion` the general fallback.
 */
export const ORCAROUTER_SEED_CATALOG: readonly CatalogModel[] = [
  {
    id: "openai/gpt-5.5",
    name: "GPT-5.5",
    contextLength: 400_000,
    inputModalities: ["text", "image"],
    reasoningEfforts: ["low", "medium", "high", "xhigh"],
    supportedEndpointTypes: ["openai", "openai-response"],
  },
  {
    id: "anthropic/claude-opus-4.8",
    name: "Claude Opus 4.8",
    contextLength: 200_000,
    inputModalities: ["text", "image"],
    supportedEndpointTypes: ["anthropic", "openai"],
  },
  {
    id: "google/gemini-3.5-flash",
    name: "Gemini 3.5 Flash",
    contextLength: 1_000_000,
    inputModalities: ["text", "image"],
    supportedEndpointTypes: ["gemini", "openai"],
  },
  {
    id: "deepseek/deepseek-v4-pro",
    name: "DeepSeek V4 Pro",
    contextLength: 160_000,
    inputModalities: ["text"],
    supportedEndpointTypes: ["openai"],
  },
  {
    id: ORCAROUTER_DEFAULT_MODEL,
    name: "OrcaRouter Auto",
    inputModalities: ["text"],
    supportedEndpointTypes: ["openai", "openai-response", "anthropic", "gemini"],
  },
  {
    id: ORCAROUTER_FALLBACK_MODEL,
    name: "OrcaRouter Fusion",
    inputModalities: ["text"],
    supportedEndpointTypes: ["openai", "openai-response", "anthropic", "gemini"],
  },
];

/** Caps so a bad response cannot consume unbounded memory or UI. */
export const MAX_CATALOG_BYTES = 4_000_000;
export const MAX_CATALOG_ITEMS = 1_000;

function asStringArray(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((v): v is string => typeof v === "string").map((v) => v.toLowerCase());
}

function asPositiveInt(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? Math.round(value) : undefined;
}

/** Normalize one raw `data[]` entry; `undefined` means the entry is unusable. */
export function parseCatalogModel(raw: unknown): CatalogModel | undefined {
  if (!raw || typeof raw !== "object") return undefined;
  const item = raw as Record<string, unknown>;
  const id = typeof item.id === "string" ? item.id.trim() : "";
  if (!id || id.length > 200) return undefined;
  const endpointTypes = asStringArray(item.supported_endpoint_types);
  if (endpointTypes.length === 0) return undefined;
  const architecture = (item.architecture ?? {}) as Record<string, unknown>;
  const modalities = asStringArray(architecture.input_modalities);
  const parameters = asStringArray(item.supported_parameters);
  const efforts = parameters.some((p) => p.includes("reasoning"))
    ? ["low", "medium", "high", "xhigh"]
    : undefined;
  return {
    id,
    name: typeof item.name === "string" && item.name.trim() ? item.name.trim() : id,
    contextLength: asPositiveInt(item.context_length) ?? asPositiveInt(item.max_completion_tokens),
    inputModalities: modalities,
    reasoningEfforts: efforts,
    supportedEndpointTypes: endpointTypes,
  };
}

/**
 * Chat models this client can call: an OpenAI-compatible endpoint type, no non-text endpoint, and
 * (optionally) a required input modality declared explicitly. Undeclared capability fails closed
 * for modality filters.
 */
export function filterChatModels(models: CatalogModel[], requiredModality?: string): CatalogModel[] {
  const text = models.filter(
    (m) =>
      m.supportedEndpointTypes.some((t) => TEXT_ENDPOINT_TYPES.includes(t)) &&
      !m.supportedEndpointTypes.some((t) => NON_TEXT_ENDPOINT_TYPES.includes(t)),
  );
  if (!requiredModality) return text;
  const want = requiredModality.toLowerCase();
  return text.filter((m) => m.inputModalities.includes(want));
}

/** Parse a raw `/models` body into the catalog (unfiltered), bounded and shape-checked. */
export function parseCatalogBody(body: unknown): CatalogModel[] {
  const data = Array.isArray(body)
    ? body
    : body && typeof body === "object"
      ? ((body as Record<string, unknown>).data as unknown)
      : undefined;
  if (!Array.isArray(data)) return [];
  const out: CatalogModel[] = [];
  for (const raw of data.slice(0, MAX_CATALOG_ITEMS)) {
    const parsed = parseCatalogModel(raw);
    if (parsed) out.push(parsed);
  }
  return out;
}

/** The verified seed, restricted to the models this client can speak to. */
export function seedChatModels(requiredModality?: string): CatalogModel[] {
  return filterChatModels(ORCAROUTER_SEED_CATALOG.map((m) => ({ ...m })), requiredModality);
}

export interface DiscoverOptions {
  apiKey?: string;
  baseUrl?: string;
  /** Capability to ask the gateway for; `chat` is the only one brain0 can call. */
  capability?: string;
  requiredModality?: string;
  timeoutMs?: number;
  fetchImpl?: typeof fetch;
}

/**
 * Fetch the live catalog. Never throws: a failure is reported through `degraded`/`error` and the
 * verified seed is returned so a fresh install still has a working model list.
 */
export async function discoverModels(options: DiscoverOptions = {}): Promise<CatalogResult> {
  const base = (options.baseUrl ?? ORCAROUTER_API_BASE).replace(/\/+$/, "");
  const doFetch = options.fetchImpl ?? fetch;
  const capability = options.capability ?? "chat";
  const url = `${base}/models?capability=${encodeURIComponent(capability)}`;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), options.timeoutMs ?? 6_000);
  try {
    const headers: Record<string, string> = { accept: "application/json" };
    if (options.apiKey) headers.authorization = `Bearer ${options.apiKey}`;
    const res = await doFetch(url, { headers, signal: controller.signal });
    if (!res.ok) {
      return {
        models: seedChatModels(options.requiredModality),
        source: "fallback",
        degraded: true,
        error: `models endpoint returned ${res.status}`,
      };
    }
    const text = await res.text();
    if (text.length > MAX_CATALOG_BYTES) {
      return {
        models: seedChatModels(options.requiredModality),
        source: "fallback",
        degraded: true,
        error: "models response exceeded the size cap",
      };
    }
    const parsed = parseCatalogBody(JSON.parse(text) as unknown);
    const models = filterChatModels(parsed, options.requiredModality);
    if (models.length === 0) {
      return {
        models: seedChatModels(options.requiredModality),
        source: "fallback",
        degraded: true,
        error: "models endpoint advertised no compatible chat model",
      };
    }
    return { models, source: "live", degraded: false };
  } catch (err) {
    const message = err instanceof Error ? err.message : "models request failed";
    return {
      models: seedChatModels(options.requiredModality),
      source: "fallback",
      degraded: true,
      error: message,
    };
  } finally {
    clearTimeout(timer);
  }
}
