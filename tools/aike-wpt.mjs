// aike-wpt: run WPT `*.any.js` testharness tests on the Aike JS engine.
//
// Usage:
//   aike tools/aike/aike-wpt.mjs [options] [components...]
//
// Options:
//   -r, --root <dir>        WPT suite root (default: $AIKE_WPT_ROOT, else current directory)
//   -f, --fail-file <path>  write FAIL details to this file
//       --log-wptreport <path>  write results in wptreport JSON (wpt.fyi format)
//   -h, --help              show this help
//
// `components` are directories or individual .any.js files, resolved against
// the root (e.g. `console`, `url`, `console/console-is-a-namespace.any.js`).
// Default: `console`. idlharness.any.js files are skipped (need WebIDL).
//
// Examples:
//   aike tools/aike/aike-wpt.mjs
//   aike tools/aike/aike-wpt.mjs -r /path/to/wpt console url
//   aike tools/aike/aike-wpt.mjs -f fails.txt console encoding
//   aike tools/aike/aike-wpt.mjs --log-wptreport report.json console url
//
// Test features supported:
//   - `// META: script=<path>` deps are loaded into the shared global scope
//     (as real browsers do with <script src>), resolved root-relative
//     (`/common/...`) or relative to the test file.
//   - `self` is provided as an alias of globalThis (window-variant semantics).
//   - engine API coverage is NOT shimmed: tests only load if the engine
//     provides the objects they use (URL, TextEncoder, ...).

import * as fs from "fs";
import { join, basename, dirname } from "path";

const G = globalThis;

// ---------------------------------------------------------------------------
// Minimal testharness-compatible shim (only what the console tests use)
// ---------------------------------------------------------------------------

const results = [];
let currentFile = "(unknown)";
let currentAbsFile = null;
let currentComponent = "(unknown)";
const fileDurations = new Map(); // abs path -> duration in seconds

function fmt(v) {
  if (v === undefined) return "undefined";
  if (v === null) return "null";
  const t = typeof v;
  if (t === "string") return JSON.stringify(v);
  if (t === "symbol") return v.toString();
  if (t === "function") return "<function>";
  if (t === "object") {
    try { return JSON.stringify(v); } catch { return String(v); }
  }
  return String(v);
}

class AssertionError extends Error {
  constructor(message) {
    super(message);
    this.name = "AssertionError";
  }
}

function assert_(cond, name, description, detail) {
  if (!cond) {
    throw new AssertionError(`${name}${description ? ": " + description : ""}: ${detail}`);
  }
}

function makeTestRecord(name) {
  const record = { component: currentComponent, file: currentFile, abs: currentAbsFile, kind: "test", name: String(name), status: "PASS", message: "" };
  results.push(record);
  const cleanups = [];
  const t = {
    add_cleanup(fn) { cleanups.push(fn); },
    _run_cleanups() {
      for (const fn of cleanups) {
        try { fn(); } catch (e) {
          record.status = "FAIL";
          const msg = e instanceof Error ? e.message : String(e);
          record.message += (record.message ? " | " : "") + `cleanup error: ${msg}`;
        }
      }
    },
  };
  return { record, t };
}

function test(func, name) {
  const { record, t } = makeTestRecord(name);
  try {
    func.call(t, t);
  } catch (e) {
    record.status = "FAIL";
    record.message = e instanceof Error ? `${e.name}: ${e.message}` : String(e);
  }
  t._run_cleanups();
}

function promise_test(func, name) {
  const { record, t } = makeTestRecord(name);
  record.status = "RUNNING";
  let p;
  try { p = Promise.resolve(func.call(t, t)); }
  catch (e) { p = Promise.reject(e); }
  p.then(
    () => { record.status = "PASS"; t._run_cleanups(); },
    (e) => {
      record.status = "FAIL";
      record.message = e instanceof Error ? `${e.name}: ${e.message}` : String(e);
      t._run_cleanups();
    }
  );
}

function assert_true(actual, description) { assert_(!!actual, "assert_true", description, `expected true got ${fmt(actual)}`); }
function assert_false(actual, description) { assert_(!actual, "assert_false", description, `expected false got ${fmt(actual)}`); }
function assert_equals(actual, expected, description) { assert_(Object.is(actual, expected), "assert_equals", description, `expected ${fmt(expected)} but got ${fmt(actual)}`); }
function assert_not_equals(actual, expected, description) { assert_(!Object.is(actual, expected), "assert_not_equals", description, `disallowed value ${fmt(actual)}`); }
function assert_array_equals(actual, expected, description) {
  assert_(Array.isArray(actual), "assert_array_equals", description, `expected array but got ${fmt(actual)}`);
  assert_(actual.length === expected.length, "assert_array_equals", description, `expected length ${expected.length} but got ${actual.length}`);
  for (let i = 0; i < expected.length; i++) {
    assert_(Object.is(actual[i], expected[i]), "assert_array_equals", description, `at index ${i}: expected ${fmt(expected[i])} but got ${fmt(actual[i])}`);
  }
}
function assert_own_property(object, property_name, description) { assert_(object.hasOwnProperty(property_name), "assert_own_property", description, `expected property ${String(property_name)} missing`); }
function assert_throws_js(constructor, func, description) {
  let threw = null;
  try { func(); } catch (e) { threw = e; }
  assert_(threw !== null, "assert_throws_js", description, `expected ${constructor.name} to throw but nothing was thrown`);
  assert_(threw instanceof constructor, "assert_throws_js", description, `expected ${constructor.name} to throw but got ${fmt(threw)}`);
}

