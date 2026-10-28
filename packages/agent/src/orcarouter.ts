/**
 * OrcaRouter Cyber Inference — first-class provider constants.
 *
 * OrcaRouter is an OpenAI-compatible gateway (`https://api.orcarouter.ai/v1`). This module keeps
 * the provider's identity and model policy in one place so the config layer, the server routes,
 * the GUI panel, the catalog fallback and the docs cannot drift apart.
 *
 * The provider is aimed at security teams doing authorized, scoped work: the default
 * `orcarouter/auto` route works with any key, the OrcaCyber models are the recommended tier for
 * teams whose workspace has been verified for cyber work, and `orcarouter/fusion` is the general
 * fallback.
 */

/** Stable provider id used in `BRAIN0_LLM_PROVIDER` and in the provider registry. */
export const ORCAROUTER_PROVIDER_ID = "orcarouter";

/** Display name surfaced in the GUI, the provider registry and the docs. */
export const ORCAROUTER_DISPLAY_NAME = "OrcaRouter Cyber Inference";

/** OpenAI-compatible inference + model-discovery base URL. */
export const ORCAROUTER_API_BASE = "https://api.orcarouter.ai/v1";

/** Official provider page (brand surface). */
export const ORCAROUTER_HOME_URL = "https://www.orcarouter.ai";

/** Cyber provider page. */
export const ORCAROUTER_CYBER_URL = "https://www.orcarouter.ai/cyber";

/** Cyber-tier model catalog. */
export const ORCAROUTER_CYBER_MODELS_URL = "https://www.orcarouter.ai/models?tier=cyber";

/** Where users create and revoke their own key. */
export const ORCAROUTER_KEYS_URL = "https://www.orcarouter.ai/console/api-keys";

/** Environment variable that holds a user's `sk-orca-…` key for the durable, file-based path. */
export const ORCAROUTER_ENV_KEY = "ORCAROUTER_API_KEY";

/** Optional endpoint override (self-hosted / staging). */
export const ORCAROUTER_ENV_ENDPOINT = "BRAIN0_ORCAROUTER_ENDPOINT";

/** Default model: automatic routing, works with any key. */
export const ORCAROUTER_DEFAULT_MODEL = "orcarouter/auto";

/** General fallback model when the recommended cyber tier is not available to a workspace. */
export const ORCAROUTER_FALLBACK_MODEL = "orcarouter/fusion";

/**
 * Recommended cyber-tier models, in the order the docs present them. These are suggestions, not
 * requirements: a workspace that has not been verified for cyber work simply sees them rejected by
 * the gateway, and can stay on `orcarouter/auto`.
 */
export const ORCAROUTER_CYBER_MODELS = [
  {
    id: "orca/orcacyber-ultra-1.0",
    name: "OrcaCyber Ultra",
    summary: "Security code scanning and multi-step reasoning over large code bases.",
  },
  {
    id: "orca/orcacyber-zero-1.5",
    name: "OrcaCyber Zero",
    summary: "OrcaCyber harness-driven model for scoped security reasoning.",
  },
] as const;

/** Lightweight shape check only — the first real request establishes validity. */
export function looksLikeOrcaRouterKey(value: string): boolean {
  const key = value.trim();
  return key.startsWith("sk-orca-") && key.length > "sk-orca-".length + 7;
}

/** Mask a key for display: never returns more than the last four characters. */
export function maskOrcaRouterKey(value: string): string {
  const key = value.trim();
  if (!key) return "";
  if (!looksLikeOrcaRouterKey(key)) return "••••";
  return `sk-orca-…${key.slice(-4)}`;
}

/** Resolve the inference base URL: explicit override wins, default is the public relay. */
export function resolveOrcaRouterBase(env: Record<string, string | undefined>): string {
  const explicit = env[ORCAROUTER_ENV_ENDPOINT]?.trim();
  return explicit && explicit.length > 0 ? explicit.replace(/\/+$/, "") : ORCAROUTER_API_BASE;
}
