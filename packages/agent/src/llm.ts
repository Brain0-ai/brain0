/**
 * Pluggable LLM providers. At runtime the provider is LOCAL
 * (Ollama, via its OpenAI-compatible endpoint) or REMOTE (Anthropic / OpenAI). There is no offline
 * runtime provider: {@link EchoLLM} exists ONLY for tests, and a missing/unreachable provider
 * becomes a {@link NullLLMProvider} that throws {@link NoLlmError} (the server surfaces a clear UI
 * state instead of silently echoing).
 */

export interface LlmMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

/** What provider is active + whether it egresses — surfaced to the GUI for a truthful notice. */
export interface ProviderDescriptor {
  name: string;
  model: string;
  endpoint: string;
  remote: boolean;
  ok?: boolean;
}

export interface LLMProvider {
  readonly descriptor?: ProviderDescriptor;
  complete(messages: LlmMessage[]): Promise<string>;
  /** Optional native structured mode; callers still validate via parseStructured. */
  completeStructured?(messages: LlmMessage[], schema: object): Promise<string>;
  /** Optional cheap reachability check (used once at boot). */
  probe?(): Promise<boolean>;
}

/** Thrown when no LLM is configured/reachable. Never produced at runtime by a real provider. */
export class NoLlmError extends Error {
  constructor(message = "no LLM configured or reachable") {
    super(message);
    this.name = "NoLlmError";
  }
}

/** Stand-in when no provider is reachable: every call rejects so the route shows a clear state. */
export class NullLLMProvider implements LLMProvider {
  readonly descriptor: ProviderDescriptor = {
    name: "none",
    model: "",
    endpoint: "",
    remote: false,
    ok: false,
  };
  complete(): Promise<string> {
    return Promise.reject(new NoLlmError());
  }
  completeStructured(): Promise<string> {
    return Promise.reject(new NoLlmError());
  }
}

/** Deterministic, offline provider for tests ONLY (never selected at runtime). */
export class EchoLLM implements LLMProvider {
  complete(messages: LlmMessage[]): Promise<string> {
    const user = messages.find((m) => m.role === "user")?.content ?? "";
    const firstLine = user.split("\n").find((l) => l.trim().length > 0) ?? "";
    return Promise.resolve(`[offline-llm] ${firstLine.slice(0, 200)}`);
  }
}

async function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  return Promise.race([
    p,
    new Promise<never>((_, reject) => setTimeout(() => reject(new Error("timeout")), ms)),
  ]);
}

/**
 * OpenAI Chat Completions — also the LOCAL Ollama provider (its OpenAI-compatible endpoint at
 * `http://localhost:11434/v1`). For Ollama pass `apiKey=""` (no auth header) and `remote:false`.
 */
export class OpenAICompatProvider implements LLMProvider {
  readonly descriptor: ProviderDescriptor;

  constructor(
    private readonly apiKey: string,
    private readonly model = "gpt-4o-mini",
    private readonly baseUrl = "https://api.openai.com/v1",
    descriptor?: Partial<ProviderDescriptor>,
    // Generous cap so the structured findings JSON is not truncated mid-object (Ollama maps this to
    // num_predict; a small default there would cut the answer → invalid JSON → prose fallback).
    private readonly maxTokens = 4096,
  ) {
    this.descriptor = {
      name: descriptor?.name ?? "openai",
      model,
      endpoint: baseUrl,
      remote: descriptor?.remote ?? true,
      ok: descriptor?.ok,
    };
  }

  private headers(): Record<string, string> {
    const h: Record<string, string> = { "content-type": "application/json" };
    if (this.apiKey) h.authorization = `Bearer ${this.apiKey}`;
    return h;
  }

  private async chat(body: Record<string, unknown>): Promise<string> {
    const res = await fetch(`${this.baseUrl}/chat/completions`, {
      method: "POST",
      headers: this.headers(),
      body: JSON.stringify({ model: this.model, max_tokens: this.maxTokens, ...body }),
    });
    if (!res.ok) throw new Error(`${this.descriptor.name} failed: ${res.status} ${await res.text()}`);
    const json = (await res.json()) as { choices: Array<{ message?: { content?: string } }> };
    return json.choices[0]?.message?.content ?? "";
  }

  complete(messages: LlmMessage[]): Promise<string> {
    return this.chat({ messages });
  }

  // Best-effort native JSON mode (broadly supported by OpenAI and recent Ollama). The caller still
  // validates with parseStructured and falls back to `complete()` if this errors.
  completeStructured(messages: LlmMessage[], _schema: object): Promise<string> {
    return this.chat({ messages, response_format: { type: "json_object" } });
  }

  // Cheap reachability: GET /models (no model load), so a cold Ollama still reports reachable.
  async probe(): Promise<boolean> {
    try {
      const res = await withTimeout(fetch(`${this.baseUrl}/models`, { headers: this.headers() }), 4000);
      return res.ok;
    } catch {
      return false;
    }
  }
}

/** Back-compat alias (kept so existing `OpenAIProvider` imports keep resolving). */
export const OpenAIProvider = OpenAICompatProvider;

/** Anthropic Claude (Messages API). Remote only. */
export class ClaudeProvider implements LLMProvider {
  readonly descriptor: ProviderDescriptor;

  constructor(
    private readonly apiKey: string,
    private readonly model = "claude-sonnet-4-6",
    // Generous cap so the structured findings JSON completes instead of truncating mid-object.
    private readonly maxTokens = 4096,
    private readonly baseUrl = "https://api.anthropic.com/v1",
  ) {
    this.descriptor = { name: "anthropic", model, endpoint: baseUrl, remote: true };
  }

  async complete(messages: LlmMessage[]): Promise<string> {
    const system = messages
      .filter((m) => m.role === "system")
      .map((m) => m.content)
      .join("\n\n");
    const rest = messages
      .filter((m) => m.role !== "system")
      .map((m) => ({ role: m.role, content: m.content }));

    const res = await fetch(`${this.baseUrl}/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": this.apiKey,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({
        model: this.model,
        max_tokens: this.maxTokens,
        system: system || undefined,
        messages: rest,
      }),
    });
    if (!res.ok) throw new Error(`Claude failed: ${res.status} ${await res.text()}`);
    const json = (await res.json()) as { content: Array<{ type: string; text?: string }> };
    return json.content.map((c) => c.text ?? "").join("");
  }

  async probe(): Promise<boolean> {
    try {
      await withTimeout(this.complete([{ role: "user", content: "ping" }]), 6000);
      return true;
    } catch {
      return false;
    }
  }
}
