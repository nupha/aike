// Test: bing search via CDP — query, extract DOM results, print, then close
//
// Prerequisites: Google Chrome / Chromium installed on the system.
// Launches Chrome in non-headless mode so you can see the browser window.

import { Browser } from "browser";

let apiPass = 0;
let apiFail = 0;

function check(label, condition, detail) {
  if (condition) {
    console.log("  [ok] " + label);
    apiPass++;
  } else {
    console.log("  [FAIL] " + label + (detail ? " -- " + detail : ""));
    apiFail++;
  }
}

async function main() {
  console.log("=== CDP Browser: bing search demo ===\n");

  // 1. launch browser
  let browser;
  try {
    browser = await Browser.launch({ headless: false, port: 9366 });
  } catch (err) {
    console.log("  Launch failed:", err.message || err);
    Aike.exit(1);
  }
  console.log("  Browser launched, isRunning:", browser.isRunning);

  // 2. create page + set viewport
  const page = await browser.newPage();
  console.log("  New page created, targetId:", page.targetId);
  await page.setViewport(1280, 800);
  console.log("  Viewport: 1280x800\n");

  // 3. go to bing.com
  console.log("  Navigate to bing.com ...");
  await page.navigate("https://www.bing.com");
  await page.waitForNavigation(15000);
  console.log("  Bing homepage loaded");

  // 4. type query into the search box and submit
  const query = "hello world";
  console.log("  Searching for:", query, "\n");
  await page.evaluate(`(function() {
    var input = document.querySelector('input[name="q"]');
    if (!input) throw new Error("search input not found");
    input.value = "${query}";
    // dispatch input event so bing reacts
    input.dispatchEvent(new Event("input", { bubbles: true }));
    // submit the search form
    var form = document.getElementById("sb_form");
    if (form) form.submit();
    else input.form.submit();
  })()`);

  // 5. wait for the search results page to load
  await page.waitForNavigation(15000);
  console.log("  Search results page loaded\n");

  // 6. extract search results from the DOM
  const results = await page.evaluate(`(function() {
    var items = document.querySelectorAll("#b_results .b_algo");
    var out = [];
    for (var i = 0; i < items.length; i++) {
      var el = items[i];
      var link = el.querySelector("h2 a");
      var snippet = el.querySelector(".b_caption p");
      out.push({
        title: link ? link.innerText.trim() : "",
        url: link ? link.href : "",
        snippet: snippet ? snippet.innerText.trim() : "",
      });
    }
    return out;
  })()`);

  // 7. print results
  console.log("=== Search Results ===");
  if (results.length === 0) {
    console.log(
      "  (no results found via #b_results .b_algo, trying fallback)\n",
    );
    // fallback: dump the page title and a snippet of text
    const fallback = await page.evaluate(`(function() {
      return {
        title: document.title,
        text: document.body.innerText.substring(0, 500)
      };
    })()`);
    console.log("  Page title:", fallback.title);
    console.log("  Body text:", fallback.text);
  } else {
    for (var i = 0; i < results.length; i++) {
      var r = results[i];
      console.log("  [" + (i + 1) + "] " + r.title);
      console.log("      " + r.url);
      if (r.snippet) console.log("      " + r.snippet);
      console.log("");
    }
  }
  console.log("=== End of Results ===\n");

  // 7b. exercise new Page-domain APIs that are safe on any loaded page
  //     (these don't mutate or reload the document, so the bing demo above
  //     keeps working). See aike/memo/cdp-features.md:66-75. The destructive
  //     APIs (setContent/reload + history back/forward) live in
  //     tests/browser/document.mjs and tests/browser/navigation.mjs.
  console.log("=== Exercise Page-domain APIs (on bing page) ===");
  const tmpDir = "/tmp/aike_downloads";

  await page.stopLoading();
  check("stopLoading() resolves", true);

  await page.setBypassCSP(true);
  await page.setBypassCSP(false);
  check("setBypassCSP(true/false) resolves", true);

  await page.setDownloadBehavior(tmpDir);
  await page.setDownloadBehavior("");
  check("setDownloadBehavior(allow/deny) resolves", true);

  await page.bringToFront();
  check("bringToFront() resolves", true);

  // captureSnapshot needs the CDP session attached; a readyState evaluate
  // forces the session active first.
  await page.evaluate("document.readyState");
  const mhtml = await page.captureSnapshot();
  check(
    "captureSnapshot() returns MHTML",
    typeof mhtml === "string" && mhtml.length > 0 &&
      (mhtml.indexOf("MIME-Version") >= 0 || mhtml.indexOf("multipart/related") >= 0),
  );

  // 8. scroll to bottom, then wait
  console.log("  Scrolling to bottom of page...");
  await page.evaluate(
    "window.scrollTo(0, document.documentElement.scrollHeight)",
  );
  console.log("  Waiting 5 seconds...");
  await new Promise((resolve) => setTimeout(resolve, 5_000));

  // 9. close tab + browser
  console.log("  Closing tab...");
  await page.close();
  console.log("  Closing browser...");
  await browser.close();
  console.log("\nDone.");

  console.log("\n" + "=".repeat(50));
  console.log("Page-API checks on bing page: pass " + apiPass + ", fail " + apiFail);
  Aike.exit(apiFail > 0 ? 1 : 0);
}

main().catch((err) => {
  console.log("Unhandled error:", err.message || err);
  Aike.exit(1);
});
