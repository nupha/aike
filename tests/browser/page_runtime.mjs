// Test: Page-class Runtime domain P0/P1 APIs
//
// Covers (all flat on `Page`, exercised offline against a data: URL):
//   Runtime (P0): callFunctionOn, getProperties
//   Runtime (P1): evaluateHandle, onConsole
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_runtime.mjs

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
    "<!DOCTYPE html><html><head><title>runtime</title></head><body>" +
      "<div id='box'>hello</div>" +
      "</body></html>",
  );

async function testEvaluateHandle(page) {
  console.log("\n=== Runtime.evaluateHandle (P1) ===");
  await page.navigate(PAGE);
  await page.waitForSelector("#box", 5000);

  // evaluateHandle returns a handle carrying an objectId string.
  const h = await page.evaluateHandle("({ a: 1, b: 2, c: 3 })");
  check(
    "evaluateHandle returns handle with objectId",
    h && typeof h.objectId === "string" && h.objectId.length > 0,
    "handle=" + JSON.stringify(h),
  );

  // DOM nodes also produce an objectId.
  const body = await page.evaluateHandle("document.body");
  check(
    "evaluateHandle works for DOM nodes",
    body && typeof body.objectId === "string" && body.objectId.length > 0,
    "body=" + JSON.stringify(body),
  );
}

async function testGetProperties(page) {
  console.log("\n=== Runtime.getProperties (P0) ===");

  const h = await page.evaluateHandle("({ a: 1, b: 2, c: 3 })");
  const props = await page.getProperties(h.objectId);
  check(
    "getProperties enumerates own values",
    props && props.a === 1 && props.b === 2 && props.c === 3,
    "props=" + JSON.stringify(props),
  );

  // Nested / array values round-trip through the JSON bridge.
  const h2 = await page.evaluateHandle("({ name: 'x', list: [10, 20] })");
  const props2 = await page.getProperties(h2.objectId);
  check(
    "getProperties handles strings and arrays",
    props2 &&
      props2.name === "x" &&
      Array.isArray(props2.list) &&
      props2.list[0] === 10 &&
      props2.list[1] === 20,
    "props2=" + JSON.stringify(props2),
  );
}

async function testCallFunctionOn(page) {
  console.log("\n=== Runtime.callFunctionOn (P0) ===");

  const h = await page.evaluateHandle("({ a: 1, b: 2 })");

  // Function reads `this` (the remote object) and returns a computed value.
  const sum = await page.callFunctionOn(
    h.objectId,
    "function () { return this.a + this.b; }",
  );
  check("callFunctionOn reads `this`", sum === 3, "sum=" + sum);

  // With extra call arguments.
  const withArg = await page.callFunctionOn(
    h.objectId,
    "function (x) { return this.a + x; }",
    [10],
  );
  check("callFunctionOn passes args", withArg === 11, "withArg=" + withArg);

  // Method that mutates and returns a derived property.
  const keys = await page.callFunctionOn(
    h.objectId,
    "function () { return Object.keys(this); }",
  );
  check(
    "callFunctionOn can return arrays",
    Array.isArray(keys) && keys.length === 2 && keys[0] === "a",
    "keys=" + JSON.stringify(keys),
  );
}

async function testOnConsole(page) {
  console.log("\n=== Runtime.onConsole (P1) ===");

  let msg = null;
  page.onConsole((m) => {
    msg = m;
  });
  // onConsole enables Runtime internally; still enable explicitly for safety.
  await page.enableRuntime();

  await page.evaluate("console.log('aike-console-test', 42)");

  // consoleAPICalled is delivered async via the CDP event poller.
  const fired = await poll(async () => msg !== null);
  check("onConsole callback fired for console.log", fired === true);
  check(
    "onConsole entry has type 'log'",
    fired && msg && msg.type === "log",
    "msg=" + JSON.stringify(msg),
  );
  check(
    "onConsole entry carries the logged args",
    fired &&
      msg &&
      Array.isArray(msg.args) &&
      msg.args[0] &&
      msg.args[0].value === "aike-console-test" &&
      msg.args[1].value === 42,
    "msg=" + JSON.stringify(msg),
  );
}

async function testEvaluate(page) {
  console.log("\n=== page.evaluate() value round-trips ===");
  await page.navigate(
    "data:text/html," +
      encodeURIComponent(
        "<body><script>window._testVar = 42;" +
          "function greet(n) { return 'Hello, ' + n + '!'; }</script></body>",
      ),
  );
  await page.waitForSelector("body", 5000);

  check("number", (await page.evaluate("42")) === 42);
  check("string", (await page.evaluate("'hello'")) === "hello");
  check("bool true", (await page.evaluate("true")) === true);
  check("null", (await page.evaluate("null")) === null);
  check("array", JSON.stringify(await page.evaluate("[1,2,3]")) === "[1,2,3]");

  const obj = await page.evaluate("({a:1,b:'two'})");
  check("object a", obj.a === 1, "got: " + obj.a);
  check("object b", obj.b === "two", "got: " + obj.b);

  check("existing var", (await page.evaluate("window._testVar")) === 42);
  check(
    "function call",
    (await page.evaluate("greet('World')")) === "Hello, World!",
  );
}

async function testEnableRuntime(page) {
  console.log("\n=== page.enableRuntime() ===");
  // enableRuntime enables the Runtime domain; must resolve without error.
  await page.enableRuntime();
  check("enableRuntime resolves", true);
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
    testEvaluate,
    testEvaluateHandle,
    testGetProperties,
    testCallFunctionOn,
    testOnConsole,
    testEnableRuntime,
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
