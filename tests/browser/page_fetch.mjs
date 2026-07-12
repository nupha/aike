// Test: Page-class Fetch domain (request interception) — P0 from memo/cdp-features.md
//
// Covers (all flat on `Page`):
//   Fetch (P0): setRequestInterception, continueRequest, abortRequest,
//               respondRequest, onRequestPaused
//
// Fully offline: we intercept the page's MAIN-FRAME navigation to a
// http://localhost:1/* URL (Fetch catches it before it hits the network, so no
// server is needed) and fulfill / pass-through / block it. Top-level navigation
// has no CORS restrictions, so this is the reliable way to exercise interception.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_fetch.mjs

import { launch } from "browser";

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

// Poll a predicate that returns a promise; resolves to the final value.
async function poll(fn, tries = 40, delay = 100) {
  for (let i = 0; i < tries; i++) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, delay));
  }
  return await fn();
}

async function bodyText(page) {
  try {
    return await page.evaluate("document.body ? document.body.innerText : ''");
  } catch (e) {
    return "";
  }
}

// Navigate (interception catches it) and poll the resulting document body.
async function navigateBody(page, url) {
  await page.navigate(url);
  return await poll(async () => {
    const v = await bodyText(page);
    return v && v.length ? v : "";
  }, 40, 100);
}

async function testRespond(page) {
  console.log("\n=== Fetch: respondRequest (P0) ===");
  let paused = 0;
  page.onRequestPaused((evt) => {
    paused++;
    page.respondRequest(evt.requestId, {
      status: 200,
      body: "mocked",
      contentType: "text/plain",
    });
  });
  await page.setRequestInterception(true);
  const body = await navigateBody(page, "http://localhost:1/x");
  check("onRequestPaused fired", paused >= 1, "paused=" + paused);
  check("respondRequest fulfills navigation with mocked body",
    body === "mocked", "body=" + JSON.stringify(body));
  await page.setRequestInterception(false);
}

async function testContinue(page) {
  console.log("\n=== Fetch: continueRequest (P0) ===");
  let paused = 0;
  page.onRequestPaused((evt) => {
    paused++;
    page.continueRequest(evt.requestId);
  });
  await page.setRequestInterception(true);
  // continueRequest lets it reach the network; offline it fails -> no "mocked".
  const body = await navigateBody(page, "http://localhost:1/y");
  check("onRequestPaused fired", paused >= 1, "paused=" + paused);
  check("continueRequest passes request through (no mocked body)",
    body.indexOf("mocked") < 0, "body=" + JSON.stringify(body));
  await page.setRequestInterception(false);
}

async function testAbort(page) {
  console.log("\n=== Fetch: abortRequest (P0) ===");
  let paused = 0;
  page.onRequestPaused((evt) => {
    paused++;
    page.abortRequest(evt.requestId, "Failed");
  });
  await page.setRequestInterception(true);
  const body = await navigateBody(page, "http://localhost:1/z");
  check("onRequestPaused fired", paused >= 1, "paused=" + paused);
  check("abortRequest blocks the request (no mocked body)",
    body.indexOf("mocked") < 0, "body=" + JSON.stringify(body));
  await page.setRequestInterception(false);
}

async function main() {
  // Watchdog: never let the suite hang the tool — force-exit after 25s.
  setTimeout(() => {
    console.log("  [WATCHDOG] forcing exit after 25s (a step likely hung)");
    Aike.exit(1);
  }, 25000);

  let browser;
  try {
    browser = await launch({ headless: true });
  } catch (e) {
    console.log("\n  Failed to launch browser: " + (e && (e.message || e)));
    console.log("  Make sure Google Chrome or Chromium is installed.");
    Aike.exit(1);
    return;
  }

  check("Browser launched", browser.isRunning === true);
  const page = await browser.newPage();

  const tests = [testRespond, testContinue, testAbort];
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
  if (fail > 0) Aike.exit(1);
  else Aike.exit(0);
}

main();
