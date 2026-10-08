// 🌐 fetch_browser runtime switches — `fetch.cors` / `fetch.cookie`
//
// Verifies the per-runtime switches that select fetch behaviour:
//   - cors off  → no CORS enforcement (cross-origin without ACAO succeeds)
//   - cors on   → CORS enforced
//   - cookie off→ fetch does NOT touch the jar; the caller may set `Cookie`
//   - cookie on → fetch auto-reads/writes the jar and strips `Cookie`
//
// The "client origin" is the WPT runner shim (`__wptClientOrigin`). CORS tests
// point it at a foreign origin; cookie tests point it at the server origin so
// the default `credentials: same-origin` sends the jar cookie.
//
// Build with the machinery compiled in:
//   cargo build --features fetch_browser
// Run:
//   aike -m tests/whatwg/fetch_browser_config.js

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
  console.log("SKIP: this build has no `fetch_browser` feature");
  console.log("RESULT: PASS (skipped)");
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
  check("default fetch.cors === false", fetch.cors === false);
  check("default fetch.cookie === false", fetch.cookie === false);
  check("switches are enumerable own props", Object.keys(fetch).includes("cors") && Object.keys(fetch).includes("cookie"), JSON.stringify(Object.keys(fetch)));
  try {
    fetch.cors = 1;
    check("non-boolean rejected", false, "no throw");
  } catch (e) {
    check("non-boolean rejected", e.name === "TypeError", e.name);
  }

  // ── Mode A: cors off, cookie off ──
  console.log("\n=== A: cors=false, cookie=false ===");
  fetch.cors = false;
  fetch.cookie = false;
  setClientOrigin(FOREIGN);
  {
    const echoed = await (await fetch(`${BASE}/echo`, { headers: { Cookie: "manual=1" } })).text();
    check("manual Cookie header is sent", echoed.includes("manual=1"), echoed);

    await fetch(`${BASE}/setcookie`);
    const after = await (await fetch(`${BASE}/echo`)).text();
    check("fetch does NOT auto-send a stored cookie", !after.includes("srv=xyz"), after);

    const sc = await fetch(`${BASE}/setcookie`);
    check("Set-Cookie response header is not filtered", sc.headers.getSetCookie().some((v) => v.includes("srv=xyz")));

    check("CORS not enforced (no-ACAO cross-origin succeeds)", !(await rejects(() => fetch(`${BASE}/noacao`))));
  }

  // ── Mode B: cors on, cookie off ──
  console.log("\n=== B: cors=true, cookie=false ===");
  fetch.cors = true;
  fetch.cookie = false;
  setClientOrigin(FOREIGN);
  {
    check("no-ACAO cross-origin rejects", await rejects(() => fetch(`${BASE}/noacao`)));
    check("ACAO cross-origin succeeds", !(await rejects(() => fetch(`${BASE}/acao`))));
  }

  // ── Mode C: cors off, cookie on (same-origin so credentials sends) ──
  console.log("\n=== C: cors=false, cookie=true ===");
  fetch.cors = false;
  fetch.cookie = true;
  setClientOrigin(SERVER_ORIGIN);
  {
    await fetch(`${BASE}/setcookie`);
    const after = await (await fetch(`${BASE}/echo`)).text();
    check("fetch auto-sends the stored cookie", after.includes("srv=xyz"), after);

    const echoed = await (await fetch(`${BASE}/echo`, { headers: { Cookie: "manual=1" } })).text();
    check("manual Cookie header is stripped (fetch owns the jar)", !echoed.includes("manual=1"), echoed);
  }

  // ── Mode D: cors on, cookie on ──
  console.log("\n=== D: cors=true, cookie=true ===");
  fetch.cors = true;
  fetch.cookie = true;
  {
    setClientOrigin(FOREIGN);
    check("no-ACAO cross-origin rejects", await rejects(() => fetch(`${BASE}/noacao`)));

    setClientOrigin(SERVER_ORIGIN);
    await fetch(`${BASE}/setcookie`);
    const after = await (await fetch(`${BASE}/echo`)).text();
    check("auto cookie still works with cors on", after.includes("srv=xyz"), after);
  }

  await server.shutdown();
  console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
  console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  process.exit(fail > 0 ? 1 : 0);
}

if (HAS_FEATURE) {
  main().catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
    process.exit(2);
  });
}
