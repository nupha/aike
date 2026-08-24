// Test: Page-class CSS domain APIs (P3 from memo/cdp-features.md)
//
// Covers (all flat on `Page`):
//   CSS (P3): getComputedStyle, getMatchedStyles
//
// Fully offline against a data: URL with inline + <style> rules so computed
// and matched styles are observable.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_css.mjs

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
    "<!DOCTYPE html><html><head><style>" +
      "#box{color:rgb(255,0,0);font-weight:bold;}" +
      ".item{margin:5px;}" +
      "</style></head><body>" +
      '<div id="box" class="item" style="padding:10px;">hello</div>' +
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

    console.log("\n=== CSS domain (P3) ===");

    // getComputedStyle -> object name->value; inline style wins.
    const cs = await page.getComputedStyle("#box");
    check("getComputedStyle('#box') returns an object",
      cs && typeof cs === "object", "cs=" + JSON.stringify(cs));
    check("getComputedStyle reflects inline padding:10px",
      cs && (cs["padding-top"] === "10px" || /10px/.test(cs["padding"] || "")),
      "padding=" + (cs && (cs["padding-top"] || cs["padding"])));
    check("getComputedStyle reflects <style> color",
      cs && cs["color"] === "rgb(255, 0, 0)", "color=" + (cs && cs["color"]));

    // getMatchedStyles -> raw CDP result JSON with matched rules.
    const ms = await page.getMatchedStyles("#box");
    check("getMatchedStyles('#box') returns data",
      ms && (ms.matchedCSSRules !== undefined || Array.isArray(ms.matchedCSSRules)),
      "ms=" + JSON.stringify(ms).slice(0, 120));
  } catch (e) {
    console.log("  [FAIL] css test threw: " + (e && (e.message || e)));
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
