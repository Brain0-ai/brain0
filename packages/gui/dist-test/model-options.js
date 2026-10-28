/**
 * Pure model-option logic for the provider panel: which catalog entries may appear in the chat
 * dropdown, and what the selected value becomes when the catalog or the required modality changes.
 *
 * Kept free of DOM so it is unit-testable; the panel renders exactly what these functions return,
 * so the dropdown can never offer a model the current capability filter excludes.
 */
const TEXT_ENDPOINTS = ["openai", "openai-response", "anthropic", "gemini"];
const NON_TEXT_ENDPOINTS = ["image-generation", "openai-video", "jina-rerank", "embeddings"];
/**
 * Options for the chat model selector. `modality` is the strongest input the UI will actually
 * send: a model that does not declare it is excluded (fail closed — never inferred from the name).
 */
export function chatModelOptions(catalog, modality) {
    if (!catalog)
        return [];
    return catalog.models.filter((m) => {
        const types = m.supportedEndpointTypes.map((t) => t.toLowerCase());
        if (!types.some((t) => TEXT_ENDPOINTS.includes(t)))
            return false;
        if (types.some((t) => NON_TEXT_ENDPOINTS.includes(t)))
            return false;
        if (!modality)
            return true;
        return m.inputModalities.map((x) => x.toLowerCase()).includes(modality.toLowerCase());
    });
}
/**
 * Keep the current selection only if it is still offered; otherwise fall back to the first option
 * so an incompatible model is never silently retained and sent.
 */
export function reconcileSelection(current, options, preferred) {
    if (options.some((o) => o.id === current))
        return { model: current, changed: false };
    const preferredHit = preferred ? options.find((o) => o.id === preferred) : undefined;
    const next = preferredHit?.id ?? options[0]?.id ?? "";
    return { model: next, changed: next !== current };
}
/** Human-readable state line for the catalog (live vs degraded/outage fallback). */
export function catalogNote(catalog, count) {
    if (!catalog)
        return "Model catalog unavailable.";
    if (catalog.degraded) {
        return `Gateway unreachable (${catalog.error ?? "unknown"}) — showing the verified fallback catalog.`;
    }
    return `${count} compatible chat models from the live catalog.`;
}
//# sourceMappingURL=model-options.js.map