// Test: new aria-based APIs on Page (added alongside the MCP browser tools)
//
//   - page.ariaSnapshot() -> JSON string of aria nodes, each carrying a stable
//     `nodeId` (its ref, e.g. e12) and `backendNodeId` (survives DOM mutations).
//   - page.clickBackendNode(backendNodeId) -> click via CDP backend node id,
//     without re-querying the DOM (robust to DOM changes).
//   - page.typeBackendNode(backendNodeId, text) -> focus + insertText (CJK safe).
//   - ref-based flow: ref "eN" -> backendNodeId -> clickBackendNode / typeBackendNode,
//     mirroring the logic in mcp/browser/src/handlers.ts (parseRef + refToBackend).
//
// Run as a module (.mjs) so `import` is parsed in module mode:
//   cargo run -- tests/browser/page_aria.mjs
//
// `page.ariaSnapshot()` is backed by `Accessibility.getFullAXTree` and returns
// each node's `attributes` as a `{role, name, description}` object. The test
// asserts on that normalized shape, independent of any raw CDP wire format.

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

// Normalize Chrome's aria `attributes` (array of {name, value}) into a
// {name: value} object. Tolerates an object form too (future-proofing).
function attrsOf(node) {
  const raw = node ? node.attributes : undefined;
  const o = {};
  if (Array.isArray(raw)) {
    for (const kv of raw) {
      if (kv && kv.name !== undefined) o[kv.name] = kv.value;
    }
  } else if (raw && typeof raw === "object") {
    for (const k of Object.keys(raw)) o[k] = raw[k];
  }
  return o;
}

function findNode(nodes, role, name) {
  return nodes.find((n) => {
    const a = attrsOf(n);
    return a.role === role && (name === undefined || a.name === name);
  });
}

// Mirror of handlers.ts parseRef: accept "e12", "12", or a number.
function parseRef(ref) {
  if (ref === undefined || ref === null) return undefined;
  if (typeof ref === "number") return ref;
  const s = String(ref).trim();
  const m = s.match(/^e(\d+)$/) ?? s.match(/^(\d+)$/);
  return m ? Number(m[1]) : undefined;
}

const PAGE =
  "data:text/html," +
  encodeURIComponent(
    "<!DOCTYPE html><html><head><title>aria</title></head><body>" +
      '<h1 id="h" aria-label="heading-one">Aria Page</h1>' +
      '<button id="btn" aria-label="submit-btn">Click me</button>' +
      '<input id="name" aria-label="name-field" type="text" />' +
      '<a id="link" href="#x" aria-label="go-link">Go</a>' +
      "<script>" +
      "window.__c=0;" +
      "var b=document.getElementById('btn');" +
      "if(b)b.addEventListener('click',function(){window.__c++;});" +
      "</script>" +
      "</body></html>",
  );

async function testAriaSnapshot(page) {
  console.log("\n=== ariaSnapshot() ===");
  await page.navigate(PAGE);
  await page.waitForSelector("#btn", 5000);

  const json = await page.ariaSnapshot();
  check(
    "ariaSnapshot() returns a non-empty string",
    typeof json === "string" && json.length > 0,
    "len=" + (json ? json.length : 0),
  );

  let nodes;
  try {
    nodes = JSON.parse(json);
  } catch (e) {
    check("ariaSnapshot() parses to JSON", false, "parse error: " + e);
    return null;
  }
  check(
    "ariaSnapshot() is an array of aria nodes",
    Array.isArray(nodes),
    "type=" + typeof nodes,
  );
  check(
    "ariaSnapshot() has >= 4 nodes (heading/button/input/link)",
    Array.isArray(nodes) && nodes.length >= 4,
    "len=" + (nodes ? nodes.length : "n/a"),
  );

  // Every node must carry the ids we depend on for ref-based interaction.
  const idsOk =
    Array.isArray(nodes) &&
    nodes.every(
      (n) =>
        typeof n.nodeId === "number" && typeof n.backendNodeId === "number",
    );
  check(
    "every aria node has nodeId + backendNodeId",
    idsOk,
    JSON.stringify(nodes ? nodes.slice(0, 2) : null),
  );

  // refToBackend map: nodeId -> backendNodeId (mirrors handlers.ts).
  const refToBackend = new Map();
  for (const n of nodes) refToBackend.set(n.nodeId, n.backendNodeId);

  const btn = findNode(nodes, "button", "submit-btn");
  check(
    "aria tree contains button[role=button,name=submit-btn]",
    btn !== undefined,
    "btn=" + JSON.stringify(btn),
  );
  check(
    "button backendNodeId is a positive number",
    btn && typeof btn.backendNodeId === "number" && btn.backendNodeId > 0,
    "backend=" + (btn ? btn.backendNodeId : "n/a"),
  );

  const input = findNode(nodes, "textbox", "name-field");
  check(
    "aria tree contains input[role=textbox,name=name-field]",
    input !== undefined,
    "input=" + JSON.stringify(input),
  );

  const heading = findNode(nodes, "heading", "heading-one");
  check(
    "aria tree contains heading[role=heading,name=heading-one]",
    heading !== undefined,
    "heading=" + JSON.stringify(heading),
  );

  return { nodes, refToBackend, btn, input };
}

