// 🍪 WHATWG Cookie Store API 测试 — cookieStore / CookieStore
//
// 覆盖 get / getAll / set / delete 及其算法（规范化、字符/前缀/大小校验、
// expires/maxAge、options.url、opaque origin 拒绝）与「与 fetch 共用同一
// cookie store」的集成行为。
//
// 注意：aike 无 Document，cookieStore 的「当前 URL」取运行脚本的 URL
// (import.meta.url)。file:// 脚本的 origin 是 opaque，按规范会 SecurityError，
// 因此这里用 Aike.evalModule(code, baseUrl) 在非 opaque 的 http(s) 基址下运行。
//
// 运行：aike -m tests/whatwg/cookie_store.js

import { HttpServer } from "http";

let pass = 0;
let fail = 0;
let skip = 0;

globalThis.check = function check(label, ok, detail) {
  if (ok) {
    pass += 1;
    console.log(`  ✅ ${label}`);
  } else {
    fail += 1;
    console.log(`  ❌ ${label}${detail === undefined ? "" : " — " + detail}`);
  }
};

// `cookieStore` exists only when the browser machinery was compiled in.
const HAS_FEATURE = typeof cookieStore !== "undefined";
if (!HAS_FEATURE) {
  console.log("SKIP: this build has no `fetch_browser` feature (no cookieStore)");
  console.log("RESULT: PASS (skipped)");
}

// ===================================================================
// 1. 在 https 基址下的纯 API 行为
// ===================================================================
const API_TESTS = `
  const rejectName = async (fn) => {
    try { await fn(); return "no-error"; } catch (e) { return e.name; }
  };
  const throwName = (fn) => {
    try { fn(); return "no-error"; } catch (e) { return e.name; }
  };

  check("typeof cookieStore === 'object'", typeof cookieStore === "object");
  check("typeof CookieStore === 'function'", typeof CookieStore === "function");
  check("new CookieStore() throws TypeError", throwName(() => new CookieStore()) === "TypeError");

  // ---- set/get/getAll/delete roundtrip ----
  await cookieStore.set("a", "1");
  const item = await cookieStore.get("a");
  check("get returns the item", !!item && item.name === "a" && item.value === "1", JSON.stringify(item));
  check("CookieListItem keys are [name,value]", JSON.stringify(Object.keys(item)) === '["name","value"]', JSON.stringify(Object.keys(item)));
  check("getAll includes the cookie", (await cookieStore.getAll()).some((c) => c.name === "a" && c.value === "1"));
  check("getAll(name) filters by name", (await cookieStore.getAll("a")).length === 1);
  check("get({name}) works", (await cookieStore.get({ name: "a" })).value === "1");
  check("get(missing) returns null", (await cookieStore.get("missing")) === null);
  await cookieStore.delete("a");
  check("delete(name) removes it", (await cookieStore.get("a")) === null);

  // ---- normalization (trim TAB/SPACE) ----
  await cookieStore.set("  pad  ", "  v  ");
  const pad = await cookieStore.get("pad");
  check("name/value are normalized", !!pad && pad.name === "pad" && pad.value === "v", JSON.stringify(pad));
  await cookieStore.delete("pad");

  // ---- argument overload / required-member checks ----
  check("get() rejects TypeError", (await rejectName(() => cookieStore.get())) === "TypeError");
  check("get({}) rejects TypeError", (await rejectName(() => cookieStore.get({}))) === "TypeError");
  check("getAll() resolves to an array", Array.isArray(await cookieStore.getAll()));
  check("getAll(undefined) resolves to an array", Array.isArray(await cookieStore.getAll(undefined)));
  check("set() throws TypeError", throwName(() => cookieStore.set()) === "TypeError");
  check("set({}) throws TypeError", throwName(() => cookieStore.set({})) === "TypeError");
  check("set missing value throws TypeError", throwName(() => cookieStore.set({ name: "x" })) === "TypeError");
  check("set bad sameSite throws TypeError", throwName(() => cookieStore.set({ name: "x", value: "1", sameSite: "bogus" })) === "TypeError");
  check("delete() throws TypeError", throwName(() => cookieStore.delete()) === "TypeError");
  check("delete({}) throws TypeError", throwName(() => cookieStore.delete({})) === "TypeError");

  // ---- algorithm failures reject with TypeError ----
  check("name with '=' rejects", (await rejectName(() => cookieStore.set({ name: "a=b", value: "1" }))) === "TypeError");
  check("empty name+value rejects", (await rejectName(() => cookieStore.set({ name: "", value: "" }))) === "TypeError");
  check("invalid char rejects", (await rejectName(() => cookieStore.set({ name: "a;b", value: "1" }))) === "TypeError");
  check("path without '/' rejects", (await rejectName(() => cookieStore.set({ name: "p", value: "1", path: "x" }))) === "TypeError");
  check("expires+maxAge rejects", (await rejectName(() => cookieStore.set({ name: "p", value: "1", expires: Date.now() + 1000, maxAge: 5 }))) === "TypeError");
  check("foreign domain rejects", (await rejectName(() => cookieStore.set({ name: "p", value: "1", domain: "other.test" }))) === "TypeError");
  check("__Host- with domain rejects", (await rejectName(() => cookieStore.set({ name: "__Host-x", value: "1", domain: "example.com" }))) === "TypeError");
  check("__Host- with non-/ path rejects", (await rejectName(() => cookieStore.set({ name: "__Host-x", value: "1", path: "/sub/" }))) === "TypeError");
  check("options.url mismatch rejects", (await rejectName(() => cookieStore.get({ name: "x", url: "https://other.test/" }))) === "TypeError");
  check("options.url equal resolves", (await cookieStore.get({ name: "x", url: "https://example.com/" })) === null);

  // ---- expires / maxAge ----
  await cookieStore.set({ name: "future", value: "1", expires: Date.now() + 60000 });
  check("future expires is visible", (await cookieStore.getAll("future")).length === 1);
  await cookieStore.set({ name: "past", value: "1", expires: Date.now() - 60000 });
  check("past expires is evicted", (await cookieStore.getAll("past")).length === 0);
  await cookieStore.set({ name: "age", value: "1", maxAge: 60 });
  check("maxAge cookie is visible", (await cookieStore.getAll("age")).length === 1);
  await cookieStore.delete({ name: "future" });
  await cookieStore.delete({ name: "age" });
`;

