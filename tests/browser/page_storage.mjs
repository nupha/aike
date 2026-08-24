// Test: Page-class Storage domain APIs (P2/P3 from memo/cdp-features.md)
//
// Covers (all flat on `Page`):
//   Storage (P2): clearData
//   Storage (P3): getStorageKeyForFrame
//
// Offline: we intercept a navigation to http://localhost:1/storage so the page
// gets a real origin (http://localhost:1) that Storage APIs accept; Fetch
// fulfills it, so no server is needed.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_storage.mjs

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

async function main() {
  // Watchdog: never let the suite hang the tool — force-exit after 25s.
  setTimeout(() => {
    console.log("  [WATCHDOG] forcing exit after 25s (a step likely hung)");
    process.exit(1);
  }, 25000);

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

  try {
    // Establish a real origin via request interception.
    page.onRequestPaused((evt) => {
      page.respondRequest(evt.requestId, {
        status: 200,
        body: "<!DOCTYPE html><html><body><div id='box'>storage</div></body></html>",
        contentType: "text/html",
      });
    });
    await page.setRequestInterception(true);
    await page.navigate("http://localhost:1/storage");
    await page.waitForSelector("#box", 5000);
    await page.setRequestInterception(false);

    console.log("\n=== Storage domain (P2/P3) ===");

    // clearData(origin) — void command, must resolve without error.
    const origin = "http://localhost:1";
    await page.clearData(origin);
    check("clearData(origin) resolves", true);

    await page.clearData(origin, "local_storage,cookies");
    check("clearData(origin, types) resolves", true);

    // getStorageKeyForFrame(frameId) — resolve the main frame id first.
    const ft = await page.getFrameTree();
    const frameId = ft && ft.frameTree && ft.frameTree.frame ? ft.frameTree.frame.id : "";
    check("getFrameTree returns a frame id", typeof frameId === "string" && frameId.length > 0,
      "frameId=" + frameId);

    const key = await page.getStorageKeyForFrame(frameId);
    check("getStorageKeyForFrame returns a storage key",
      typeof key === "string" && key.length > 0, "key=" + key);
  } catch (e) {
    console.log("  [FAIL] storage test threw: " + (e && (e.message || e)));
    fail++;
  }

  try { await page.close(); } catch (e) {}
  try { await browser.close(); } catch (e) {}

  console.log("\n" + "=".repeat(50));
  console.log("pass: " + pass + ", fail: " + fail);
  if (fail > 0) process.exit(1);
  else process.exit(0);
}

main();
