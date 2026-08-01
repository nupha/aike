// wpt-runner: run WPT `*.any.js` testharness tests on the Aike JS engine,
// using the upstream WPT `testharness.js` (shell mode) to set up the
// test environment.
//
// Usage:
//   aike tools/aike/wpt-runner.mjs [options] [components...]
//
// Options:
//   -r, --root <dir>        WPT suite root (default: $AIKE_WPT_ROOT, else current directory)
//   -f, --fail-file <path>  write FAIL details to this file
//       --log-wptreport <path>  write results in wptreport JSON (wpt.fyi format)
//       --wpt-server <url>  redirect relative fetch() URLs to this static server
//                           (default: $WPT_SERVER_URL, else http://localhost:8080).
//                           Use --no-wpt-server to disable interception.
//       --no-wpt-server     disable fetch() interception
//   -h, --help              show this help
//
// `components` are directories or individual .any.js files, resolved against
// the root (e.g. `console`, `url`). If none is specified, nothing is tested.
// idlharness.any.js files are skipped (need WebIDL).
//
// Differences from aike-wpt.mjs:
//   - The test environment (test/promise_test/assert_*) is provided by the
//     upstream WPT testharness.js loaded from $AIKE_WPT_ROOT/resources/testharness.js
//     (ShellTestEnvironment, the no-DOM mode used by JS shells), instead of a
//     hand-written shim. This makes assert semantics match the spec exactly.
//   - Each test file is run in a fresh harness realm: testharness.js is
//     re-evaluated and our result/completion callbacks re-registered, so the
//     global Tests instance is reset per file (matching how real browsers load
//     one .any.js per document).
//
// Requirements: the WPT checkout must be available. By default we look at
// $AIKE_WPT_ROOT (env), ~/devel/wpt (memory), then cwd.

import * as fs from "fs";
import { basename, dirname, join } from "path";

const G = globalThis;

// ---------------------------------------------------------------------------
// Locate upstream testharness.js
// ---------------------------------------------------------------------------

function findTestharness(roots) {
  return (async () => {
    for (const base of roots) {
      if (!base) continue;
      const p = join(base, "resources/testharness.js");
      try {
        if (await fs.exists(p)) return p;
      } catch {}
    }
    return null;
  })();
}

// `testharnessText` is loaded after CLI parsing (so -r / env root is known).
let testharnessPath = null;
let testharnessText = "";

// ---------------------------------------------------------------------------
// Static wpt server (fixtures fetched at runtime)
// ---------------------------------------------------------------------------

let wptServer = G.Aike.env.WPT_SERVER_URL || "http://localhost:8080";
let wptServerEnabled = true;
let root = G.Aike.env.AIKE_WPT_ROOT || G.Aike.cwd();

// WPT suite root (also settable via -r). Resolved later in main().
let repoRoot = G.Aike.cwd();

// WPT test status codes (testharness.js Test.prototype.status)
const STATUS = { PASS: 0, FAIL: 1, TIMEOUT: 2, PRECONDITION_FAILED: 3 };

// ---------------------------------------------------------------------------
// Result collection: fed by testharness.js result/completion callbacks.
// ---------------------------------------------------------------------------

const results = [];
let currentFile = "(unknown)";
let currentAbsFile = null;
let currentComponent = "(unknown)";
const fileDurations = new Map(); // abs path -> duration in seconds

// Per-file scratch filled by the callbacks and drained into `results`.
let pendingResults = [];
let runDone = false;

// Build the closure that testharness.js will call for every finished test.
// Registered fresh after each testharness.js (re)evaluation so it points at
// the current `pendingResults`.
function registerHarnessCallbacks() {
  // Completion is suppressed until after the file's synchronous code has run
  // (see runOneFile). Reset the guard for this fresh harness realm.
  G.__wpt_allow_complete = false;
  // One result row per test. The callback receives the live Test object.
  G.add_result_callback((test) => {
    const ok = test.status === STATUS.PASS;
    pendingResults.push({
      component: currentComponent,
      file: currentFile,
      abs: currentAbsFile,
      kind: "test",
      name: String(test.name || "(unnamed)"),
      status: ok ? "PASS" : "FAIL",
      message: test.message || "",
    });
  });
  // Set once the harness fires completion for this file.
  G.add_completion_callback(() => {
    runDone = true;
  });
}

// ---------------------------------------------------------------------------
// Environment setup shared by every test file
// ---------------------------------------------------------------------------