// ===================================================================
// 2. opaque origin（file://）→ SecurityError
// ===================================================================
const OPAQUE_TEST = `
  try {
    await cookieStore.get("x");
    check("opaque origin rejects with SecurityError", false, "resolved unexpectedly");
  } catch (e) {
    check("opaque origin rejects with SecurityError", e.name === "SecurityError", e.name);
  }
`;

// ===================================================================
// 3. 与 fetch 共用同一 store
// ===================================================================
const PORT = 8158;
const BASE = `http://127.0.0.1:${PORT}`;
const server = new HttpServer();
server
  .get("/set", (_req, res) => {
    res.setHeader("Set-Cookie", "srv=xyz; Path=/");
    res.end("ok");
  })
  .get("/echo", (req, res) => res.end(req.headers["cookie"] ?? ""));

const INTEGRATION_TEST = `
  // fetch only auto-uses the jar when its cookie switch is on.
  fetch.cookie = true;

  // fetch stores a cookie → cookieStore sees it
  await fetch("${BASE}/set");
  const all = await cookieStore.getAll();
  check("fetch Set-Cookie is visible to cookieStore", all.some((c) => c.name === "srv" && c.value === "xyz"), JSON.stringify(all));

  // cookieStore writes a cookie → fetch sends it
  await cookieStore.set("api", "v");
  const echoed = await (await fetch("${BASE}/echo")).text();
  check("cookieStore.set is sent by fetch", echoed.includes("api=v"), echoed);

  // cookieStore deletes a cookie → fetch no longer sends it
  await cookieStore.delete("srv");
  const echo2 = await (await fetch("${BASE}/echo")).text();
  check("cookieStore.delete removes it from fetch", !echo2.includes("srv=xyz"), echo2);
`;

async function main() {
  console.log("\n=== Cookie Store API ===");
  await Aike.evalModule(API_TESTS, "https://example.com/");

  console.log("\n=== Opaque origin ===");
  await Aike.evalModule(OPAQUE_TEST, "file:///tmp/aike-cookie-store-test.js");

  console.log("\n=== Shared store with fetch ===");
  server.listen({ port: PORT, host: "127.0.0.1", cors: false });
  await new Promise((r) => setTimeout(r, 200));
  try {
    await Aike.evalModule(INTEGRATION_TEST, `${BASE}/`);
  } finally {
    await server.shutdown();
  }

  console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
  console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
}

if (HAS_FEATURE) {
  main().catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
  });
}
