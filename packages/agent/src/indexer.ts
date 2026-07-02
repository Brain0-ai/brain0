/**
 * Embedding indexer: backfills embeddings for task nodes that lack them,
 * computed over the task's prompt + decision summary (hydrated from the payload store).
 * Runs incrementally — only tasks missing an embedding are processed.
 */

import type { Brain0Store } from "@brain0/shared";
import type { EmbeddingProvider } from "./embeddings.js";

/** Function that hydrates a payload reference to text (e.g. `FsPayloadReader.getText`). */
export type GetText = (ref: string) => Promise<string | undefined>;

/** Assemble the embedding source text for a task from its versions' prompt/summary refs. */
export async function taskEmbeddingText(
  store: Brain0Store,
  taskId: string,
  getText: GetText,
): Promise<string> {
  const parts: string[] = [];
  for (const version of store.taskVersions(taskId)) {
    if (version.promptRef) {
      const text = await getText(version.promptRef);
      if (text) parts.push(text);
    }
    if (version.decisionSummaryRef) {
      const text = await getText(version.decisionSummaryRef);
      if (text) parts.push(text);
    }
  }
  return parts.join("\n");
}

/** Compute and store embeddings for all tasks missing one. Returns how many were indexed. */
export async function backfillEmbeddings(
  store: Brain0Store,
  getText: GetText,
  provider: EmbeddingProvider,
): Promise<number> {
  let indexed = 0;
  for (const taskId of store.tasksMissingEmbeddings()) {
    const text = await taskEmbeddingText(store, taskId, getText);
    if (text.trim().length === 0) continue;
    const embedding = await provider.embed(text);
    store.putTaskEmbedding(taskId, embedding);
    indexed += 1;
  }
  return indexed;
}
