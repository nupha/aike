// Regression test for bugs/http-server.md — multi-cookie `Set-Cookie` responses.
//
// Two symptoms, both rooted in the response-building path:
//   1. Repeated `setHeader("Set-Cookie", …)` lost every call but the last,
//      because the builder used `HeaderMap::insert` (replace) instead of
//      `append`.
//   2. A header value the HTTP layer refuses to encode (CR/LF, forbidden by
//      RFC 9110 §5.5) vanished with no exception at all, because the builder
//      dropped `HeaderValue::from_str` failures on the floor.
//
// Also pins the replacement semantics for single-valued fields, so the fix does
// not silently start emitting two `Content-Type` lines.
//
// Assertions go through HttpClient, which applies no CORS and exposes the raw
// header list, so `getSetCookie()` sees what actually went out on the wire.
//
// Run: aike -m tests/http/set_cookie.mjs

import { HttpClient, HttpServer } from "http";

const PORT = 18146;
const BASE = `http://127.0.0.1:${PORT}`;

const A = "sid=AAA; Path=/";
const B = "pref=dark; Path=/";
// A cookie whose value carries a comma — the reason the comma-joined
// workaround cannot be parsed back into separate cookies.
const EXPIRES = "pref=dark; Expires=Wed, 21 Oct 2035 07:28:00 GMT; Path=/";

let pass = 0;
let fail = 0;

function check(label, ok, detail) {
  if (ok) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

const client = new HttpClient();

const svr = new HttpServer();

// Baseline: one cookie, unchanged.
svr.get("/single", (_req, res) => {
  res.setHeader("Set-Cookie", A);
  res.end("ok");
});

// Two setHeader calls → two field lines. This is the bug.
svr.get("/twice", (_req, res) => {
  res.setHeader("Set-Cookie", A);
  res.setHeader("Set-Cookie", B);
  res.end("ok");
});

// Three, one of them comma-bearing, to pin ordering and prove no folding.
svr.get("/three", (_req, res) => {
  res.setHeader("Set-Cookie", A);
  res.setHeader("Set-Cookie", EXPIRES);
  res.setHeader("Set-Cookie", B);
  res.end("ok");
});

// setHeaders() must accumulate the same way.
svr.get("/setHeaders", (_req, res) => {
  res.setHeaders({ "Set-Cookie": A, "X-Trace": "t1" });
  res.setHeader("Set-Cookie", B);
  res.end("ok");
});

// A CR/LF in the value must throw, not vanish.
svr.get("/bad-value", (_req, res) => {
  try {
    res.setHeader("Set-Cookie", `${A}\n${B}`);
    res.end("no-throw");
  } catch (e) {
    res.setHeader("X-Threw", e instanceof TypeError ? "TypeError" : "other");
    res.setHeader("X-Message", e.message);
    res.end("threw");
  }
});

// An invalid header name must throw too.
svr.get("/bad-name", (_req, res) => {
  try {
    res.setHeader("bad header", "x");
    res.end("no-throw");
  } catch (e) {
    res.setHeader("X-Threw", e instanceof TypeError ? "TypeError" : "other");
    res.end("threw");
  }
});

// Single-valued field: the later call replaces the earlier one, so the response
// must not carry two Content-Type lines.
svr.get("/dup-content-type", (_req, res) => {
  res.setHeader("Content-Type", "text/plain");
  res.setHeader("Content-Type", "text/html");
  res.end("ok");
});

// The comma-bearing cookie must survive as one intact value.
svr.get("/expires", (_req, res) => {
  res.setHeader("Set-Cookie", EXPIRES);
  res.end("ok");
});

svr.listen({ host: "127.0.0.1", port: PORT, cors: false });

async function main() {
  console.log("\n=== multi-value Set-Cookie ===");

  let r = await client.get(`${BASE}/single`);
  await r.text();
  check(
    "single cookie is unchanged",
    JSON.stringify(r.headers.getSetCookie()) === JSON.stringify([A]),
    JSON.stringify(r.headers.getSetCookie()),
  );

  r = await client.get(`${BASE}/twice`);
  await r.text();
  check(
    "two setHeader calls emit two Set-Cookie headers",
    JSON.stringify(r.headers.getSetCookie()) === JSON.stringify([A, B]),
    JSON.stringify(r.headers.getSetCookie()),
  );

  r = await client.get(`${BASE}/three`);
  await r.text();
  check(
    "three cookies keep order and value",
    JSON.stringify(r.headers.getSetCookie()) === JSON.stringify([A, EXPIRES, B]),
    JSON.stringify(r.headers.getSetCookie()),
  );

  console.log("\n=== setHeaders accumulation ===");
  r = await client.get(`${BASE}/setHeaders`);
  await r.text();
  check(
    "setHeaders + setHeader both land",
    JSON.stringify(r.headers.getSetCookie()) === JSON.stringify([A, B]),
    JSON.stringify(r.headers.getSetCookie()),
  );

  console.log("\n=== invalid header values throw ===");
  r = await client.get(`${BASE}/bad-value`);
  const badBody = await r.text();
  check("CR/LF value throws instead of vanishing", badBody === "threw", badBody);
  check("  ...with a TypeError", r.headers.get("x-threw") === "TypeError",
    r.headers.get("x-threw"));
  check(
    "  ...naming the header in the message",
    (r.headers.get("x-message") ?? "").includes("Set-Cookie"),
    r.headers.get("x-message"),
  );
  check("  ...and no Set-Cookie was sent",
    r.headers.getSetCookie().length === 0,
    JSON.stringify(r.headers.getSetCookie()));

  r = await client.get(`${BASE}/bad-name`);
  const nameBody = await r.text();
  check("invalid header name throws", nameBody === "threw", nameBody);
  check("  ...with a TypeError",
    r.headers.get("x-threw") === "TypeError", r.headers.get("x-threw"));

  console.log("\n=== single-valued fields still replace ===");
  r = await client.get(`${BASE}/dup-content-type`);
  await r.text();
  check(
    "duplicate Content-Type keeps only the last value",
    r.headers.get("content-type") === "text/html",
    r.headers.get("content-type"),
  );
  // Count the raw field lines: a second Content-Type would show up here.
  const ctLines = [];
  r.headers.forEach((value, key) => {
    if (key === "content-type") ctLines.push(value);
  });
  check(
    "  ...and is emitted exactly once",
    JSON.stringify(ctLines) === JSON.stringify(["text/html"]),
    JSON.stringify(ctLines),
  );

  console.log("\n=== comma-bearing cookie stays intact ===");
  r = await client.get(`${BASE}/expires`);
  await r.text();
  check(
    "Expires cookie is one unsplit value",
    JSON.stringify(r.headers.getSetCookie()) === JSON.stringify([EXPIRES]),
    JSON.stringify(r.headers.getSetCookie()),
  );

  console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} ===`);
  console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
}

await main();
await svr.shutdown();
process.exit(fail > 0 ? 1 : 0);