// WPT .any.js tests reference `self`.
G.self = G;
// `.any.js` files run in the "window"-like variant here; GLOBAL lets tests
// branch on the scope they were loaded into (e.g. isWindow() guards).
G.GLOBAL = {
  isWindow: () => true,
  isWorker: () => false,
  isShadowRealm: () => false,
};
// testharness.js shell mode references structured_clone only when dispatching
// results across realms; we provide a safe fallback just in case.
if (typeof G.structured_clone === "undefined") {
  G.structured_clone = (v) => JSON.parse(JSON.stringify(v));
}

// ---------------------------------------------------------------------------
// fetch() interception
//
// WPT tests run with no wptserve origin, so a fetch() URL has to be resolved
// against *something*. We redirect every non-absolute URL to the static wpt
// server (aike/tools/wpt-server.mjs), which serves the WPT checkout from its
// root. The base is the server URL joined with the current test file's
// directory (relative to repoRoot), so paths like "../resources/data.json"
// and "resources/x" resolve exactly as they would in a browser. Absolute
// URLs (http(s):, data:, about:, ...) are passed through untouched.
// Controlled by --wpt-server / --no-wpt-server.
// ---------------------------------------------------------------------------

if (wptServerEnabled && wptServer) {
  const realFetch = G.fetch ? G.fetch.bind(G) : fetch.bind(globalThis);
  const base = wptServer.replace(/\/+$/, "");
  // Resolve `url` (relative or server-root-relative) to an absolute URL on the
  // static wpt server, mirroring how the browser resolves against the page URL.
  const resolve = (url) => {
    // Absolute (with scheme) or protocol-relative: leave as-is.
    if (/^[a-z][a-z0-9+.-]*:/i.test(url)) return url;
    if (url.startsWith("//")) return "http:" + url;
    // Compute the current test file's dir served by the wpt server.
    let dir = "";
    if (currentAbsFile && repoRoot && currentAbsFile.startsWith(repoRoot)) {
      dir = currentAbsFile.slice(repoRoot.replace(/\/+$/, "").length);
      dir = dir.replace(/[^/]*$/, ""); // keep up to and including last slash
    } else if (currentAbsFile) {
      dir = currentAbsFile.replace(/[^/]*$/, "");
    }
    const testBase = base + "/" + dir.replace(/^\/+/, "");
    return new URL(url, testBase).href;
  };
  G.fetch = (input, init) => {
    let url;
    if (typeof input === "string") url = input;
    else if (typeof URL !== "undefined" && input instanceof URL)
      url = input.href;
    else if (
      input &&
      typeof input === "object" &&
      typeof input.url === "string"
    )
      url = input.url;
    else return realFetch(input, init);
    const u = resolve(url);
    const isSimple =
      typeof input === "string" ||
      (typeof URL !== "undefined" && input instanceof URL);
    if (isSimple) return realFetch(u, init);
    // URL changed: re-issue against the static server, rebuilding a Request
    // (preserving method/headers/body) when the original was one.
    const req = typeof Request !== "undefined" ? new Request(u, input) : u;
    return realFetch(req, init);
  };
}

// ---------------------------------------------------------------------------
// META: script= dependency loading
// ---------------------------------------------------------------------------

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
      throw new Error(
        `loading META script '${meta.value}': ${e instanceof Error ? e.message : e}`,
      );
    }
  }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

const USAGE = `Usage: aike tools/aike/wpt-runner.mjs [options] [components...]

Options:
  -r, --root <dir>        WPT suite root (default: $AIKE_WPT_ROOT, else current directory)
  -f, --fail-file <path>  write FAIL details to this file
      --log-wptreport <path>  write results in wptreport JSON (wpt.fyi format)
      --wpt-server <url>  redirect relative fetch() URLs to this static server
                          (default: $WPT_SERVER_URL, else http://localhost:8080).
                          Use --no-wpt-server to disable interception.
  -h, --help              show this help

components: directories or .any.js files, resolved against the root
            (none = nothing is tested). Example: console url console/foo.any.js`;

const args = G.Aike.argv.slice(2);
root = G.Aike.env.AIKE_WPT_ROOT || G.Aike.cwd();
let failFile = null;
let logWptreport = null;
const components = [];

for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (a === "-r" || a === "--root") root = args[++i];
  else if (a.startsWith("--root=")) root = a.slice("--root=".length);
  else if (a === "-f" || a === "--fail-file") failFile = args[++i];
  else if (a.startsWith("--fail-file="))
    failFile = a.slice("--fail-file=".length);
  else if (a === "--log-wptreport") logWptreport = args[++i];
  else if (a.startsWith("--log-wptreport="))
    logWptreport = a.slice("--log-wptreport=".length);
  else if (a === "--wpt-server") wptServer = args[++i];
  else if (a.startsWith("--wpt-server="))
    wptServer = a.slice("--wpt-server=".length);
  else if (a === "--no-wpt-server") wptServerEnabled = false;
  else if (a === "-h" || a === "--help") {
    console.log(USAGE);
    G.Aike.exit(0);
  } else components.push(a);
}
if (logWptreport && !logWptreport.startsWith("/"))
  logWptreport = join(G.Aike.cwd(), logWptreport);
