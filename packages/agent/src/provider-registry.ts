/**
 * Named provider registry.
 *
 * The GUI shows one row per provider this build can speak to. OrcaRouter Cyber Inference is a
 * first-class entry with its own id, display name, endpoint and default model — it is not a
 * "custom base URL" affordance for some other provider.
 */

import { resolveLlmConfig, type LlmProviderName } from "./llm-config.js";
import {
  ORCAROUTER_API_BASE,
  ORCAROUTER_CYBER_MODELS_URL,
  ORCAROUTER_CYBER_URL,
  ORCAROUTER_DEFAULT_MODEL,
  ORCAROUTER_DISPLAY_NAME,
  ORCAROUTER_ENV_KEY,
  ORCAROUTER_FALLBACK_MODEL,
  ORCAROUTER_KEYS_URL,
  ORCAROUTER_PROVIDER_ID,
} from "./orcarouter.js";

export interface ProviderEntry {
  id: LlmProviderName;
  name: string;
  endpoint: string;
  defaultModel: string;
  remote: boolean;
  /** Whether a model catalog is available for this provider (drives the dropdown). */
  catalog: boolean;
  /** Short, truthful description shown in the provider panel. */
  description?: string;
  docsUrl?: string;
  modelsUrl?: string;
  keysUrl?: string;
  envKey?: string;
}

const ORCAROUTER_CYBER_MODELS_LABEL =
  "recommended tier for verified workspaces: OrcaCyber Ultra / OrcaCyber Zero";

/** Providers surfaced in the dashboard's provider panel, in display order. */
export const PROVIDER_REGISTRY: readonly ProviderEntry[] = [
  {
    id: "ollama",
    name: "Ollama (local)",
    endpoint: "http://localhost:11434/v1",
    defaultModel: "qwen3:4b",
    remote: false,
    catalog: false,
    description: "Local runtime used by default; zero egress.",
  },
  {
    id: "anthropic",
    name: "Anthropic",
    endpoint: "https://api.anthropic.com/v1",
    defaultModel: "claude-sonnet-4-6",
    remote: true,
    catalog: false,
    envKey: "ANTHROPIC_API_KEY",
  },
  {
    id: "openai",
    name: "OpenAI",
    endpoint: "https://api.openai.com/v1",
    defaultModel: "gpt-4o-mini",
    remote: true,
    catalog: false,
    envKey: "OPENAI_API_KEY",
  },
  {
    id: ORCAROUTER_PROVIDER_ID,
    name: ORCAROUTER_DISPLAY_NAME,
    endpoint: ORCAROUTER_API_BASE,
    defaultModel: ORCAROUTER_DEFAULT_MODEL,
    remote: true,
    catalog: true,
    description:
      "Named reasoning provider for security teams running authorized, scoped work: red teams get " +
      "the OrcaCyber models, blue teams keep the same endpoint's gateway controls. Default " +
      `orcarouter/auto; ${ORCAROUTER_CYBER_MODELS_LABEL}; general fallback ${ORCAROUTER_FALLBACK_MODEL}.`,
    docsUrl: ORCAROUTER_CYBER_URL,
    modelsUrl: ORCAROUTER_CYBER_MODELS_URL,
    keysUrl: ORCAROUTER_KEYS_URL,
    envKey: ORCAROUTER_ENV_KEY,
  },
  {
    id: "echo",
    name: "Echo (tests only)",
    endpoint: "",
    defaultModel: "",
    remote: false,
    catalog: false,
    description: "Deterministic offline provider, only via BRAIN0_LLM_PROVIDER=echo.",
  },
];

/** The provider the next request would use, given the current environment. */
export function activeProviderId(env: Record<string, string | undefined>): LlmProviderName {
  return resolveLlmConfig(env).provider;
}

/** Look up one registry entry. */
export function providerEntry(id: string): ProviderEntry | undefined {
  return PROVIDER_REGISTRY.find((p) => p.id === id);
}