async function testClickBackendNode(page, ctx) {
  console.log("\n=== clickBackendNode(backendNodeId) ===");
  const backend = ctx.btn.backendNodeId;
  await page.clickBackendNode(backend);
  const clicks = await poll(async () => {
    const c = await page.evaluate("window.__c");
    return c === 1 ? c : 0;
  });
  check(
    "clickBackendNode fired the element click listener",
    clicks === 1,
    "clicks=" + clicks,
  );
}

async function testRefClick(page, ctx) {
  console.log("\n=== ref-based click (parseRef + refToBackend) ===");
  const ref = "e" + ctx.btn.nodeId;
  const parsed = parseRef(ref);
  check(
    "parseRef('eN') extracts the nodeId",
    parsed === ctx.btn.nodeId,
    "parsed=" + parsed,
  );
  const backend = ctx.refToBackend.get(parsed);
  check(
    "refToBackend maps ref -> backendNodeId",
    typeof backend === "number" && backend === ctx.btn.backendNodeId,
    "backend=" + backend,
  );
  await page.clickBackendNode(backend);
  const clicks = await poll(async () => {
    const c = await page.evaluate("window.__c");
    return c === 2 ? c : 0;
  });
  check(
    "ref-based click fired the listener again (count=2)",
    clicks === 2,
    "clicks=" + clicks,
  );
}

async function testTypeBackendNode(page, ctx) {
  console.log("\n=== typeBackendNode(backendNodeId, text) ===");
  await page.evaluate("document.getElementById('name').value=''");
  await page.typeBackendNode(ctx.input.backendNodeId, "aike");
  const val = await poll(async () => {
    const v = await page.evaluate("document.getElementById('name').value");
    return v === "aike" ? v : "";
  });
  check(
    "typeBackendNode set the input value",
    val === "aike",
    "val=" + JSON.stringify(val),
  );
}

async function testTypeCJK(page, ctx) {
  console.log("\n=== typeBackendNode CJK safety ===");
  await page.evaluate("document.getElementById('name').value=''");
  await page.typeBackendNode(ctx.input.backendNodeId, "你好世界");
  const val = await poll(async () => {
    const v = await page.evaluate("document.getElementById('name').value");
    return v === "你好世界" ? v : "";
  });
  check(
    "typeBackendNode inserted CJK text intact",
    val === "你好世界",
    "val=" + JSON.stringify(val),
  );
}

async function testRefType(page, ctx) {
  console.log("\n=== ref-based type (parseRef + refToBackend) ===");
  const ref = "e" + ctx.input.nodeId;
  const backend = ctx.refToBackend.get(parseRef(ref));
  check(
    "ref-based input resolves backendNodeId",
    backend === ctx.input.backendNodeId,
    "backend=" + backend,
  );
  await page.evaluate("document.getElementById('name').value=''");
  await page.typeBackendNode(backend, "ref-path");
  const val = await poll(async () => {
    const v = await page.evaluate("document.getElementById('name').value");
    return v === "ref-path" ? v : "";
  });
  check(
    "ref-based typeBackendNode set the input value",
    val === "ref-path",
    "val=" + JSON.stringify(val),
  );
}

async function testClickBackendNodeNegative(page) {
  console.log("\n=== clickBackendNode(negative) ===");
  // A bogus backend node id must reject (no node to box-model).
  let threw = false;
  try {
    await page.clickBackendNode(123456789);
  } catch (e) {
    threw = true;
  }
  check("clickBackendNode with unknown id rejects", threw === true);
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

  const testFns = [
    testAriaSnapshot,
    testClickBackendNode,
    testRefClick,
    testTypeBackendNode,
    testTypeCJK,
    testRefType,
    testClickBackendNodeNegative,
  ];

  // testAriaSnapshot returns a context we thread into the ref-based tests.
  let ctx = null;
  for (const fn of testFns) {
    try {
      if (fn === testAriaSnapshot) {
        ctx = await fn(page);
      } else {
        await fn(page, ctx);
      }
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
  if (fail > 0) Aike.exit(1);
  else Aike.exit(0);
}

main();
