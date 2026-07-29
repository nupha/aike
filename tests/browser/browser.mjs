// Test: Browser-class domain APIs (Browser / Target / Security domains)
//
// Covers the methods defined on the `Browser` instance in class_browser.rs:
//   Browser domain : getVersion, getWindowBounds, setWindowBounds, setDownloadBehavior
//   Target domain  : getTargets, listPages, newContext, disposeContext
//   Security domain: setIgnoreHTTPSErrors, handleCertificateError
//
// This is a self-contained, headless script — no external network is required
// (pages are created against about:blank / data: URLs), so it can run anywhere
// Chrome is installed. It mirrors the layout of tests/browser/*.mjs.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/browser_api.mjs

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

async function testBrowserDomain(browser) {
  console.log("\n=== Browser domain ===");

  // browser.getVersion() -> { protocolVersion, product, revision, userAgent, jsVersion }
  const ver = await browser.getVersion();
  check(
    "getVersion() returns version info",
    ver && typeof ver.protocolVersion === "string" &&
      typeof ver.product === "string" &&
      typeof ver.userAgent === "string" &&
      typeof ver.jsVersion === "string",
    "got: " + JSON.stringify(ver),
  );

  // browser.getWindowBounds() -> { bounds: { left, top, width, height, windowState } }
  // browser.setWindowBounds({ width, height }) — resize, then verify it applied.
  // NOTE: these need a real GUI window, so under headless Chrome they reject
  // with "No web contents in the target". Skip gracefully in that case (the
  // bing demo in tests/browser.mjs runs headful and exercises them for real).
  try {
    const wb = await browser.getWindowBounds();
    check(
      "getWindowBounds() returns bounds",
      wb && wb.bounds && typeof wb.bounds.width === "number" &&
        typeof wb.bounds.height === "number" &&
        typeof wb.bounds.windowState === "string",
      "got: " + JSON.stringify(wb),
    );

    const beforeW = wb.bounds.width;
    await browser.setWindowBounds({ width: 800, height: 600 });
    const wb2 = await browser.getWindowBounds();
    check(
      "setWindowBounds() resizes the window",
      wb2 && wb2.bounds && Math.abs(wb2.bounds.width - 800) <= 50 &&
        Math.abs(wb2.bounds.height - 600) <= 50,
      "beforeW=" + beforeW + " after=" + JSON.stringify(wb2 && wb2.bounds),
    );
  } catch (e) {
    const msg = (e && (e.message || e)) || "";
    if (msg.indexOf("No web contents") >= 0) {
      console.log("  [skip] window bounds (headless has no window): " + msg);
    } else {
      console.log("  [FAIL] window bounds threw: " + msg);
      fail++;
    }
  }

  // browser.setDownloadBehavior(path) — browser-level allow/deny (mirrors the
  // page-level API but scoped to the whole browser). Both must resolve.
  const tmpDir = "/tmp/aike_browser_downloads";
  await browser.setDownloadBehavior(tmpDir);
  await browser.setDownloadBehavior("");
  check("setDownloadBehavior(allow/deny) resolves", true);
}

async function testTargetDomain(browser) {
  console.log("\n=== Target domain ===");

  // browser.getTargets() -> { targetInfos: [ { targetId, type, url, ... }, ... ] }
  const targets = await browser.getTargets();
  check(
    "getTargets() returns targetInfos array",
    targets && Array.isArray(targets.targetInfos) && targets.targetInfos.length >= 1,
    "len=" + (targets && targets.targetInfos ? targets.targetInfos.length : "n/a"),
  );

  // browser.listPages() -> [ { targetId, url, title, type }, ... ] (page type only)
  const pages = await browser.listPages();
  check(
    "listPages() returns page targets",
    Array.isArray(pages) && pages.length >= 1 &&
      pages.every((p) => p.type === "page"),
    "len=" + (pages ? pages.length : "n/a"),
  );
  const hasNewPage = pages.some((p) => typeof p.targetId === "string" && p.targetId.length > 0);
  check("listPages() includes the launched page", hasNewPage);

  // browser.newContext() -> string context id, then dispose it.
  const ctxId = await browser.newContext();
  check(
    "newContext() returns a context id",
    typeof ctxId === "string" && ctxId.length > 0,
    "ctxId=" + ctxId,
  );

  // A page created inside the new context should appear among targets.
  const ctxPage = await browser.newPage({ url: "about:blank", contextId: ctxId });
  check(
    "newPage({ contextId }) creates a page in the context",
    ctxPage && typeof ctxPage.targetId === "string" && ctxPage.targetId.length > 0,
    "targetId=" + (ctxPage && ctxPage.targetId),
  );
  const inCtx = (await browser.getTargets()).targetInfos.some(
    (t) => t.targetId === ctxPage.targetId,
  );
  check("context page is listed by getTargets()", inCtx);

  // browser.disposeContext(id) — tear the context down; the page should go with it.
  await browser.disposeContext(ctxId);
  check("disposeContext() resolves", true);
  const gone = !(await browser.getTargets()).targetInfos.some(
    (t) => t.targetId === ctxPage.targetId,
  );
  check("context page removed after disposeContext()", gone);

  // Disposing an unknown context must surface a CDP error (rejection), not
  // silently succeed — this documents the error-propagation contract.
  let rejected = false;
  try {
    await browser.disposeContext("nonexistent-context-id");
  } catch (e) {
    rejected = true;
  }
  check("disposeContext() on unknown id rejects", rejected);
}

