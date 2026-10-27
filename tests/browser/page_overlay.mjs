// Test: Page-class Overlay domain APIs
//
// Covers (all flat on `Page`):
//   Overlay (P2): highlight, hideHighlight
//
// Fully offline against a data: URL. The highlight is visual only, so we just
// verify the commands resolve without error.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_overlay.mjs

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

const PAGE =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><body>" +
      '<div id="box" class="item">hello</div>' +
      "</body></html>",
  );

async function main() {
  setTimeout(() => {
    console.log("  [WATCHDOG] forcing exit after 25s (a step likely hung)");
    process.exit(1);
  }, 25000);

  let browser;
  try {
    browser = await Browser.launch({ headless: true });
  } catch (e) {
    console.log("\n  Failed to launch browser: " + (e && (e.message || e)));
    process.exit(1);
    return;
  }

  check("Browser launched", browser.isRunning === true);
  const page = await browser.newPage();

  try {
    await page.navigate(PAGE);
    await page.waitForSelector("#box", 5000);

    console.log("\n=== Overlay domain (P2) ===");

    await page.highlight("#box");
    check("highlight('#box') resolves", true);

    await page.hideHighlight();
    check("hideHighlight() resolves", true);
  } catch (e) {
    console.log("  [FAIL] overlay test threw: " + (e && (e.message || e)));
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