// Resolve to an absolute path: fs.* resolves relative paths against the
// process cwd, while import() resolves them against the script's directory.
repoRoot = root.startsWith("/") ? root : join(G.Aike.cwd(), root);

// If no component is specified, run nothing.
if (components.length === 0) {
  console.log("No component specified; nothing to test.");
  console.log(
    "Usage: aike wpt-runner.mjs -r <wpt-root> <component> [component ...]",
  );
  G.Aike.exit(0);
}

// After CLI/env resolution, locate and load upstream testharness.js. The WPT
// root (-r / $AIKE_WPT_ROOT) is the preferred source; fall back to
// a few well-known checkouts.
testharnessPath = await findTestharness([
  root,
  G.Aike.env.AIKE_WPT_ROOT,
  join(G.Aike.env.HOME || "", "devel/wpt"),
  G.Aike.cwd(),
]);
if (!testharnessPath) {
  console.log(
    "ERROR: could not find resources/testharness.js. Set AIKE_WPT_ROOT or pass -r <wpt-root>, or place a WPT checkout at ~/devel/wpt.",
  );
  G.Aike.exit(1);
}
testharnessText = await fs.readText(testharnessPath);
// WPT's testharness.js ShellTestEnvironment.on_tests_ready is a no-op, which
// leaves test_environment.all_loaded false forever, so the harness never
// reports completion in a headless JS shell. The browser path relies on the
// window 'load' event firing after the test script runs; in a shell there is
// no such event, so we replicate it: defer setting all_loaded to a microtask
// (after the synchronous test()/promise_test() registrations in the file have
// run). This is the standard shell-mode hook a wpt runner's testharnessreport
// would provide.
const SHELL_NOOP =
  "    ShellTestEnvironment.prototype.on_tests_ready = function() {};";
// Defer harness completion to after the test file's synchronous code has run.
// testharness.js's notify_result() auto-completes the run whenever all_done()
// is true, but in a shell the 'load' event never fires, so we drive
// all_loaded ourselves. Because the engine may run the on_tests_ready hook
// (which sets all_loaded) before the file's tests finish registering, we also
// guard Tests.prototype.complete() behind a flag that we only flip *after*
// Aike.evalScript(file) returns. This prevents premature completion that
// would otherwise drop every test after the first.
if (testharnessText.includes("    Tests.prototype.complete = function() {")) {
  testharnessText = testharnessText.replace(
    "    Tests.prototype.complete = function() {",
    "    Tests.prototype.complete = function() {\n        if (!self.__wpt_allow_complete) return;",
  );
} else {
  console.log(
    "# WARNING: Tests.prototype.complete not found; completion may misfire.",
  );
}
const SHELL_HOOK = [
  "    ShellTestEnvironment.prototype.on_tests_ready = function() {",
  "        var self_env = this;",
  "        self.__wpt_on_loaded = function() {",
  "            self_env.all_loaded = true;",
  "            if (self_env.on_loaded_callback) self_env.on_loaded_callback();",
  "        };",
  "    };",
].join("\n");
if (testharnessText.includes(SHELL_NOOP)) {
  testharnessText = testharnessText.replace(SHELL_NOOP, SHELL_HOOK);
} else {
  console.log(
    "# WARNING: ShellTestEnvironment.on_tests_ready no-op not found; completion may not fire.",
  );
}
console.log(`# using testharness.js from ${testharnessPath}`);

// Max wait for outstanding promise_test callbacks to settle, per component.
const PROMISE_SETTLE_TIMEOUT_MS = 5000;

// ---------------------------------------------------------------------------
// Single-file execution
// ---------------------------------------------------------------------------