// WPT .any.js tests reference `self`.
G.self = G;
G.test = test;
G.promise_test = promise_test;
G.assert_true = assert_true;
G.assert_false = assert_false;
G.assert_equals = assert_equals;
G.assert_not_equals = assert_not_equals;
G.assert_array_equals = assert_array_equals;
G.assert_own_property = assert_own_property;
G.assert_throws_js = assert_throws_js;

// ---------------------------------------------------------------------------
// META: script= dependency loading
// ---------------------------------------------------------------------------

let repoRoot = G.Aike.cwd();
const loadedHelpers = new Set();

// WPT parses metadata lines at the very top of the file, stopping at the
// first line that isn't a META comment.
function parseMeta(code) {
  const metas = [];
  const lines = code.split("\n");
  const re = /^\s*\/\/\s*META:\s*(\S+)=(.*)$/;
  for (const line of lines) {
    const m = re.exec(line);
    if (!m) break;
    metas.push({ key: m[1], value: m[2] });
  }
  return metas;
}

function resolveMetaPath(metaPath, testFile) {
  if (metaPath.startsWith("/")) return join(repoRoot, metaPath.slice(1));
  return join(dirname(testFile), metaPath);
}

// Load `// META: script=` deps into the shared global scope, deduped by
// absolute path. Mirrors how the generated .html shell loads scripts.
async function loadScriptDeps(testFile, code) {
  for (const meta of parseMeta(code)) {
    if (meta.key !== "script") continue;
    const abs = resolveMetaPath(meta.value, testFile);
    if (loadedHelpers.has(abs)) continue;
    loadedHelpers.add(abs);
    try {
      Aike.evalScript(await fs.readText(abs));
    } catch (e) {
      throw new Error(`loading META script '${meta.value}': ${e instanceof Error ? e.message : e}`);
    }
  }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const USAGE = `Usage: aike tools/aike/aike-wpt.mjs [options] [components...]

Options:
  -r, --root <dir>        WPT suite root (default: $AIKE_WPT_ROOT, else current directory)
  -f, --fail-file <path>  write FAIL details to this file
      --log-wptreport <path>  write results in wptreport JSON (wpt.fyi format)
  -h, --help              show this help

components: directories or .any.js files, resolved against the root
            (default: console). Example: console url console/foo.any.js`;

const args = G.Aike.argv.slice(2);
let root = G.Aike.env.AIKE_WPT_ROOT || G.Aike.cwd();
let failFile = null;
let logWptreport = null;
const components = [];

for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (a === "-r" || a === "--root") root = args[++i];
  else if (a.startsWith("--root=")) root = a.slice("--root=".length);
  else if (a === "-f" || a === "--fail-file") failFile = args[++i];
  else if (a.startsWith("--fail-file=")) failFile = a.slice("--fail-file=".length);
  else if (a === "--log-wptreport") logWptreport = args[++i];
  else if (a.startsWith("--log-wptreport=")) logWptreport = a.slice("--log-wptreport=".length);
  else if (a === "-h" || a === "--help") { console.log(USAGE); G.Aike.exit(0); }
  else components.push(a);
}
if (logWptreport && !logWptreport.startsWith("/")) logWptreport = join(G.Aike.cwd(), logWptreport);
const timeStart = Date.now() / 1000;
if (components.length === 0) components.push("console");
// Normalize to an absolute path: fs.* resolves relative paths against the
// process cwd, while import() resolves them against the script's directory.
repoRoot = root.startsWith("/") ? root : join(G.Aike.cwd(), root);

// Resolve a component argument to a list of .any.js test files.
// Directories are walked recursively so component args like `fetch` collect
// tests nested in subdirectories (e.g. fetch/api/basic/*.any.js).
async function resolveTestFiles(comp) {
  const compPath = comp.startsWith("/") ? comp : join(repoRoot, comp);
  const st = await fs.stat(compPath);
  if (!st.isDirectory) return [compPath];
  const out = [];
  const walk = async (dir) => {
    for (const e of await fs.readDir(dir)) {
      const p = join(dir, e.name);
      if (e.isDirectory) { await walk(p); continue; }
      if (e.name.endsWith(".any.js") && e.name !== "idlharness.any.js") out.push(p);
    }
  };
  await walk(compPath);
  return out.sort();
}

