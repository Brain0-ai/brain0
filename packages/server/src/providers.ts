/**
 * Provider API for the dashboard.
 *
 * The GUI never holds an OrcaRouter key. It asks the server for provider status (with the key
 * already masked), for the live model catalog (fetched server-side), and hands a pasted key to the
 * server for this session only. One credential seam serves every route, so the provider, the
 * catalog and the status panel cannot disagree about which key is in use.
 */

import {
  CredentialGenerations,
  PROVIDER_REGISTRY,
  SessionCredentialSource,
  classifyOrcaRouterStatus,
  discoverModels,
  envCredentialSource,
  maskOrcaRouterKey,
  resolveOrcaRouterBase,
  resolveOrcaRouterCredential,
  type CatalogModel,
  type CatalogResult,
  type OrcaRouterCredential,
} from "@brain0/agent";
import {
  ORCAROUTER_API_BASE,
  ORCAROUTER_CYBER_MODELS,
  ORCAROUTER_CYBER_MODELS_URL,
  ORCAROUTER_CYBER_URL,
  ORCAROUTER_DEFAULT_MODEL,
  ORCAROUTER_DISPLAY_NAME,
  ORCAROUTER_FALLBACK_MODEL,
  ORCAROUTER_HOME_URL,
  ORCAROUTER_KEYS_URL,
  ORCAROUTER_PROVIDER_ID,
} from "@brain0/agent";

export interface ProviderApiOptions {
  env: Record<string, string | undefined>;
  session: SessionCredentialSource;
  generations: CredentialGenerations;
  /** Current active provider/model, and how to install a new one after a key/model change. */
  active: () => { provider: string; model: string };
  applyModel: (model: string) => void;
  /** Rebuild the provider after a credential change; returns the new reachability. */
  reload: () => Promise<{ ok: boolean }>;
  /** Catalog fetch, injectable for tests. */
  discover?: typeof discoverModels;
  now?: () => number;
}

export interface ProviderStatus {
  active: { provider: string; model: string };
  providers: typeof PROVIDER_REGISTRY;
  orcarouter: {
    id: string;
    displayName: string;
    endpoint: string;
    defaultModel: string;
    fallbackModel: string;
    recommendedModels: typeof ORCAROUTER_CYBER_MODELS;
    credential: { present: boolean; source?: string; masked?: string };
    authState: { generation: number; needsReauth: boolean; reason?: string };
    catalog: { source: CatalogResult["source"]; degraded: boolean; count: number; error?: string };
    links: { home: string; cyber: string; cyberModels: string; keys: string };
    envKey: string;
  };
}

/** Masked credential view — never contains the key itself. */
function credentialView(
  credential: OrcaRouterCredential | undefined,
): { present: boolean; source?: string; masked?: string } {
  return credential
    ? { present: true, source: credential.source, masked: credential.masked }
    : { present: false };
}

export function createProviderApi(options: ProviderApiOptions): {
  status(): Promise<ProviderStatus>;
  models(query: { capability?: string; modality?: string }): Promise<{
    source: CatalogResult["source"];
    degraded: boolean;
    error?: string;
    models: CatalogModel[];
  }>;
  saveKey(raw: string): Promise<{ ok: true; masked: string; reachable: boolean }>;
  clearKey(): Promise<{ ok: true; present: boolean }>;
  selectModel(model: string): Promise<{ ok: true; model: string }>;
  /** Record a provider failure against the credential generation that made the request. */
  noteFailure(generation: number, status?: number): void;
} {
  const discover = options.discover ?? discoverModels;
  let lastCatalog: CatalogResult | undefined;

  const credential = (): OrcaRouterCredential | undefined =>
    resolveOrcaRouterCredential(options.env, options.session, [envCredentialSource]);

  const fetchCatalog = async (
    requiredModality?: string,
    capability = "chat",
  ): Promise<CatalogResult> => {
    const cred = credential();
    const result = await discover({
      apiKey: cred?.key,
      baseUrl: resolveOrcaRouterBase(options.env),
      capability,
      requiredModality,
    });
    if (!requiredModality) lastCatalog = result;
    return result;
  };

  return {
    async status(): Promise<ProviderStatus> {
      const catalog = await fetchCatalog();
      const active = options.active();
      return {
        active,
        providers: PROVIDER_REGISTRY,
        orcarouter: {
          id: ORCAROUTER_PROVIDER_ID,
          displayName: ORCAROUTER_DISPLAY_NAME,
          endpoint: resolveOrcaRouterBase(options.env),
          defaultModel: ORCAROUTER_DEFAULT_MODEL,
          fallbackModel: ORCAROUTER_FALLBACK_MODEL,
          recommendedModels: ORCAROUTER_CYBER_MODELS,
          credential: credentialView(credential()),
          authState: options.generations.current(),
          catalog: {
            source: catalog.source,
            degraded: catalog.degraded,
            count: catalog.models.length,
            error: catalog.error,
          },
          links: {
            home: ORCAROUTER_HOME_URL,
            cyber: ORCAROUTER_CYBER_URL,
            cyberModels: ORCAROUTER_CYBER_MODELS_URL,
            keys: ORCAROUTER_KEYS_URL,
          },
          envKey: "ORCAROUTER_API_KEY",
        },
      };
    },

    async models(query): Promise<{
      source: CatalogResult["source"];
      degraded: boolean;
      error?: string;
      models: CatalogModel[];
    }> {
      // capability=chat is the only capability brain0 has (text chat over indexed diffs); any
      // other value is answered with the same filtered text catalog so a stale client still gets
      // a coherent list rather than an error.
      const result = await fetchCatalog(
        query.modality?.toLowerCase() || undefined,
        query.capability || "chat",
      );
      return {
        source: result.source,
        degraded: result.degraded,
        error: result.error,
        models: result.models,
      };
    },

    async saveKey(raw: string): Promise<{ ok: true; masked: string; reachable: boolean }> {
      options.session.set(raw); // throws on a shape mismatch, before anything is stored
      options.generations.replace();
      const reachable = (await options.reload()).ok;
      const masked = maskOrcaRouterKey(raw);
      return { ok: true, masked, reachable };
    },

    async clearKey(): Promise<{ ok: true; present: boolean }> {
      options.session.clear();
      options.generations.replace();
      await options.reload();
      return { ok: true, present: credential() !== undefined };
    },

    async selectModel(model: string): Promise<{ ok: true; model: string }> {
      const catalog = lastCatalog ?? (await fetchCatalog());
      if (!catalog.models.some((m) => m.id === model)) {
        throw new Error(`model '${model}' is not in the compatible catalog`);
      }
      options.applyModel(model);
      await options.reload();
      return { ok: true, model };
    },

    noteFailure(generation, status): void {
      const kind = classifyOrcaRouterStatus(status);
      if (kind === "needs-reauth") {
        options.generations.fail(generation, "OrcaRouter rejected the key (401/403) — reconnect");
      }
    },
  };
}

/** HTTP status for an error message thrown by the handlers above. */
export function providerApiStatus(err: unknown): number {
  const message = err instanceof Error ? err.message : String(err);
  return /does not look like|not in the compatible catalog|required|invalid/i.test(message) ? 400 : 500;
}

/** Exported for the docs/tests: the public API base the provider talks to. */
export const ORCAROUTER_PUBLIC_API_BASE = ORCAROUTER_API_BASE;