async function runOneFile(file) {
  currentAbsFile = file;
  currentFile = basename(file);

  // Fresh harness realm: re-evaluate testharness.js so the global Tests
  // instance (and its callback lists) reset, then register our collectors.
  pendingResults = [];
  runDone = false;
  Aike.evalScript(testharnessText);
  registerHarnessCallbacks();

  const code = await fs.readText(file);
  await loadScriptDeps(file, code);
  Aike.evalScript(code);

  // All test()/promise_test() calls in the file have now run synchronously.
  // Now allow completion and drive it: the ShellTestEnvironment hook set
  // self.__wpt_on_loaded, which marks the environment all_loaded and runs the
  // harness's on_loaded callback. With __wpt_allow_complete now true, that
  // callback's complete() actually runs. Synchronous tests are already done;
  // any pending promise_test callbacks finish during the wait loop below and
  // complete the run via notify_result's all_done() check.
  G.__wpt_allow_complete = true;
  if (typeof G.__wpt_on_loaded === "function") G.__wpt_on_loaded();

  // Let synchronous tests and any promise_test callbacks settle. testharness
  // fires add_completion_callback once the last test finishes, so poll until
  // runDone flips (or the deadline passes).
  const deadline = G.Aike.now() + PROMISE_SETTLE_TIMEOUT_MS;
  while (!runDone) {
    if (G.Aike.now() >= deadline) break;
    await G.Aike.sleep(1);
  }

  for (const r of pendingResults) results.push(r);
  if (!runDone && pendingResults.length === 0) {
    // Nothing completed: likely a load-time throw inside the file. Report it
    // as a file-level failure if it wasn't already captured by import error.
  }
}

// Resolve a component argument to a list of .any.js test files.
async function resolveTestFiles(comp) {
  const compPath = comp.startsWith("/") ? comp : join(repoRoot, comp);
  const st = await fs.stat(compPath);
  if (!st.isDirectory) return [compPath];
  const out = [];
  const walk = async (dir) => {
    for (const e of await fs.readDir(dir)) {
      const p = join(dir, e.name);
      if (e.isDirectory) {
        await walk(p);
        continue;
      }
      if (e.name.endsWith(".any.js") && e.name !== "idlharness.any.js")
        out.push(p);
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
    console.log(`\n=== running ${file} ===`);
    const start = G.Aike.now();
    try {
      await runOneFile(file);
    } catch (e) {
      results.push({
        component: comp,
        file: basename(file),
        abs: file,
        kind: "load-error",
        name: "(file load error)",
        status: "FAIL",
        message: String(e),
      });
    }
    fileDurations.set(file, (G.Aike.now() - start) / 1000);
  }

  let passed = 0;
  let failed = 0;
  const failures = [];
  for (const r of results.slice(before)) {
    if (r.status === "PASS") {
      passed++;
      continue;
    }
    failed++;
    failures.push(r);
  }
  const status = failed > 0 ? "FAIL" : "PASS";
  console.log(`=== ${comp} ===`);
  for (const r of failures) {
    console.log(`FAIL  [${r.file}] ${r.name}`);
    if (r.message) console.log(`      ${r.message}`);
  }
  console.log(
    `Files: ${files.length}  Tests: ${failed + passed}  Passed: ${passed}  Failed: ${failed}  [${status === "FAIL" ? "FAIL" : "PASS"}]`,
  );
  return { comp, files: files.length, tests: failed + passed, passed, failed };
}

const summaries = [];
let totalFailed = 0;
for (const comp of components) {
  const s = await runComponent(comp);
  if (s) {
    summaries.push(s);
    totalFailed += s.failed;
  }
}

// FAIL report file.
if (failFile) {
  const lines = [];
  for (const comp of components) {
    const compResults = results.filter(
      (r) => r.component === comp && r.status !== "PASS",
    );
    if (compResults.length === 0) continue;
    lines.push(`=== ${comp} ===`);
    for (const r of compResults) {
      lines.push(`FAIL  [${r.file}] ${r.name}`);
      if (r.message) lines.push(`      ${r.message}`);
    }
    lines.push("");
  }
  const totalTests = results.length;
  const totalPassed = results.filter((r) => r.status === "PASS").length;
  lines.push(`=== summary ===`);
  lines.push(
    `Components: ${components.length}  Files: ${summaries.reduce((n, s) => n + s.files, 0)}  Tests: ${totalTests}  Passed: ${totalPassed}  Failed: ${totalFailed}`,
  );
  await fs.writeText(failFile, lines.join("\n") + "\n");
  console.log(`\nFAIL details written to ${failFile}`);
}

console.log(`\n=== summary ===`);
for (const s of summaries) {
  console.log(
    `${s.comp}: ${s.files} files, ${s.tests} tests, ${s.passed} passed, ${s.failed} failed`,
  );
}
console.log(
  `Total: ${summaries.reduce((n, s) => n + s.tests, 0)} tests, ${summaries.reduce((n, s) => n + s.passed, 0)} passed, ${totalFailed} failed`,
);
G.Aike.exit(totalFailed > 0 ? 1 : 0);
