// 🔐 fetch_cors_cookie build semantics — CORS + cookie jar always on
//
// The browser-security layer is a compile-time decision (`fetch_cors_cookie`
// feature). In such a build there is NO runtime switch: CORS is always
// enforced and fetch always owns the shared cookie jar. This file verifies
// that contract:
//   - `fetch.cors` / `fetch.cookie` do not exist
//   - cross-origin fetch without ACAO rejects
//   - cross-origin fetch with ACAO succeeds
//   - fetch auto-reads/writes the shared jar and strips caller `Cookie`
//
// The "client origin" is the WPT runner shim (`__wptClientOrigin`). CORS tests
// point it at a foreign origin; cookie tests point it at the server origin so
// the default `credentials: same-origin` sends the jar cookie.
//
// Build with the machinery compiled in:
//   cargo build --features fetch_cors_cookie
// Run:
//   aike -m tests/whatwg/fetch_cors_cookie.js

import { HttpServer } from "http";

let pass = 0;
let fail = 0;
let skip = 0;

function check(label, ok, detail) {
  if (ok) {
    pass += 1;
    console.log(`  ✅ ${label}`);
  } else {
    fail += 1;
    console.log(`  ❌ ${label}${detail === undefined ? "" : " — " + detail}`);
  }
}

function setClientOrigin(origin) {
  globalThis.__wptClientOrigin = origin;
  globalThis.__wptClientUrl = origin + "/";
}

// `cookieStore` exists only when the browser machinery was compiled in.
const HAS_FEATURE = typeof cookieStore !== "undefined";
if (!HAS_FEATURE) {
  console.log("SKIP: this build has no `fetch_cors_cookie` feature");
  console.log("RESULT: PASS (skipped)");
  process.exit(0);
}

const PORT = 8161;
const BASE = `http://127.0.0.1:${PORT}`;
const SERVER_ORIGIN = BASE;
const FOREIGN = "https://client.example";

const server = new HttpServer();
server
  .get("/setcookie", (_req, res) => {
    res.setHeader("Set-Cookie", "srv=xyz; Path=/");
    res.end("ok");
  })
  .get("/echo", (req, res) => res.end(req.headers["cookie"] ?? ""))
  .get("/noacao", (_req, res) => res.end("ok"))
  .get("/acao", (_req, res) => {
    res.setHeader("Access-Control-Allow-Origin", "*");
    res.end("ok");
  });

async function rejects(fn) {
  try {
    await fn();
    return false;
  } catch {
    return true;
  }
}

async function main() {
  server.listen({ port: PORT, host: "127.0.0.1", cors: false });
  await new Promise((r) => setTimeout(r, 200));

  console.log("\n=== API shape ===");
  check("no fetch.cors switch", !("cors" in fetch), `keys=${JSON.stringify(Object.keys(fetch))}`);
  check("no fetch.cookie switch", !("cookie" in fetch));

  console.log("\n=== CORS (always enforced) ===");
  setClientOrigin(FOREIGN);
  check("no-ACAO cross-origin rejects", await rejects(() => fetch(`${BASE}/noacao`)));
  check("ACAO cross-origin succeeds", !(await rejects(() => fetch(`${BASE}/acao`))));

  console.log("\n=== Cookie jar (always on) ===");
  setClientOrigin(SERVER_ORIGIN);
  await fetch(`${BASE}/setcookie`);
  const after = await (await fetch(`${BASE}/echo`)).text();
  check("fetch auto-sends the stored cookie", after.includes("srv=xyz"), after);

  const echoed = await (await fetch(`${BASE}/echo`, { headers: { Cookie: "manual=1" } })).text();
  check("manual Cookie header is stripped (fetch owns the jar)", !echoed.includes("manual=1"), echoed);

  await server.shutdown();
  console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
  console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  process.exit(fail > 0 ? 1 : 0);
}

main().catch((e) => {
  console.log("UNEXPECTED ERROR:", e);
  console.log("RESULT: FAIL");
  process.exit(2);
});
