// CORS probe for the WHATWG fetch() implementation.
//
// Runs a battery of fetches against a deliberately CORS-hostile server and
// prints one `PROBE <name>: <outcome>` line per probe. Requires the
// `fetch_browser` feature (the machinery must be compiled in) and turns CORS
// enforcement on for this runtime via `fetch.cors = true`.
//
// The server is aike's own `HttpServer` started with `listen({ cors: false })`,
// so it attaches no permissive CORS layer and every `Access-Control-*` header
// below is one the test set deliberately. Readiness polling and the hit
// counters are read back over HttpClient, which never applies CORS.
//
// Run: aike -m tests/whatwg/fetch_cors.mjs

import { HttpClient, HttpServer } from "http";

const PORT = 18131;
const BASE = `http://127.0.0.1:${PORT}`;

// Pin the client ("document") origin so the CORS decision is deterministic
// instead of depending on whatever ambient location the host provides. It
// deliberately differs from BASE, so every probe below is cross-origin.
const CLIENT_ORIGIN = "http://client.test:8080";
globalThis.self = {
  location: { origin: CLIENT_ORIGIN, href: `${CLIENT_ORIGIN}/doc.html` },
};

// Enable CORS enforcement for this runtime (default is off).
fetch.cors = true;

const svr = new HttpServer();

// Counted server-side so a preflight (OPTIONS) can be told apart from the
// actual request it guards.
const hits = {};
const count = (key) => {
  hits[key] = (hits[key] ?? 0) + 1;
};

// No Access-Control-Allow-Origin at all.
svr.get("/no-acao", (_req, res) => {
  count("GET /no-acao");
  res.end("no-acao");
});

// Wildcard allow-origin.
svr.get("/acao-star", (_req, res) => {
  count("GET /acao-star");
  res.setHeader("access-control-allow-origin", "*");
  res.end("acao-star");
});

// Opaque-origin allow-origin — matches only a null client origin, not ours.
svr.get("/acao-null", (_req, res) => {
  count("GET /acao-null");
  res.setHeader("access-control-allow-origin", "null");
  res.end("acao-null");
});

// Allow-origin naming exactly the client origin.
svr.get("/acao-client", (_req, res) => {
  count("GET /acao-client");
  res.setHeader("access-control-allow-origin", CLIENT_ORIGIN);
  res.end("acao-client");
});

// Allow-origin naming some unrelated origin.
svr.get("/acao-other", (_req, res) => {
  count("GET /acao-other");
  res.setHeader("access-control-allow-origin", "http://other.example");
  res.end("acao-other");
});

// Response header exposure: x-secret is not listed in
// Access-Control-Expose-Headers, so a CORS-filtered response must hide it.
svr.get("/expose", (_req, res) => {
  count("GET /expose");
  res.setHeader("access-control-allow-origin", "*");
  res.setHeader("content-type", "text/plain");
  res.setHeader("x-secret", "hidden-unless-exposed");
  res.end("expose");
});

// Preflight-allowed: the OPTIONS reply permits PUT + x-custom.
svr.options("/preflight-ok", (_req, res) => {
  count("OPTIONS /preflight-ok");
  res.setHeader("access-control-allow-origin", "*");
  res.setHeader("access-control-allow-methods", "PUT");
  res.setHeader("access-control-allow-headers", "x-custom");
  res.sendStatus(204);
});
svr.put("/preflight-ok", (_req, res) => {
  count("PUT /preflight-ok");
  res.setHeader("access-control-allow-origin", "*");
  res.end("preflight-ok");
});

// Preflight-denied: the OPTIONS reply permits only GET, so a PUT preflight
// must fail the fetch.
svr.options("/preflight-denied", (_req, res) => {
  count("OPTIONS /preflight-denied");
  res.setHeader("access-control-allow-origin", "*");
  res.setHeader("access-control-allow-methods", "GET");
  res.setHeader("access-control-allow-headers", "content-type");
  res.sendStatus(204);
});
svr.put("/preflight-denied", (_req, res) => {
  count("PUT /preflight-denied");
  res.setHeader("access-control-allow-origin", "*");
  res.end("preflight-denied");
});

svr.listen({ host: "127.0.0.1", port: PORT, cors: false });

const client = new HttpClient();

async function waitForServer() {
  for (let i = 0; i < 100; i++) {
    try {
      const r = await client.get(`${BASE}/no-acao`, { timeout: 2 });
      if (r.status === 200) return;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`test server did not come up on ${BASE}`);
}

/** Fetch and print a single outcome line describing what happened. */
async function probe(name, url, init) {
  try {
    const r = await fetch(url, init);
    const body = await r.text();
    console.log(`PROBE ${name}: OK status=${r.status} type=${r.type} body="${body}"`);
  } catch (e) {
    console.log(`PROBE ${name}: REJECTED ${e.message}`);
  }
}

await waitForServer();

// Simple cross-origin GETs: only an acceptable Access-Control-Allow-Origin
// passes the CORS check.
await probe("get-no-acao", `${BASE}/no-acao`);
await probe("get-acao-star", `${BASE}/acao-star`);
await probe("get-acao-null", `${BASE}/acao-null`);
await probe("get-acao-client", `${BASE}/acao-client`);
await probe("get-acao-other", `${BASE}/acao-other`);

// PUT is not CORS-safelisted, so it needs a preflight round trip first.
await probe("put-preflight-ok", `${BASE}/preflight-ok`, {
  method: "PUT",
  headers: { "x-custom": "1" },
  body: "x",
});
await probe("put-preflight-denied", `${BASE}/preflight-denied`, {
  method: "PUT",
  headers: { "x-custom": "1" },
  body: "x",
});

// A CORS-safelisted method still needs a preflight for a non-safelisted
// header. /no-acao has no OPTIONS route, so the preflight cannot succeed.
await probe("get-unsafe-header", `${BASE}/no-acao`, {
  headers: { "x-custom": "1" },
});

// Response header exposure: x-secret is only readable when the server lists it
// in Access-Control-Expose-Headers.
const exposed = await fetch(`${BASE}/expose`);
console.log(`PROBE expose-x-secret: ${exposed.headers.get("x-secret")}`);
console.log(`PROBE expose-content-type: ${exposed.headers.get("content-type")}`);

console.log(`PROBE server-hits: ${JSON.stringify(hits)}`);

await svr.shutdown();