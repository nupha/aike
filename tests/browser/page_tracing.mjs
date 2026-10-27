// Test: Page-class Tracing domain APIs
//
// Covers (all flat on `Page`):
//   Tracing (P3): startTracing, stopTracing
//   (stopTracing internally drains the stream via the IO domain `readStream`)
//
// Fully offline against a data: URL. We start tracing, do a little work to
// generate trace events, then stop and check the returned trace is non-empty
// JSON.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_tracing.mjs

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
    "<!DOCTYPE html><html><body><div id='box'>tracing</div></body></html>",
  );

async function main() {
  setTimeout(() => {
    console.log("  [WATCHDOG] forcing exit after 30s (a step likely hung)");
    process.exit(1);
  }, 30000);

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
    console.log("\n=== Tracing domain (P3) ===");

    await page.startTracing("blink,devtools,toplevel");
    check("startTracing resolves", true);

    // Generate some activity so the trace has events.
    await page.navigate(PAGE);
    await page.waitForSelector("#box", 5000);
    await page.evaluate("for(let i=0;i<1000;i++){Math.sqrt(i);}");

    const trace = await page.stopTracing();
    check("stopTracing returns a string", typeof trace === "string",
      "len=" + (trace ? trace.length : "n/a"));

    let parsed = null;
    try { parsed = JSON.parse(trace); } catch (e) {}
    const events = parsed && parsed.traceEvents;
    check("trace is a JSON object with traceEvents array",
      parsed && typeof parsed === "object" && Array.isArray(events) && events.length > 0,
      "events=" + (events ? events.length : "n/a"));
  } catch (e) {
    console.log("  [FAIL] tracing test threw: " + (e && (e.message || e)));
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
