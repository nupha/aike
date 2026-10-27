// Test: Page-domain navigation APIs
//
// Covers: page.getNavigationHistory(), page.back(), page.forward()
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/navigation.mjs

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

// Two distinct pages build a 2-entry navigation history for back/forward tests.
const PAGE_A =
  "data:text/html," +
  "<!DOCTYPE html><html><head><title>A</title></head>" +
  "<body><h1 id='a'>Page A</h1></body></html>";
const PAGE_B =
  "data:text/html," +
  "<!DOCTYPE html><html><head><title>B</title></head>" +
  "<body><h1 id='b'>Page B</h1></body></html>";

async function testNavigationHistory(page) {
  console.log("\n=== page.getNavigationHistory() / back() / forward() ===");
  await page.navigate(PAGE_A);
  await page.waitForSelector("#a", 5000);
  await page.navigate(PAGE_B);
  await page.waitForSelector("#b", 5000);

  const h = await page.getNavigationHistory();
  check("history.currentIndex is a number", typeof h.currentIndex === "number");
  check(
    "history.entries is an array (>= 2 entries)",
    h.entries && typeof h.entries.length === "number" && h.entries.length >= 2,
    "len=" + (h.entries ? h.entries.length : "n/a"),
  );

  const before = h.currentIndex;
  await page.back();
  // back()/forward() navigate via history entry; waitForSelector can be
  // discarded by the in-flight navigation, so poll document.title instead.
  let titleB = null;
  for (let i = 0; i < 40; i++) {
    try {
      titleB = await page.evaluate("document.title");
      if (titleB === "A") break;
    } catch (e) {}
  }
  const h2 = await page.getNavigationHistory();
  check("back() landed on PAGE_A", titleB === "A", "title=" + titleB);
  check(
    "back() decreased currentIndex",
    h2.currentIndex < before,
    "before=" + before + " after=" + h2.currentIndex,
  );

  await page.forward();
  let titleF = null;
  for (let i = 0; i < 40; i++) {
    try {
      titleF = await page.evaluate("document.title");
      if (titleF === "B") break;
    } catch (e) {}
  }
  const h3 = await page.getNavigationHistory();
  check("forward() landed on PAGE_B", titleF === "B", "title=" + titleF);
  check(
    "forward() increased currentIndex",
    h3.currentIndex > h2.currentIndex,
    "before=" + h2.currentIndex + " after=" + h3.currentIndex,
  );
}

async function testNavigate(page) {
  console.log("\n=== page.navigate() ===");
  await page.navigate(PAGE_A);
  await page.waitForSelector("#a", 5000);
  const title = await page.evaluate("document.title");
  check("navigated to test page", title === "A", "got: " + title);
}

async function testWaitForNavigation(page) {
  console.log("\n=== page.waitForNavigation() native CDP event ===");
  // Subscribe BEFORE navigating (Playwright order) — the load event fires
  // during navigation, so a late subscriber would miss it and time out.
  const nav1 = page.waitForNavigation(10000);
  await page.navigate(PAGE_A);
  await nav1;
  check("waitForNavigation on first load", true);

  const nav2 = page.waitForNavigation(10000);
  await page.navigate(PAGE_B);
  await nav2;
  check("waitForNavigation on second navigation", true);

  const text = await page.evaluate("document.body.innerText");
  check("page content loaded", text.includes("Page B"), "got: " + text);
}

async function testAddInitScript(page) {
  console.log("\n=== page.addInitScript() ===");
  // Inject a script that sets a global before any page JS runs.
  const scriptId = await page.addInitScript(
    "window.__injectedByTest = 'injected';",
  );
  check(
    "addInitScript returns identifier",
    typeof scriptId === "string" && scriptId.length > 0,
  );

  const nav = page.waitForNavigation(10000);
  await page.navigate(
    "data:text/html,<script>window.__pageVar = window.__injectedByTest;</script>",
  );
  await nav;

  const injected = await page.evaluate("window.__injectedByTest");
  check("injected script ran", injected === "injected", "got: " + injected);

  const pageVar = await page.evaluate("window.__pageVar");
  check(
    "page can read injected var",
    pageVar === "injected",
    "got: " + pageVar,
  );
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

  const tests = [
    testNavigate,
    testNavigationHistory,
    testWaitForNavigation,
    testAddInitScript,
  ];
  for (const fn of tests) {
    try {
      await fn(page);
    } catch (e) {
      console.log("  [FAIL] " + fn.name + " threw: " + (e && (e.message || e)));
      fail++;
    }
  }

  try {
    await page.close();
  } catch (e) {}
  try {
    await browser.close();
  } catch (e) {}

  console.log("\n" + "=".repeat(50));
  console.log("pass: " + pass + ", fail: " + fail);
  if (fail > 0) process.exit(1);
  else process.exit(0);
}

main();
