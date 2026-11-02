/**
 * Cheaper Inference — named provider constants.
 *
 * Cheaper Inference is an OpenAI-compatible LLM gateway (`https://api.cheaperinference.com/v1`).
 * One key gives access to models from several labs. The wire format is OpenAI Chat Completions,
 * so the server uses the shared {@link OpenAICompatProvider}; this module only keeps the provider's
 * identity in one place for the config layer, the registry and the docs.
 */

/** Stable provider id used in `BRAIN0_LLM_PROVIDER` and in the provider registry. */
export const CHEAPERINFERENCE_PROVIDER_ID = "cheaperinference";

/** Display name surfaced in the GUI provider list and the docs. */
export const CHEAPERINFERENCE_DISPLAY_NAME = "Cheaper Inference";

/** OpenAI-compatible inference base URL. */
export const CHEAPERINFERENCE_API_BASE = "https://api.cheaperinference.com/v1";

/** Provider docs. */
export const CHEAPERINFERENCE_DOCS_URL = "https://cheaperinference.com/docs";

/** Environment variable that holds the user's key. */
export const CHEAPERINFERENCE_ENV_KEY = "CHEAPER_INFERENCE_API_KEY";

/** Default model. Model ids on this gateway are bare (no vendor prefix). */
export const CHEAPERINFERENCE_DEFAULT_MODEL = "gpt-5.4-mini";
