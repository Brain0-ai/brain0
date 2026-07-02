/**
 * Static-file helpers for serving the built GUI (`packages/gui/build`) from the same server
 * that exposes the API — so `brain0 up` needs one process and one port, with no vite dev
 * dependency. Pure functions, unit-tested; the HTTP wiring lives in server.ts.
 */

import { normalize, resolve, sep } from "node:path";

/** Content types for everything the vite build emits (plus the svg/png favicons). */
const CONTENT_TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".json": "application/json",
  ".map": "application/json",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".txt": "text/plain; charset=utf-8",
};

/** The content type for a file path, defaulting to octet-stream for anything unexpected. */
export function contentTypeFor(path: string): string {
  const dot = path.lastIndexOf(".");
  const ext = dot === -1 ? "" : path.slice(dot).toLowerCase();
  return CONTENT_TYPES[ext] ?? "application/octet-stream";
}

/**
 * Resolve a URL pathname inside the GUI root, refusing anything unsafe. Strict policy: any
 * `..` segment is refused outright (no legitimate GUI request ever contains one — the built
 * app references assets by clean absolute paths), as are NUL bytes and malformed encodings.
 * A final prefix check guards the invariant even if normalization rules ever change.
 */
export function safeJoin(root: string, urlPathname: string): string | undefined {
  let decoded: string;
  try {
    decoded = decodeURIComponent(urlPathname);
  } catch {
    return undefined; // malformed percent-encoding
  }
  if (decoded.includes("\0")) return undefined;
  if (decoded.split("/").some((seg) => seg === "..")) return undefined;
  const rootAbs = resolve(root);
  const joined = resolve(rootAbs, "." + normalize("/" + decoded));
  return joined === rootAbs || joined.startsWith(rootAbs + sep) ? joined : undefined;
}

/**
 * Cache policy: vite content-hashes everything under /assets/, so those are immutable;
 * index.html (and the favicons at the root) must always revalidate.
 */
export function cacheControlFor(urlPathname: string): string {
  return urlPathname.startsWith("/assets/")
    ? "public, max-age=31536000, immutable"
    : "no-cache";
}
