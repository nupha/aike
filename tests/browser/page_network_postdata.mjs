// Test: Page-class Network `getRequestPostData` (P2 from memo/cdp-features.md)
//
// Covers (flat on `Page`):
//   Network (P2): getRequestPostData
//
// Offline: we intercept a navigation to http://localhost:1/pd (fulfilled by
// Fetch) to get a real same-origin document, then issue a POST `fetch` to
// /submit. The Fetch pause lets us read the POST body via
// `Network.getRequestPostData`. No server needed.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_network_postdata.mjs

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

// Poll a predicate that returns a promise; resolves to the final value.
async function poll(fn, tries = 40, delay = 100) {
  for (let i = 0; i < tries; i++) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, delay));
  }
  return await fn();
}

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
    let postData = null;

    page.onRequestPaused(async (evt) => {
      // The callback object exposes `requestId` and `request`
      // (with `url`/`method`), per fetch.rs.
      const url = evt.request ? evt.request.url : evt.url;
      const method = evt.request ? evt.request.method : evt.method;
      if (/submit/.test(url) && method === "POST") {
        // `Network.getRequestPostData` needs the *Network* request id, which is
        // `networkId` in the Fetch pause event (falls back to `requestId`).
        const netId = evt.networkId || evt.requestId;
        page.getRequestPostData(netId)
          .then((d) => { postData = d; })
          .catch(() => {});
        await page.respondRequest(evt.requestId, {
          status: 200,
          body: "ok",
          contentType: "text/plain",
        });
      } else if (/pd/.test(url)) {
        await page.respondRequest(evt.requestId, {
          status: 200,
          body: "<!DOCTYPE html><html><body><div id='box'>pd</div></body></html>",
          contentType: "text/html",
        });
      } else {
        await page.continueRequest(evt.requestId);
      }
    });

    await page.setRequestInterception(true);
    await page.navigate("http://localhost:1/pd");
    await page.waitForSelector("#box", 5000);

    console.log("\n=== Network.getRequestPostData (P2) ===");

    // Issue a same-origin POST; the Fetch pause captures the body.
    await page.evaluate(
      "fetch('/submit', { method: 'POST', body: 'a=1&b=2' })",
    );

    const captured = await poll(async () => (postData !== null ? postData : ""));
    check("getRequestPostData returns the POST body",
      captured === "a=1&b=2", "postData=" + JSON.stringify(captured));

    await page.setRequestInterception(false);
  } catch (e) {
    console.log("  [FAIL] postData test threw: " + (e && (e.message || e)));
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