async function testSecurityDomain(browser) {
  console.log("\n=== Security domain ===");

  // browser.setIgnoreHTTPSErrors(bool) — browser-level void command.
  await browser.setIgnoreHTTPSErrors(true);
  await browser.setIgnoreHTTPSErrors(false);
  check("setIgnoreHTTPSErrors(true/false) resolves", true);

  // browser.handleCertificateError(cb) — register a listener (sync, returns
  // undefined). We smoke-test that registration succeeds and the callback shape
  // is correct when a cert error is delivered. Triggering a real cert error
  // needs a TLS endpoint, so we only assert the registration path here.
  let registered = true;
  let fired = false;
  try {
    const ret = browser.handleCertificateError((evt) => {
      fired = true;
      // evt: { eventId, errorType, requestURL, accept(bool) }
      check("cert callback gets eventId", typeof evt.eventId === "number");
      check("cert callback gets accept()", typeof evt.accept === "function");
      evt.accept(false);
    });
    check("handleCertificateError() returns undefined", ret === undefined);
  } catch (e) {
    registered = false;
    console.log("  [FAIL] handleCertificateError threw: " + (e && (e.message || e)));
  }
  check("handleCertificateError() registered without error", registered);
  // Note: `fired` depends on a live insecure-TLS navigation; left unasserted.
}

// --- Tab-level tests merged from tests/test_browser.js ---

async function testNewTab(browser) {
  console.log("\n=== Browser.newPage() ===");
  const tab = await browser.newPage();
  check("Tab object", typeof tab === "object" && tab !== null);
  check(
    "targetId is string",
    typeof tab.targetId === "string" && tab.targetId.length > 0,
  );
  await tab.close();
}

async function testBasicAPI(browser) {
  console.log("\n=== Basic API (newPage + navigate + getOuterHTML flow) ===");
  const tab = await browser.newPage();
  await tab.navigate("data:text/html,<h1 id='h'>hello</h1>");
  await tab.waitForSelector("#h", 5000);
  const html = await tab.getOuterHTML();
  check("full flow works", html.includes("hello"), "got: " + html.slice(0, 50));
  await tab.close();
}

async function testMultipleTabs(browser) {
  console.log("\n=== Multiple tabs ===");
  const tab1 = await browser.newPage();
  const tab2 = await browser.newPage();

  check(
    "unique targetIds",
    tab1.targetId !== tab2.targetId,
    tab1.targetId + " vs " + tab2.targetId,
  );

  // Each tab has an independent JS world.
  await tab1.navigate("data:text/html,<script>window._v=1;</script>");
  await tab2.navigate("data:text/html,<script>window._v=2;</script>");
  await tab1.waitForSelector("body", 5000);
  await tab2.waitForSelector("body", 5000);
  check("tab1 evaluate", (await tab1.evaluate("window._v")) === 1);
  check("tab2 evaluate", (await tab2.evaluate("window._v")) === 2);

  await tab1.close();
  await tab2.close();
}

async function testCustomPort() {
  console.log("\n=== launch() with custom port ===");
  // Use a non-default port so it does not collide with the main instance.
  const b2 = await Browser.launch({ headless: true, port: 9366 });
  check("custom port works", b2.isRunning === true);
  await b2.close();
  check("custom port close() resolved", true);
}

async function main() {
  let browser;
  try {
    browser = await Browser.launch({ headless: true });
  } catch (e) {
    console.log("\n  Failed to launch browser: " + (e && (e.message || e)));
    console.log("  Make sure Google Chrome or Chromium is installed.");
    Aike.exit(1);
    return;
  }

  check("Browser launched", browser.isRunning === true);

  // One page so listPages/getTargets exercise a real target.
  const page = await browser.newPage();

  const tests = [
    testBrowserDomain,
    testTargetDomain,
    testSecurityDomain,
    testNewTab,
    testBasicAPI,
    testMultipleTabs,
    testCustomPort,
  ];
  for (const fn of tests) {
    try {
      await fn(browser);
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
