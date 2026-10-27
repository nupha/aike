// Test: Page-class Network domain APIs
//
// Covers (all flat on `Page`):
//   Network (P0): setUserAgent, setExtraHTTPHeaders, setOfflineMode
//   Network (P1): setCacheDisabled, setCookies, deleteCookie, clearBrowserCookies
//   Network (cookies): setCookie / getCookies round-trip
//
// All checks are offline-capable (UA override is observable via
// navigator.userAgent; cookies are set against a domain and read back from the
// browser cookie jar). No external network required.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_network.mjs

import { Browser } from "browser";

let pass = 0;
let fail = 0;

function check(label, condition, detail) {
  if (condition) {
    console.log("  [ok] " + label);
    pass++;
  } else {
    console.log("  [FAIL] " + label + (detail ? " -- " + detail : ""));
    fail++;
  }
}

async function testUserAgent(page) {
  console.log("\n=== Network: setUserAgent (P0) ===");
  const before = await page.evaluate("navigator.userAgent");
  await page.setUserAgent("aike-ua/9.9");
  const after = await page.evaluate("navigator.userAgent");
  check("setUserAgent changes navigator.userAgent",
    after === "aike-ua/9.9" && after !== before,
    "before=" + before + " after=" + after);
  // restore
  await page.setUserAgent(before);
}

async function testExtraHeaders(page) {
  console.log("\n=== Network: setExtraHTTPHeaders (P0) ===");
  // Void command — verify it resolves (header delivery needs a real server,
  // covered by the fetch-interception suite).
  await page.setExtraHTTPHeaders({ "X-Aike-Test": "1" });
  check("setExtraHTTPHeaders resolves", true);
}

async function testOfflineCache(page) {
  console.log("\n=== Network: setOfflineMode / setCacheDisabled (P0/P1) ===");
  await page.setOfflineMode(true);
  check("setOfflineMode(true) resolves", true);
  await page.setOfflineMode(false);
  check("setOfflineMode(false) resolves", true);
  await page.setCacheDisabled(true);
  check("setCacheDisabled(true) resolves", true);
  await page.setCacheDisabled(false);
  check("setCacheDisabled(false) resolves", true);
}

async function findCookie(cookies, name) {
  for (const c of cookies) {
    if (c && c.name === name) return c;
  }
  return null;
}

async function testCookies(page) {
  console.log("\n=== Network: cookies (P1) ===");

  // setCookie -> getCookies round-trip
  await page.setCookie({
    name: "aike_c1",
    value: "v1",
    domain: "example.com",
    path: "/",
  });
  let cookies = await page.getCookies();
  let c1 = await findCookie(cookies, "aike_c1");
  check("setCookie/getCookies round-trip (single)", c1 && c1.value === "v1",
    "c1=" + JSON.stringify(c1));

  // setCookies (batch) -> getCookies shows both
  await page.setCookies([
    { name: "aike_c2", value: "v2", domain: "example.com", path: "/" },
  ]);
  cookies = await page.getCookies();
  const c2 = await findCookie(cookies, "aike_c2");
  check("setCookies batch round-trip", c2 && c2.value === "v2",
    "c2=" + JSON.stringify(c2));
  const c1b = await findCookie(cookies, "aike_c1");
  check("previous cookie persists after setCookies", c1b && c1b.value === "v1");

  // deleteCookie -> cookie removed
  await page.deleteCookie("aike_c1", "http://example.com");
  cookies = await page.getCookies();
  const c1gone = await findCookie(cookies, "aike_c1");
  check("deleteCookie removes the cookie", c1gone === null);
  const c2still = await findCookie(cookies, "aike_c2");
  check("other cookie still present after deleteCookie", c2still !== null);

  // clearBrowserCookies -> all gone
  await page.clearBrowserCookies();
  cookies = await page.getCookies();
  const c2gone = await findCookie(cookies, "aike_c2");
  check("clearBrowserCookies removes all", c2gone === null);
}

async function main() {
  let browser;
  try {
    browser = await Browser.launch({ headless: true });
  } catch (e) {
    console.log("\n  Failed to launch browser: " + (e && (e.message || e)));
    console.log("  Make sure Google Chrome or Chromium is installed.");
    process.exit(1);
    return;
  }

  check("Browser launched", browser.isRunning === true);
  const page = await browser.newPage();
  await page.navigate("about:blank");

  const tests = [testUserAgent, testExtraHeaders, testOfflineCache, testCookies];
  for (const fn of tests) {
    try {
      await fn(page);
    } catch (e) {
      console.log("  [FAIL] " + fn.name + " threw: " + (e && (e.message || e)));
      fail++;
    }
  }

  try { await page.close(); } catch (e) {}
  try { await browser.close(); } catch (e) {}

  console.log("\n" + "=".repeat(50));
  console.log("pass: " + pass + ", fail: " + fail);
  if (fail > 0) process.exit(1);
  else process.exit(0);
}

main();
