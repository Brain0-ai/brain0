import { test } from "node:test";
import assert from "node:assert/strict";
import { isCrossSite, isLoopbackHost } from "./origin.js";

test("isLoopbackHost accepts only loopback names, on any port", () => {
  for (const host of ["localhost", "localhost:8787", "LOCALHOST:8787", "127.0.0.1:8787", "[::1]:8787", "localhost:5173"]) {
    assert.equal(isLoopbackHost(host), true, host);
  }
  for (const host of [
    undefined,
    "",
    "evil.example:8787", // DNS rebinding: the attacker's own hostname
    "localhost.evil.example",
    "evil.example@localhost",
    "127.0.0.1.nip.io:8787",
    "127.0.0.2:8787",
    "0.0.0.0:8787",
    "localhost:8787 ",
    "localhost:",
    "localhost:8787,evil.example",
  ]) {
    assert.equal(isLoopbackHost(host), false, String(host));
  }
});

test("isCrossSite trusts same-origin and user-initiated requests and non-browser clients", () => {
  assert.equal(isCrossSite({}), false); // curl, the CLI's /health probe
  assert.equal(isCrossSite({ "sec-fetch-site": "same-origin" }), false);
  assert.equal(isCrossSite({ "sec-fetch-site": "none" }), false); // typed in the address bar
  // vite dev: the page is on :5173 and the proxy forwards its same-origin request.
  assert.equal(isCrossSite({ "sec-fetch-site": "same-origin", origin: "http://localhost:5173" }), false);
  assert.equal(isCrossSite({ origin: "http://127.0.0.1:8787" }), false);
});

test("isCrossSite refuses requests from other sites", () => {
  assert.equal(isCrossSite({ "sec-fetch-site": "cross-site" }), true);
  assert.equal(isCrossSite({ "sec-fetch-site": "same-site" }), true); // another local port
  assert.equal(isCrossSite({ origin: "https://evil.example" }), true); // no Sec-Fetch-* (older browser)
  assert.equal(isCrossSite({ origin: "null" }), true); // sandboxed iframe, file: page
  assert.equal(isCrossSite({ "sec-fetch-site": "same-origin", origin: "https://evil.example" }), true);
});
