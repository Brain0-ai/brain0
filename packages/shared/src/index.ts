/**
 * @brain0/shared — shared TypeScript types, the storage read-client, the payload reader,
 * and the risk-color logic, mirroring the Rust core so the agent and GUI read the same
 * index it writes.
 */

export * from "./types.js";
export * from "./risk.js";
export * from "./payload.js";
export * from "./graph.js";
export * from "./detail.js";
export {
  Brain0Store,
  cosine,
  encodeVector,
  decodeVector,
} from "./store.js";
