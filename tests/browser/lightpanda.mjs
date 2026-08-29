// Test: lightpanda browser via Browser.spawn() custom command line
//
// Exercises the non-Chrome path of the browser module:
//   Browser.spawn({ bin, args, port }) — verbatim command line (lightpanda)
//   Browser.connect(cdpUrl)            — attach without owning the process
//   basic page flow on a non-Chrome CDP server
//   process.kill()                     — explicit teardown
//
// Requires a `lightpanda` binary on PATH (see lightpanda.io). When it is not
// installed the test reports SKIP and exits 0, so it can run in any checkout.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/lightpanda.mjs

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

const PORT = 9777; // dedicated port; must not collide with other tests

// Locate lightpanda via Aike.which; skip gracefully when not installed.
const bin = Aike.which("lightpanda");
if (!bin) {
  console.log("lightpanda not found on PATH -- SKIP");
  process.exit(0);
}
console.log("lightpanda binary: " + bin);

let proc = null;
try {
  // 1. Spawn the CDP server (custom command line, explicit port).
  console.log("\n=== spawn / connect ===");
  proc = await Browser.spawn({
    bin,
    args: ["serve", "--host", "127.0.0.1", "--port", String(PORT)],
    port: PORT,
  });
  check("spawn() resolves with resolved bin", proc.bin === bin, "got: " + proc.bin);
  check(
    "spawn() cdpUrl points at the requested port",
    proc.cdpUrl.startsWith(`ws://127.0.0.1:${PORT}/`),
    "got: " + proc.cdpUrl,
  );

  const browser = await Browser.connect(proc.cdpUrl);
  check("connect() returns a working Browser", !!browser);

  // 2. Browser-level info.
  // NOTE: lightpanda's CDP Browser.getVersion impersonates Chrome
  // (product "Chrome/124..."); only its HTTP /json/version reports
  // "Lightpanda/x". So assert a non-empty product, not the vendor name.
  console.log("\n=== version / page flow ===");
  const ver = await browser.getVersion();
  check(
    "getVersion() returns version info",
    typeof ver?.product === "string" && ver.product.length > 0 &&
      typeof ver?.userAgent === "string",
    "got: " + JSON.stringify(ver),
  );
  console.log("  product (lightpanda impersonates Chrome): " + ver.product);

  // 3. Page flow: newPage -> navigate -> evaluate.
  const page = await browser.newPage("about:blank");
  check("newPage() returns a Page with targetId", typeof page?.targetId === "string");

  await page.navigate("https://example.com");
  const title = await page.evaluate("document.title");
  check("navigate + evaluate works", title === "Example Domain", "got: " + JSON.stringify(title));

  const html = await page.getOuterHTML();
  check(
    "getOuterHTML() returns document markup",
    typeof html === "string" && /<html/i.test(html) && /example/i.test(html),
    "got: " + String(html).slice(0, 80),
  );

  // 4. close() must only detach — the process stays usable.
  console.log("\n=== close is detach-only ===");
  await browser.close();
  const browser2 = await Browser.connect(proc.cdpUrl);
  const ver2 = await browser2.getVersion();
  check(
    "reconnect after close() works (process not killed)",
    typeof ver2?.product === "string" && ver2.product.length > 0,
    "got: " + JSON.stringify(ver2),
  );
  await browser2.close();

  // 5. kill() really tears the process down.
  console.log("\n=== kill ===");
  await proc.kill();

  let killed = false;
  try {
    await Browser.connect(proc.cdpUrl);
  } catch {
    killed = true;
  }
  check("connect after kill() fails", killed);
} catch (e) {
  check("no unexpected error", false, String(e));
} finally {
  // Never leak the spawned server, even on failure paths.
  if (proc) {
    try {
      await proc.kill();
    } catch {}
  }
}

console.log("\n" + "=".repeat(50));
console.log(`pass: ${pass}, fail: ${fail}`);
if (fail > 0) {
  process.exit(1);
}