async function runComponent(comp) {
  let files;
  try {
    files = await resolveTestFiles(comp);
  } catch (e) {
    console.log(`=== ${comp} ===`);
    console.log(`  ERROR: cannot resolve '${comp}': ${e.message}`);
    return null;
  }
  if (files.length === 0) {
    console.log(`=== ${comp} ===`);
    console.log(`  No .any.js test files found`);
    return null;
  }

  currentComponent = comp;
  const before = results.length;
  for (const file of files) {
    currentFile = basename(file);
    currentAbsFile = file;
    console.log(`\n=== running ${file} ===`);
    const start = G.Aike.now();
    try {
      const code = await fs.readText(file);
      await loadScriptDeps(file, code);
      await import(file);
    } catch (e) {
      results.push({ component: comp, file: currentFile, abs: file, kind: "load-error", name: "(file load error)", status: "FAIL", message: String(e) });
    }
    fileDurations.set(file, (G.Aike.now() - start) / 1000);
  }
  // Let any promise_test callbacks settle.
  await G.Aike.sleep(0);

  let passed = 0;
  let failed = 0;
  const failures = [];
  for (const r of results.slice(before)) {
    if (r.status === "PASS") { passed++; continue; }
    failed++;
    failures.push(r);
  }
  const status = failed > 0 ? "FAIL" : "PASS";
  console.log(`=== ${comp} ===`);
  for (const r of failures) {
    console.log(`❌ FAIL  [${r.file}] ${r.name}`);
    if (r.message) console.log(`      ${r.message}`);
  }
  console.log(`Files: ${files.length}  Tests: ${failed + passed}  Passed: ${passed}  Failed: ${failed}  [${status === "FAIL" ? "❌ FAIL" : "PASS"}]`);
  return { comp, files: files.length, tests: failed + passed, passed, failed };
}

const summaries = [];
let totalFailed = 0;
for (const comp of components) {
  const s = await runComponent(comp);
  if (s) { summaries.push(s); totalFailed += s.failed; }
}

// FAIL report file.
if (failFile) {
  const lines = [];
  for (const comp of components) {
    const compResults = results.filter(r => r.component === comp && r.status !== "PASS");
    if (compResults.length === 0) continue;
    lines.push(`=== ${comp} ===`);
    for (const r of compResults) {
      lines.push(`FAIL  [${r.file}] ${r.name}`);
      if (r.message) lines.push(`      ${r.message}`);
    }
    lines.push("");
  }
  const totalTests = results.length;
  const totalPassed = results.filter(r => r.status === "PASS").length;
  lines.push(`=== summary ===`);
  lines.push(`Components: ${components.length}  Files: ${summaries.reduce((n, s) => n + s.files, 0)}  Tests: ${totalTests}  Passed: ${totalPassed}  Failed: ${totalFailed}`);
  await fs.writeText(failFile, lines.join("\n") + "\n");
  console.log(`\nFAIL details written to ${failFile}`);
}

console.log(`\n=== summary ===`);
for (const s of summaries) {
  console.log(`${s.comp}: ${s.files} files, ${s.tests} tests, ${s.passed} passed, ${s.failed} failed`);
}
console.log(`Total: ${summaries.reduce((n, s) => n + s.tests, 0)} tests, ${summaries.reduce((n, s) => n + s.failed, 0)} failed`);

// wptreport JSON (format consumed by wpt.fyi), one entry per test file.
if (logWptreport) {
  // Map a test status to the WPT subtest status vocabulary.
  function wptStatus(status) {
    if (status === "PASS") return "PASS";
    if (status === "RUNNING") return "TIMEOUT"; // promise_test never settled
    return "FAIL";
  }
  function wptTestPath(absFile) {
    const rel = absFile && absFile.startsWith(repoRoot + "/") ? absFile.slice(repoRoot.length) : absFile;
    return rel && rel.startsWith("/") ? rel : "/" + rel;
  }

  const byFile = new Map();
  for (const r of results) {
    const key = r.abs || r.file;
    if (!byFile.has(key)) byFile.set(key, []);
    byFile.get(key).push(r);
  }

  const reportResults = [];
  for (const [key, recs] of byFile) {
    const loadErr = recs.find(r => r.kind === "load-error");
    const entry = {
      test: wptTestPath(key),
      subsuite: "",
      status: loadErr ? "ERROR" : "OK",
      message: loadErr ? loadErr.message : null,
      duration: fileDurations.get(key) || 0,
      expected: "OK",
      subtests: [],
    };
    if (loadErr) {
      entry.subtests = [];
    } else {
      entry.subtests = recs.map(r => ({
        name: r.name,
        status: wptStatus(r.status),
        expected: "PASS",
        message: r.message || null,
      }));
    }
    reportResults.push(entry);
  }
  reportResults.sort((a, b) => (a.test < b.test ? -1 : a.test > b.test ? 1 : 0));

  const report = {
    run_info: { product: "aike", platform: G.Aike.platform },
    time_start: timeStart,
    time_end: Date.now() / 1000,
    results: reportResults,
    subsuites: {},
  };
  await fs.writeText(logWptreport, JSON.stringify(report, null, 2) + "\n");
  console.log(`wptreport written to ${logWptreport}`);
}

G.Aike.exit(totalFailed > 0 ? 1 : 0);
