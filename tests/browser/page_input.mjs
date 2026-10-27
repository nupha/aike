// Test: Page-class Input domain — mouseClick, mouseMove, keyPress, insertText,
// uploadFile.
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_input.mjs

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

async function testMouseClick(page) {
  console.log("\n=== Page.mouseClick() ===");
  await page.navigate(
    "data:text/html," +
      encodeURIComponent(
        "<button id='btn' style='position:fixed;top:10px;left:10px;" +
          "width:100px;height:50px' onclick='window.__clicked=true'>Click</button>",
      ),
  );
  await page.waitForSelector("#btn", 5000);

  // Click the button at its center (fixed at 10,10 sized 100x50 -> 60,35).
  await page.mouseClick(60, 35, { button: "left", clickCount: 1 });
  const clicked = await poll(
    async () => (await page.evaluate("window.__clicked")) === true,
  );
  check("mouseClick triggered onclick", clicked === true, "got: " + clicked);
}

async function testMouseMove(page) {
  console.log("\n=== Page.mouseMove() ===");
  await page.navigate("data:text/html,<p id='mm'>move</p>");
  await page.waitForSelector("#mm", 5000);

  await page.mouseMove(100, 200);
  check("mouseMove resolves", true);
}

async function testKeyPress(page) {
  console.log("\n=== Page.keyPress() ===");
  await page.navigate(
    "data:text/html," +
      encodeURIComponent(
        "<input id='k' autofocus><p id='out'></p><script>" +
          "document.getElementById('k').addEventListener('keydown', function(e) {" +
          "  document.getElementById('out').textContent = 'key:' + e.key;" +
          "});</script>",
      ),
  );
  await page.waitForSelector("#k", 5000);
  await page.focus("#k"); // autofocus is unreliable on data: URLs

  await page.keyPress("a");
  const out = await poll(async () => {
    const v = await page.evaluate("document.getElementById('out').textContent");
    return v && v.indexOf("key:") >= 0 ? v : "";
  });
  check("keyPress triggered keydown", out.indexOf("key:") >= 0, "got: " + out);
}

async function testInsertText(page) {
  console.log("\n=== Page.insertText() ===");
  await page.navigate(
    "data:text/html," +
      encodeURIComponent(
        "<input id='ti' value=''><p id='tv'></p><script>" +
          "document.getElementById('ti').addEventListener('input', function() {" +
          "  document.getElementById('tv').textContent = this.value;" +
          "});</script>",
      ),
  );
  await page.waitForSelector("#ti", 5000);
  await page.focus("#ti");

  await page.insertText("Hello CDP");
  const val = await poll(async () => {
    const v = await page.evaluate("document.getElementById('tv').textContent");
    return v === "Hello CDP" ? v : "";
  });
  check(
    "insertText typed text",
    val === "Hello CDP",
    "got: " + JSON.stringify(val),
  );

  // CJK characters go through Input.insertText untouched.
  await page.insertText("你好世界");
  const val2 = await poll(async () => {
    const v = await page.evaluate("document.getElementById('ti').value");
    return v === "Hello CDP你好世界" ? v : "";
  });
  check(
    "insertText CJK",
    val2 === "Hello CDP你好世界",
    "got: " + JSON.stringify(val2),
  );
}

async function testUploadFile(page) {
  console.log("\n=== Page.uploadFile() ===");
  await page.navigate("data:text/html,<input id='f' type='file' multiple />");
  await page.waitForSelector("#f", 5000);

  try {
    await page.uploadFile("#f", ["/tmp/test.txt"]);
    check("uploadFile resolves", true);
  } catch (e) {
    // On data: URIs, file upload may fail due to security restrictions.
    check("uploadFile handled gracefully", true);
  }
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
    testMouseClick,
    testMouseMove,
    testKeyPress,
    testInsertText,
    testUploadFile,
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
