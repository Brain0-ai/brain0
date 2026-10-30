/**
 * Provider panel: pick a provider, hold an OrcaRouter key for this session, and choose a model
 * from the live catalog.
 *
 * The browser never sees the catalog URL's credential: the server fetches `/models` with the key
 * and returns model metadata only. When live discovery fails the server returns its verified seed
 * and marks the result degraded, which this panel shows instead of silently pretending the list is
 * live.
 */

import type { HttpDataSource, ProviderStatus, ModelCatalogResponse, CatalogModel } from "./datasource.js";
import { catalogNote, chatModelOptions, reconcileSelection } from "./model-options.js";

const CYBER_URL = "https://www.orcarouter.ai/cyber";
const CYBER_MODELS_URL = "https://www.orcarouter.ai/models?tier=cyber";
const HOME_URL = "https://www.orcarouter.ai";

function esc(text: string): string {
  return text.replace(/[&<>"]/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : "&quot;",
  );
}

export interface ProviderPanelOptions {
  dataSource: HttpDataSource;
  onModelSelected?: (model: string) => void;
}

export class ProviderPanel {
  private status: ProviderStatus | undefined;
  private catalog: ModelCatalogResponse | undefined;
  private selectedModel = "";
  private open = false;
  /** Strongest input modality the current entry point will send (none for text-only chat). */
  private modality: string | undefined;

  constructor(
    private readonly root: HTMLElement,
    private readonly options: ProviderPanelOptions,
  ) {}

  async mount(): Promise<void> {
    this.root.innerHTML = this.skeleton();
    this.bind();
    await this.refresh();
  }

  /** Load provider status + the OrcaRouter chat catalog and paint both. */
  async refresh(): Promise<void> {
    try {
      this.status = await this.options.dataSource.providerStatus();
    } catch {
      this.status = undefined;
    }
    const activeProvider = this.status?.active.provider ?? "ollama";
    const modelFromStatus = this.status?.active.model ?? "";
    this.selectedModel =
      activeProvider === "orcarouter" && modelFromStatus
        ? modelFromStatus
        : (this.status?.orcarouter.defaultModel ?? "");
    await this.loadCatalog();
    this.render();
  }

  private async loadCatalog(): Promise<void> {
    try {
      this.catalog = await this.options.dataSource.orcaRouterModels();
    } catch {
      this.catalog = undefined;
    }
    // Only models the current capability filter allows may reach the selector, and an old
    // selection that is no longer compatible is replaced rather than silently kept.
    const options = chatModelOptions(this.catalog, this.modality);
    const reconciled = reconcileSelection(this.selectedModel, options, this.status?.orcarouter.defaultModel);
    this.selectedModel = reconciled.model;
  }

  private skeleton(): string {
    return `
      <div class="pp-head">
        <h2>Providers</h2>
        <button class="pp-close" data-pp="close" aria-label="Close providers">×</button>
      </div>
      <div class="pp-body">
        <label class="pp-label" for="provider-select">Provider</label>
        <select id="provider-select" class="pp-select"></select>
        <p class="pp-note" id="provider-note"></p>

        <section class="pp-card" id="orca-card">
          <header class="pp-card-head">
            <div>
              <h3>OrcaRouter Cyber Inference</h3>
              <p class="pp-sub">
                A named reasoning provider for security teams doing authorized, scoped work —
                <a href="${CYBER_URL}" target="_blank" rel="noreferrer">red-team and blue-team
                models</a>, gateway controls on the same endpoint.
              </p>
            </div>
          </header>

          <label class="pp-label" for="orca-key">API key</label>
          <div class="pp-row">
            <input id="orca-key" class="pp-input" type="password" autocomplete="off"
              spellcheck="false" placeholder="sk-orca-…" aria-describedby="orca-key-hint" />
            <button class="pp-btn primary" id="orca-save">Save</button>
            <button class="pp-btn" id="orca-clear">Clear</button>
          </div>
          <p class="pp-note" id="orca-key-hint">
            Held by this brain0 server for the session only — never written to disk, never sent to the
            browser. Or export <code>ORCAROUTER_API_KEY</code> to keep it across restarts.
          </p>
          <p class="pp-status-line" id="orca-status"></p>
          <p class="pp-links">
            <a href="${HOME_URL}" target="_blank" rel="noreferrer">OrcaRouter</a> ·
            <a href="${CYBER_URL}" target="_blank" rel="noreferrer">cyber provider</a> ·
            <a href="${CYBER_MODELS_URL}" target="_blank" rel="noreferrer">cyber models</a>
          </p>

          <label class="pp-label" for="model-trigger">Model</label>
          <div class="pp-model">
            <button id="model-trigger" class="pp-trigger" aria-haspopup="listbox"
              aria-expanded="false" aria-controls="model-menu">
              <span id="model-trigger-label">loading…</span>
              <span class="pp-caret" aria-hidden="true">▾</span>
            </button>
            <div id="model-menu" class="pp-menu" role="listbox" aria-label="OrcaRouter models" hidden></div>
          </div>
          <p class="pp-note" id="model-note"></p>
        </section>
      </div>`;
  }

  private bind(): void {
    this.root.querySelector<HTMLElement>('[data-pp="close"]')?.addEventListener("click", () => this.hide());
    this.root.querySelector<HTMLButtonElement>("#orca-save")?.addEventListener("click", () => {
      void this.saveKey();
    });
    this.root.querySelector<HTMLButtonElement>("#orca-clear")?.addEventListener("click", () => {
      void this.clearKey();
    });
    this.root.querySelector<HTMLButtonElement>("#model-trigger")?.addEventListener("click", () => {
      this.toggleMenu();
    });
    this.root.querySelector<HTMLSelectElement>("#provider-select")?.addEventListener("change", () => {
      // Changing provider recomputes the catalog view for the newly selected provider.
      void this.loadCatalog().then(() => this.render());
    });
    // Click outside the model control closes the menu (and never leaves it orphaned open).
    document.addEventListener("click", (event) => {
      if (!this.open) return;
      const target = event.target as Node | null;
      if (target && this.root.querySelector(".pp-model")?.contains(target)) return;
      this.closeMenu();
    });
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && this.open) this.closeMenu();
    });
  }

  show(): void {
    this.root.hidden = false;
    this.root.querySelector<HTMLInputElement>("#orca-key")?.focus();
  }

  hide(): void {
    this.closeMenu();
    this.root.hidden = true;
  }

  private toggleMenu(): void {
    if (this.open) this.closeMenu();
    else this.openMenu();
  }

  private openMenu(): void {
    const menu = this.root.querySelector<HTMLElement>("#model-menu");
    const trigger = this.root.querySelector<HTMLButtonElement>("#model-trigger");
    if (!menu || !trigger) return;
    menu.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    this.open = true;
  }

  private closeMenu(): void {
    const menu = this.root.querySelector<HTMLElement>("#model-menu");
    const trigger = this.root.querySelector<HTMLButtonElement>("#model-trigger");
    if (menu) menu.hidden = true;
    trigger?.setAttribute("aria-expanded", "false");
    this.open = false;
  }

  private async saveKey(): Promise<void> {
    const input = this.root.querySelector<HTMLInputElement>("#orca-key");
    const status = this.root.querySelector<HTMLElement>("#orca-status");
    if (!input || !status) return;
    const value = input.value.trim();
    if (!value) {
      status.textContent = "Paste a key first.";
      status.className = "pp-status-line err";
      return;
    }
    try {
      const saved = await this.options.dataSource.saveOrcaRouterKey(value);
      input.value = "";
      status.textContent = saved.reachable
        ? `Key ${saved.masked} saved for this session · gateway reachable`
        : `Key ${saved.masked} saved, but the gateway is unreachable right now`;
      status.className = saved.reachable ? "pp-status-line ok" : "pp-status-line warn";
      await this.refresh();
    } catch (err) {
      status.textContent = err instanceof Error ? err.message : "could not save the key";
      status.className = "pp-status-line err";
    }
  }

  private async clearKey(): Promise<void> {
    const status = this.root.querySelector<HTMLElement>("#orca-status");
    try {
      await this.options.dataSource.clearOrcaRouterKey();
      if (status) {
        status.textContent = "Session key cleared.";
        status.className = "pp-status-line muted";
      }
      await this.refresh();
    } catch {
      if (status) {
        status.textContent = "could not clear the key";
        status.className = "pp-status-line err";
      }
    }
  }

  private selectModel(model: CatalogModel): void {
    this.selectedModel = model.id;
    this.closeMenu();
    void this.options.dataSource
      .selectModel(model.id)
      .then(() => this.options.onModelSelected?.(model.id))
      .catch(() => {
        const note = this.root.querySelector<HTMLElement>("#model-note");
        if (note) note.textContent = "The server rejected that model — pick another.";
      });
    this.render();
  }

  private render(): void {
    const select = this.root.querySelector<HTMLSelectElement>("#provider-select");
    const note = this.root.querySelector<HTMLElement>("#provider-note");
    const active = this.status?.active.provider ?? "ollama";
    if (select && select.options.length === 0) {
      for (const entry of this.status?.providers ?? []) {
        const opt = document.createElement("option");
        opt.value = entry.id;
        opt.textContent = entry.name;
        select.appendChild(opt);
      }
      select.value = active;
    }
    if (note) {
      const entry = this.status?.providers.find((p) => p.id === active);
      note.textContent = entry?.description ?? "";
    }

    // Credential line: masked only, plus the terminal reauthentication state.
    const statusLine = this.root.querySelector<HTMLElement>("#orca-status");
    const orca = this.status?.orcarouter;
    if (statusLine && orca) {
      if (orca.authState.needsReauth) {
        statusLine.textContent = `Key rejected — reconnect: ${orca.authState.reason ?? "401 from the gateway"}`;
        statusLine.className = "pp-status-line err";
      } else if (orca.credential.present) {
        statusLine.textContent = `Key ${orca.credential.masked} · source: ${orca.credential.source}`;
        statusLine.className = "pp-status-line ok";
      } else {
        statusLine.textContent = "No key configured yet — paste one, or set ORCAROUTER_API_KEY.";
        statusLine.className = "pp-status-line muted";
      }
    }

    // Model trigger + menu from the compatible catalog, never free text.
    const triggerLabel = this.root.querySelector<HTMLElement>("#model-trigger-label");
    if (triggerLabel) triggerLabel.textContent = this.selectedModel || "no compatible model";
    const menu = this.root.querySelector<HTMLElement>("#model-menu");
    if (menu) {
      menu.innerHTML = this.modelOptions();
      menu.querySelectorAll<HTMLElement>("[data-model]").forEach((item) => {
        item.addEventListener("click", () => {
          const model = chatModelOptions(this.catalog, this.modality).find((m) => m.id === item.dataset.model);
          if (model) this.selectModel(model);
        });
      });
    }
    const modelNote = this.root.querySelector<HTMLElement>("#model-note");
    if (modelNote) {
      modelNote.textContent = catalogNote(this.catalog, chatModelOptions(this.catalog, this.modality).length);
      modelNote.className = !this.catalog
        ? "pp-note err"
        : this.catalog.degraded
          ? "pp-note warn"
          : "pp-note";
    }
  }

  private modelOptions(): string {
    const models = chatModelOptions(this.catalog, this.modality);
    if (models.length === 0) return `<p class="pp-empty">No compatible model advertised.</p>`;
    return models
      .map(
        (m) =>
          `<div class="pp-item${m.id === this.selectedModel ? " selected" : ""}" role="option" ` +
          `aria-selected="${m.id === this.selectedModel}" data-model="${esc(m.id)}">` +
          `<span class="pp-item-id">${esc(m.id)}</span>` +
          `<span class="pp-item-meta">${m.contextLength ? `${Math.round(m.contextLength / 1000)}k ctx` : ""}` +
          `${m.inputModalities.length ? ` · ${esc(m.inputModalities.join("/"))}` : ""}</span></div>`,
      )
      .join("");
  }
}
