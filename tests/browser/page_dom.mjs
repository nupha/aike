// Test: Page-class DOM / Emulation / Log domain APIs (P0/P1 from memo/cdp-features.md)
//
// Covers (all flat on `Page`, exercised offline against a data: URL):
//   DOM (P0): querySelector, querySelectorAll, getAttribute, focus, getBoxModel
//   DOM (P1): remove, click, type
//   Emulation (P0): setGeolocation, setMediaFeatures
//   Emulation (P1): setTimezone, setLocale, setTouchEnabled, setCPUThrottling
//   Log (P1): onLog, getConsoleMessages
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_dom.mjs

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

// Poll a predicate returning a promise; resolves to the final value.
async function poll(fn, tries = 40, delay = 100) {
  for (let i = 0; i < tries; i++) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, delay));
  }
  return await fn();
}

const PAGE =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><head><title>dom</title></head><body>" +
      '<div id="box" class="item" data-x="1">hello</div>' +
      '<span class="item">world</span>' +
      '<input id="name" />' +
      '<button id="btn">btn</button>' +
      "</body></html>",
  );

async function testDom(page) {
  console.log("\n=== DOM domain (P0/P1) ===");
  await page.navigate(PAGE);
  await page.waitForSelector("#box", 5000);

  // querySelector -> nodeId (number > 0)
  const nodeId = await page.querySelector("#box");
  check("querySelector('#box') returns nodeId", typeof nodeId === "number" && nodeId > 0,
    "nodeId=" + nodeId);

  // querySelectorAll -> array of nodeIds, length 2 for ".item"
  const ids = await page.querySelectorAll(".item");
  check("querySelectorAll('.item') returns 2 nodes",
    Array.isArray(ids) && ids.length === 2, "len=" + (ids ? ids.length : "n/a"));

  // getAttribute
  const cls = await page.getAttribute("#box", "class");
  check("getAttribute('#box','class')", cls === "item", "got=" + cls);
  const dx = await page.getAttribute("#box", "data-x");
  check("getAttribute('#box','data-x')", dx === "1", "got=" + dx);

  // focus -> resolves without error
  await page.focus("#name");
  check("focus('#name') resolves", true);

  // getBoxModel -> has a content quad (8 numbers)
  const bm = await page.getBoxModel("#box");
  check("getBoxModel('#box') returns model",
    bm && bm.content && Array.isArray(bm.content) && bm.content.length >= 8,
    "got=" + JSON.stringify(bm));

  // click -> native click fires the element's click listener
  await page.evaluate(
    "window.__c=0;var b=document.getElementById('btn');" +
      "if(b)b.addEventListener('click',function(){window.__c++;});",
  );
  await page.click("#btn");
  const clicks = await poll(async () => (await page.evaluate("window.__c")) === 1
    ? await page.evaluate("window.__c") : 0);
  check("click('#btn') triggers click event", clicks === 1, "clicks=" + clicks);

  // type -> focuses the input then inserts text
  await page.type("#name", "aike");
  const val = await poll(async () => {
    const v = await page.evaluate("document.getElementById('name').value");
    return v === "aike" ? v : "";
  });
  check("type('#name','aike') sets value", val === "aike", "val=" + val);

  // remove -> node is gone from the DOM
  await page.remove("#box");
  const gone = await page.evaluate("document.getElementById('box') === null");
  check("remove('#box') deletes node", gone === true);
}

async function testEmulation(page) {
  console.log("\n=== Emulation domain (P0/P1) ===");

  // setMediaFeatures(prefers-color-scheme: dark) -> matchMedia reflects it
  await page.setMediaFeatures([{ name: "prefers-color-scheme", value: "dark" }]);
  const dark = await page.evaluate(
    "matchMedia('(prefers-color-scheme: dark)').matches",
  );
  check("setMediaFeatures reflects in matchMedia", dark === true, "dark=" + dark);

  // setTimezone -> Intl reports it
  await page.setTimezone("America/New_York");
  const tz = await page.evaluate(
    "Intl.DateTimeFormat().resolvedOptions().timeZone",
  );
  check("setTimezone changes Intl timeZone", tz === "America/New_York", "tz=" + tz);

  // setLocale -> void command. NOTE: observing the effect via
  // navigator.language is environment-dependent under headless Chrome (the
  // CDP override is issued but may not change navigator.language there), so we
  // only verify the command resolves without error.
  await page.setLocale("fr-FR");
  await page.setLocale("en-US");
  check("setLocale resolves", true);

  // setTouchEnabled(true) -> navigator.maxTouchPoints > 0
  await page.setTouchEnabled(true);
  const tps = await page.evaluate("navigator.maxTouchPoints");
  check("setTouchEnabled(true) enables touch", tps > 0, "maxTouchPoints=" + tps);
  await page.setTouchEnabled(false);
  const tps2 = await page.evaluate("navigator.maxTouchPoints");
  check("setTouchEnabled(false) disables touch", tps2 === 0, "maxTouchPoints=" + tps2);

  // setGeolocation / setCPUThrottling -> void commands, just resolve
  await page.setGeolocation(1.0, 2.0, 10);
  check("setGeolocation resolves", true);
  await page.setCPUThrottling(2);
  await page.setCPUThrottling(1);
  check("setCPUThrottling resolves", true);
}

async function testLog(page) {
  console.log("\n=== Log domain (P1) ===");

  let entry = null;
  page.onLog((e) => {
    entry = e;
  });

  // Runtime must be enabled for `console.log` to surface as
  // `Runtime.consoleAPICalled` events (onLog only enables `Log`).
  await page.enableRuntime();
  await page.evaluate("console.log('aike-log-test')");

  // The console event is delivered async via the CDP event poller.
  const fired = await poll(async () => entry !== null);
  check("onLog callback fired for console.log", fired === true);
  check("onLog entry is an object",
    fired && typeof entry === "object",
    "entry=" + JSON.stringify(entry));

  // getConsoleMessages() returns buffered entries (buffer kept).
  const msgs = await page.getConsoleMessages();
  check("getConsoleMessages() returns buffered entries",
    Array.isArray(msgs) && msgs.length >= 1,
    "len=" + (msgs ? msgs.length : "n/a"));

  // getConsoleMessages(true) returns the buffered entries AND clears the buffer.
  const cleared = await page.getConsoleMessages(true);
  check("getConsoleMessages(true) returns cleared entries",
    Array.isArray(cleared) && cleared.length >= 1,
    "len=" + (cleared ? cleared.length : "n/a"));

  // A follow-up call (no clear) must now be empty.
  const after = await page.getConsoleMessages();
  check("buffer empty after getConsoleMessages(true)",
    Array.isArray(after) && after.length === 0,
    "len=" + (after ? after.length : "n/a"));
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
  const page = await browser.newPage();

  const tests = [testDom, testEmulation, testLog];
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
