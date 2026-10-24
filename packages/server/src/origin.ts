/**
 * Request-origin policy for the local server. Binding to 127.0.0.1 keeps the LAN out, but not
 * the web pages open in the user's browser: any site can send requests to 127.0.0.1, and a
 * DNS-rebinding site (its own hostname re-pointed at 127.0.0.1) can even read the responses as
 * same-origin. So every request must name a loopback Host, and the API routes also refuse
 * cross-site browser requests. Pure functions, unit-tested; the HTTP wiring lives in server.ts.
 */

import type { IncomingHttpHeaders } from "node:http";

// Any port: under vite dev the proxy rewrites Host to the server's, but Origin stays the
// page's (http://localhost:5173), and isCrossSite checks Origin with this same pattern.
const LOOPBACK_HOST_RE = /^(localhost|127\.0\.0\.1|\[::1\])(:\d{1,5})?$/i;

/** Whether a Host header names this machine's loopback interface. */
export function isLoopbackHost(host: string | undefined): boolean {
  return host !== undefined && LOOPBACK_HOST_RE.test(host);
}

/**
 * Whether a browser request was issued by a page on another site. Current browsers stamp every
 * request with Sec-Fetch-Site; older ones (Safari < 16.4) send only Origin, and only on CORS
 * and non-GET requests (the literal "null" from sandboxed or file: pages). A plain GET from an
 * <img> or <script> on such a browser carries neither, so it passes here, cannot read the
 * response, and is stopped at routes with side effects by a required custom header.
 * Non-browser clients (curl, the CLI's health probe) send neither and count as local.
 */
export function isCrossSite(headers: IncomingHttpHeaders): boolean {
  const site = headers["sec-fetch-site"];
  if (site !== undefined && site !== "same-origin" && site !== "none") return true;
  const origin = headers.origin;
  if (origin === undefined) return false;
  try {
    return !isLoopbackHost(new URL(origin).host);
  } catch {
    return true;
  }
}
