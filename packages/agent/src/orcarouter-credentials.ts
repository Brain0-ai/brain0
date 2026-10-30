/**
 * OrcaRouter credential seam.
 *
 * brain0 keeps provider secrets where it already keeps them: the environment, alongside
 * `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`. The dashboard additionally lets you hand a key to the
 * running server for the current session (in memory only — never written to disk), because the
 * alternative would be inventing a second plaintext secret store.
 *
 * Both are adapters on one interface, so the provider, the model catalog and the HTTP routes never
 * grow their own credential handling. brain0 has no browser-login framework, so OrcaRouter's
 * OAuth 2.0 + PKCE flow is intentionally not part of this integration: a pasted `sk-orca-…` key is
 * the native path here.
 */

import { maskOrcaRouterKey, ORCAROUTER_ENV_KEY, looksLikeOrcaRouterKey } from "./orcarouter.js";

export interface OrcaRouterCredential {
  /** The `sk-orca-…` key. Never log this field. */
  key: string;
  /** Where it came from, for the GUI's truthful status line. */
  source: string;
  /** Redacted form safe for logs and the GUI. */
  masked: string;
}

/** One way of obtaining a key. `read()` returns the raw key or `undefined`. */
export interface CredentialSource {
  readonly name: string;
  read(env: Record<string, string | undefined>): string | undefined;
}

/** `ORCAROUTER_API_KEY` — the same env convention as the other remote providers. */
export const envCredentialSource: CredentialSource = {
  name: "env",
  read: (env) => {
    const raw = env[ORCAROUTER_ENV_KEY]?.trim();
    return raw && raw.length > 0 ? raw : undefined;
  },
};

/**
 * In-memory adapter for the dashboard: the key lives only in this process (durability across
 * restarts comes from the environment). `clear()` forgets it, so the next request falls back to
 * the environment credential.
 */
export class SessionCredentialSource implements CredentialSource {
  readonly name = "session";
  private key: string | undefined;

  read(): string | undefined {
    return this.key;
  }

  /** Replace the session key, validating only its shape. Empty clears it. */
  set(raw: string): void {
    const trimmed = raw.trim();
    if (!trimmed) {
      this.key = undefined;
      return;
    }
    if (!looksLikeOrcaRouterKey(trimmed)) {
      throw new Error("that does not look like an OrcaRouter key (expected sk-orca-…)");
    }
    this.key = trimmed;
  }

  clear(): void {
    this.key = undefined;
  }
}

/** Resolve the first usable credential: the session key wins over the environment. */
export function resolveOrcaRouterCredential(
  env: Record<string, string | undefined>,
  session?: SessionCredentialSource,
  sources: CredentialSource[] = [envCredentialSource],
): OrcaRouterCredential | undefined {
  const ordered = session ? [session, ...sources] : sources;
  for (const source of ordered) {
    const key = source.read(env);
    if (key) return { key, source: source.name, masked: maskOrcaRouterKey(key) };
  }
  return undefined;
}

/**
 * Classify a provider failure for the UI. `needs-reauth` is terminal for this credential: the key
 * was revoked or is wrong, and no refresh attempt is meaningful (an OrcaRouter key is durable, not
 * an OAuth access token). The account is marked and the user is pointed at the console.
 */
export type OrcaRouterFailure = "needs-reauth" | "rate-limited" | "unreachable" | "provider-error";

export function classifyOrcaRouterStatus(status: number | undefined): OrcaRouterFailure {
  if (status === 401 || status === 403) return "needs-reauth";
  if (status === 429) return "rate-limited";
  if (status === undefined || status >= 500) return "unreachable";
  return "provider-error";
}

/** Terminal reauthentication state for one account/generation. */
export interface CredentialState {
  account: string;
  generation: number;
  needsReauth: boolean;
  reason?: string;
}

/**
 * Mark exactly one credential generation as needing reauthentication. A late failure carrying an
 * older generation must not poison a credential the user has since replaced.
 */
export function markNeedsReauth(
  state: CredentialState,
  failure: { generation: number; reason: string },
): CredentialState {
  if (failure.generation !== state.generation) return state;
  return { ...state, needsReauth: true, reason: failure.reason };
}

/**
 * Monotonic credential generations. Replacing or clearing the key bumps the generation, so a
 * failure raised by a request issued under the old key cannot mark the new one.
 */
export class CredentialGenerations {
  private state: CredentialState = { account: "orcarouter", generation: 0, needsReauth: false };

  current(): CredentialState {
    return { ...this.state };
  }

  /** A new key was installed: the account is usable again under a fresh generation. */
  replace(): CredentialState {
    this.state = {
      account: this.state.account,
      generation: this.state.generation + 1,
      needsReauth: false,
      reason: undefined,
    };
    return this.current();
  }

  /** A request failed: mark only the generation that made it. */
  fail(generation: number, reason: string): CredentialState {
    this.state = markNeedsReauth(this.state, { generation, reason });
    return this.current();
  }
}
