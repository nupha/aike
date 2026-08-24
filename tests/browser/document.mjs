// Test: Page-domain document APIs (see aike/memo/cdp-features.md:66-75)
//
// Covers: page.setContent(html), page.reload(opts?)
//
// These replace/reload the document, which is destructive to the bing demo
// flow in tests/browser.mjs, so they live in their own offline script.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/document.mjs

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

const PAGE_A =
  "data:text/html," +
  "<!DOCTYPE html><html><head><title>A</title></head>" +
  "<body><h1 id='a'>Page A</h1></body></html>";

async function testSetContent(page) {
  console.log("\n=== page.setContent(html) ===");
  await page.navigate(PAGE_A);
  await page.setContent(
    "<html><head><title>SetContent</title></head>" +
      "<body><div id='x'>replaced</div></body></html>",
  );
  await page.waitForSelector("#x", 5000);
  const title = await page.evaluate("document.title");
  check("setContent replaced the document", title === "SetContent", "got: " + title);
}

async function testReload(page) {
  console.log("\n=== page.reload(opts?) ===");
  await page.navigate(PAGE_A);
  await page.evaluate("window.__marker = 99");
  await page.reload({ ignoreCache: true });
  // After reload a fresh document is loaded; the marker must be gone.
  let cleared = false;
  for (let i = 0; i < 40; i++) {
    try {
      const m = await page.evaluate("window.__marker");
      if (m === undefined) {
        cleared = true;
        break;
      }
    } catch (e) {
      // evaluate may error briefly during navigation; ignore and retry.
    }
  }
  check("reload reloaded the page (state reset)", cleared);
  const title = await page.evaluate("document.title");
  check("page alive after reload", title === "A", "got: " + title);
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

  const tests = [testSetContent, testReload];
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
