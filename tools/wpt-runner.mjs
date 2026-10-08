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
//       --wpt-server <url>  base URL injected as the evalScript baseUrl, so
//                           relative fetch()/`fs` specifiers resolve against it
//                           (default: $WPT_SERVER_URL, else http://web-platform.test:8000).
//                           Use --no-wpt-server to disable injection.
//       --no-wpt-server     disable base URL injection (relative URLs fall back to cwd)
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

let wptServer = G.process.env.WPT_SERVER_URL || "http://web-platform.test:8000";
let wptServerEnabled = true;

let root = G.process.env.AIKE_WPT_ROOT || G.process.cwd();

// WPT suite root (also settable via -r). Resolved later in main().
let repoRoot = G.process.cwd();

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

  // Bind the owning file now, at registration time. A test that settles after
  // the file's settle deadline would otherwise be filed under whichever file
  // happens to be current when its callback finally runs.
  const owner = {
    component: currentComponent,
    file: currentFile,
    abs: currentAbsFile,
  };

  // One result row per test. The callback receives the live Test object.
  G.add_result_callback((test) => {
    const ok = test.status === STATUS.PASS;
    pendingResults.push({
      ...owner,
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
// WPT tests also reference `window` (the global object in window scope).
G.window = G;

// WPT is a browser-oriented suite: put `fetch` in WHATWG mode for this run —
// enforce CORS and let fetch auto-manage the cookie jar (`fetch.cors` /
// `fetch.cookie`, only present in a `fetch_browser` build) — and allow the
// self-signed WPT TLS server (`fetch.allowInsecureTLS`). Harmless when the
// machinery is not compiled in.
try {
  fetch.cors = true;
  fetch.cookie = true;
  fetch.allowInsecureTLS = true;
} catch {
  // lean build without the fetch switches: nothing to configure
}
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
// Base URL injection (replaces per-fetch interception)
//
// WPT tests run against a remote wpt server. Instead of monkey-patching
// `fetch`, we pass each test file's wpt-server URL as the `baseUrl` option of
// `Aike.evalScript`. The engine then uses that baseUrl as the script's base
// URL (the value behind `import.meta.url`), so relative fetch()/fs specifiers
// resolve against it automatically — mirroring how the browser resolves against
// the page URL. Absolute URLs (http(s):, data:, about:, ...) are unaffected.
//
// Controlled by --wpt-server / --no-wpt-server: when disabled, no filename is
// injected and relative URLs resolve against the engine cwd (legacy behavior).
// ---------------------------------------------------------------------------

let wptBase =
  wptServerEnabled && wptServer ? wptServer.replace(/\/+$/, "") : null;

// Map a local checkout path to the wpt-server URL the browser would load it
// from. e.g. /checkout/console/a.any.js -> http://web-platform.test:8000/console/a.any.js
// Used as the evalScript filename so relative fetch()/`fs` resolve correctly.
//
// Note: `.h2.` tests really belong on the h2 origin (https, port 9000), and
// their `.h2.py` fixtures 500 on the plain server. Routing them there is left
// undone on purpose: those fixtures then run every shared test over HTTP/2,
// which is markedly less reliable here and costs more tests than it gains.
function fileHttpUrl(absFile) {
  if (!wptBase) return null;

  let rel = absFile;
  if (repoRoot && absFile.startsWith(repoRoot)) {
    rel = absFile.slice(repoRoot.replace(/\/+$/, "").length);
  }
  rel = rel.replace(/^\/+/, "");

  return wptBase + "/" + rel;
}

// evalScript options carrying the wpt-server base URL as `baseUrl`, which the
// engine uses both as the reported script filename and as the base for relative
// `fs`/`fetch` resolution. Returns undefined when interception is disabled
// (relative URLs then fall back to the engine cwd, matching legacy behavior).
function evalOpts(absFile) {
  const url = fileHttpUrl(absFile);
  return url ? { baseUrl: url } : undefined;
}

// ---------------------------------------------------------------------------
// Script source fetching
//
// WPT `.sub.` files (and other server-processed variants) have `{{...}}`
// placeholders expanded by wptserve at request time. We therefore fetch the
// script body from the wpt server (the same URL the browser would load) instead
// of reading it off disk — this makes wptserve the single source of truth for
// template substitution, so we no longer need a hand-rolled expander.
//
// When the server is disabled (--no-wpt-server), there is nothing to fetch
// from, so we fall back to reading the local file and expanding `.sub.`
// placeholders ourselves, preserving legacy behavior.
// ---------------------------------------------------------------------------

// Host/port of the wpt server, derived from the configured base URL so that
// --wpt-server / $WPT_SERVER_URL stay authoritative.
function serverHostPort() {
  try {
    const u = new URL(wptServer);
    return {
      host: u.hostname,
      port: u.port || (u.protocol === "https:" ? "443" : "80"),
    };
  } catch {
    return { host: "web-platform.test", port: "8000" };
  }
}

// Local fallback: expand `{{host}}` / `{{ports[scheme][n]}}` placeholders.
// Only used when the wpt server is disabled and we read files off disk.
function substituteTemplates(code) {
  const { host, port } = serverHostPort();
  return code
    .replace(/\{\{host\}\}/g, host)
    .replace(/\{\{ports\[(\w+)\]\[(\d+)\]\}\}/g, (m, scheme, idx) =>
      scheme === "http" && idx === "0" ? port : m,
    );
}

// Fetch a test script's source. With a wpt server, request the script URL from
// it (so `.sub.` substitution happens server-side); without one, read the local
// file and expand `.sub.` placeholders locally.
async function fetchText(absFile) {
  if (wptBase) {
    const url = fileHttpUrl(absFile);
    let res;
    try {
      res = await fetch(url);
    } catch (e) {
      console.error(`# fetchText failed for ${url}: ${e}`);
      throw e;
    }
    if (!res.ok) {
      throw new Error(`fetch ${url} failed: ${res.status} ${res.statusText}`);
    }
    return await res.text();
  }
  let code = await fs.readText(absFile);
  if (absFile.includes(".sub.")) code = substituteTemplates(code);
  return code;
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
      Aike.evalScript("globalThis.self = globalThis; globalThis.window = globalThis;");
      Aike.evalScript(await fetchText(abs), evalOpts(abs));
    } catch (e) {
      console.log("DEBUG loadScriptDeps failed for", meta.value, "abs=", abs);
      console.log("DEBUG err:", e && e.stack ? e.stack : e);
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
      --wpt-server <url>  base URL injected as the evalScript filename, so
                          relative fetch()/fs specifiers resolve against it
                          (default: $WPT_SERVER_URL, else http://web-platform.test:8000).
                          Use --no-wpt-server to disable injection.
      --global <scopes>   only run .any.js whose META global= set intersects
                          <scopes> (comma list: window,worker,shadowrealm,
                          dedicatedworker,sharedworker,serviceworker).
                          Files with no META global= default to "window".
      --exclude-global <scopes>  skip .any.js whose META global= set intersects
                          <scopes>. e.g. --exclude-global window runs only the
                          non-window (DOM-free) variants.
  -h, --help              show this help

components: directories or .any.js files, resolved against the root
            (none = nothing is tested). Example: console url console/foo.any.js`;

const args = G.process.argv.slice(2);
root = G.process.env.AIKE_WPT_ROOT || G.process.cwd();
let failFile = null;
let logWptreport = null;
let globalFilter = null; // Set<string> of allowed scopes (intersection mode)
let globalExclude = null; // Set<string> of scopes to skip (intersection mode)
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
  else if (a === "--global") globalFilter = new Set(args[++i].split(","));
  else if (a.startsWith("--global="))
    globalFilter = new Set(a.slice("--global=".length).split(","));
  else if (a === "--exclude-global")
    globalExclude = new Set(args[++i].split(","));
  else if (a.startsWith("--exclude-global="))
    globalExclude = new Set(a.slice("--exclude-global=".length).split(","));
  else if (a === "-h" || a === "--help") {
    console.log(USAGE);
    G.process.exit(0);
  } else if (a.startsWith("-")) {
    console.error(`Unknown option: ${a}`);
    console.error(USAGE);
    G.process.exit(2);
  } else components.push(a);
}

// Recompute the base URL now that --wpt-server / --no-wpt-server have been applied.
wptBase = wptServerEnabled && wptServer ? wptServer.replace(/\/+$/, "") : null;

// WPT .sub.js fixtures (e.g. /common/get-host-info.sub.js) read self.location to
// derive the server protocol/host/origin at load time. Non-browser shells lack
// location, so we shim it from the resolved wpt server base URL (matching how
// Node/Bun WPT runners inject a location shim). Without this, get_host_info()
// throws "cannot read property of undefined".
//
// The pathname must be the *test file's* URL path, not the server root: tests
// build fixture URLs from it (e.g. fetch/api/basic/integrity.sub.any.js does
// `dirname(location.pathname) + "../resources/" + "top.txt"`). setLocation() is
// therefore called again per file, before the file runs.
function setLocation(url) {
  if (!wptServerEnabled || !url) return;
  try {
    const u = new URL(url);
    const location = {
      protocol: u.protocol,
      host: u.host,
      hostname: u.hostname,
      port: u.port,
      pathname: u.pathname || "/",
      search: u.search,
      href: u.href,
      // `location.origin` must carry the real port (`:8000` here) so the fetch
      // engine's CORS same-origin check matches the wpt server's
      // `Access-Control-Allow-Origin` header (which also includes the port).
      origin: u.origin,
      // `location.toString()` must return the document URL (same as `href`),
      // matching real browsers so tests that compare against `location.toString()`
      // resolve the expected referrer correctly.
      toString() {
        return u.href;
      },
    };
    G.location = location;
    G.self.location = location;
    // Expose the document URL/origin as plain globals so the fetch harness
    // (which may not see the decorated `self.location` native object) can use
    // them as the default referrer / client origin. `__wptClientUrl` is the full
    // document URL (with port), `__wptClientOrigin` the origin (with port) used
    // for CORS same-origin checks against the server's ACAO header.
    G.__wptClientUrl = u.href;
    G.__wptClientOrigin = u.origin;
    // WPT tests reference `get-host-info.sub.js` globals (e.g. HTTP_ORIGIN,
    // HTTP_NOTSAMESITE_ORIGIN) that are normally injected by that fixture. Since
    // the runner does not load dependency <script>s, define them here from the
    // current host so tests referencing them resolve.
    const host = u.hostname; // e.g. web-platform.test
    const port = u.port || (u.protocol === "https:" ? "443" : "80");
    const portElided = port === "80" || port === "443" ? "" : `:${port}`;
    const remoteHost = host.startsWith("www1.") ? host : `www1.${host}`;
    const notSameSiteHost = host.startsWith("not.") ? host : `not.${host}`;
    G.HTTP_PORT = port;
    G.HTTP_ORIGIN = `http://${host}${portElided}`;
    G.HTTP_REMOTE_ORIGIN = `http://${remoteHost}${portElided}`;
    G.HTTP_NOTSAMESITE_ORIGIN = `http://${notSameSiteHost}${portElided}`;
    G.HTTP_NOTSAMESITE_ORIGIN_DOMAIN = `http://${notSameSiteHost}${portElided}`;
    G.REMOTE_ORIGIN = G.HTTP_REMOTE_ORIGIN;
    G.NOTSAMESITE_ORIGIN = G.HTTP_NOTSAMESITE_ORIGIN;
  } catch (e) {
    console.error(`# WARNING: failed to shim self.location from ${url}: ${e}`);
  }
}

setLocation(wptServer && wptServer.replace(/\/+$/, ""));

if (logWptreport && !logWptreport.startsWith("/"))
  logWptreport = join(G.process.cwd(), logWptreport);
const timeStart = Date.now() / 1000;
// Resolve to an absolute path: fs.* resolves relative paths against the
// process cwd, while import() resolves them against the script's directory.
repoRoot = root.startsWith("/") ? root : join(G.process.cwd(), root);

// If no component is specified, run nothing.
if (components.length === 0) {
  console.log("No component specified; nothing to test.");
  console.log(
    "Usage: aike wpt-runner.mjs -r <wpt-root> <component> [component ...]",
  );
  G.process.exit(0);
}

// After CLI/env resolution, locate and load upstream testharness.js. The WPT
// root (-r / $AIKE_WPT_ROOT) is the preferred source; fall back to
// a few well-known checkouts.
testharnessPath = await findTestharness([
  root,
  G.process.env.AIKE_WPT_ROOT,
  join(G.process.env.HOME || "", "devel/wpt"),
  G.process.cwd(),
]);
if (!testharnessPath) {
  console.log(
    "ERROR: could not find resources/testharness.js. Set AIKE_WPT_ROOT or pass -r <wpt-root>, or place a WPT checkout at ~/devel/wpt.",
  );
  G.process.exit(1);
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
// Must comfortably exceed the slowest file: a file cut off here leaves tests
// pending, and they then settle while a later file is running.
const PROMISE_SETTLE_TIMEOUT_MS = 30000;

// ---------------------------------------------------------------------------
// Single-file execution
// ---------------------------------------------------------------------------

async function runOneFile(file) {
  currentAbsFile = file;
  currentFile = basename(file);

  // Point location at this test file's URL, as a browser would, so tests that
  // derive fixture paths from location.pathname resolve to the right directory.
  setLocation(fileHttpUrl(file));

  // Fresh harness realm: re-evaluate testharness.js so the global Tests
  // instance (and its callback lists) reset, then register our collectors.
  pendingResults = [];
  runDone = false;
  Aike.evalScript(testharnessText);
  registerHarnessCallbacks();

  // testharness.js may redefine `self`/`location` globals, clobbering the shim
  // we injected above. Re-apply it now (after testharness) so the fetch engine
  // still sees a valid client URL/origin for referrer + CORS checks.
  setLocation(fileHttpUrl(file));

  // Fetch the test file from the wpt server so `.sub.` placeholders are
  // expanded server-side (single source of truth). With no server, falls back
  // to a local read with local `.sub.` expansion.
  let code;
  try {
    code = await fetchText(file);
  } catch (e) {
    console.error(
      `# fetchText failed for ${file}: ${e && e.stack ? e.stack : e}`,
    );
    throw e;
  }
  await loadScriptDeps(file, code);

  // Run the test file as an ES module in a fresh module scope. This isolates
  // each file's top-level `const`/`let` from every other file (the global
  // lexical environment is NOT shared across modules), fixing the cross-file
  // redeclaration load errors (e.g. `SyntaxError: redeclaration of 'url'`)
  // that happened when all files shared one global script scope.
  //
  // Dependencies loaded via `META: script=` still use evalScript (global), so
  // their `var`/`const`/`function` declarations are visible to this module as
  // ordinary globals — matching how a browser loads the .html shell's <script>
  // deps before the test. The module's baseUrl is the file's wpt-server URL,
  // so relative fetch()/fs resolve against the server exactly as in a browser.
  // Assignments the test makes to globalThis (e.g. self.x = ...) still take
  // effect on the shared global.
  //
  // The test file is evaluated as a sloppy-mode script (so WPT code relying on
  // implicit globals — e.g. `result = await ...` without a declaration — works
  // as it does in a browser classic script), but wrapped in an async IIFE so
  // each file's top-level `const`/`let` stay scoped to that file. This avoids
  // the cross-file `redeclaration of 'url'` SyntaxErrors that sharing one
  // global script scope caused, while still allowing top-level `await`. An
  // async IIFE also keeps `this` as globalThis (arrow inherits the enclosing
  // global script's `this`) and lets promise callbacks created in the file
  // close over its top-level declarations.
  await Aike.evalScript(`(async () => {\n${code}\n})()`, {
    topLevelAwait: true,
    baseUrl: fileHttpUrl(file),
  });

  // All test()/promise_test() calls in the file have now run synchronously.
  // Now allow completion and drive it: the ShellTestEnvironment hook set
  // self.__wpt_on_loaded, which marks the environment all_loaded and runs the
  // harness's on_loaded callback. With __wpt_allow_complete now true, that
  // callback's complete() actually runs. Synchronous tests are already done;
  // any pending promise_test callbacks finish during the wait loop below and
  // complete the run via notify_result's all_done() check.
  G.__wpt_allow_complete = true;
  if (typeof G.__wpt_on_loaded === "function") G.__wpt_on_loaded();
  // ShellTestEnvironment fires the on_loaded -> complete() callback from a
  // Promise.resolve().then() during harness init, i.e. *before* this file has
  // finished executing. At that point __wpt_allow_complete is still false, so
  // the guarded Tests.prototype.complete() returns early and is never called
  // again. Drive completion explicitly now that the flag is set, so the harness
  // settles into its COMPLETE phase and any completion-dependent flush happens.
  if (typeof G.done === "function") {
    G.done();
  }

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

// True if two sets share at least one element.
function hasIntersection(a, b) {
  for (const x of a) if (b.has(x)) return true;
  return false;
}

// Parse a .any.js file's `// META: global=...` declarations from its header.
// Returns the set of global scopes it targets. A file with no such META line
// is treated as window-only (the WPT default).
const globalMetaCache = new Map();
async function parseGlobalMeta(file) {
  if (globalMetaCache.has(file)) return globalMetaCache.get(file);
  const text = await fs.readText(file);
  const scopes = new Set();
  // Only scan the leading META block (header lines before the first code).
  const head = text.split("\n").slice(0, 40);
  for (const line of head) {
    const m = line.match(/^\s*\/\/\s*META:\s*global=(.*)$/);
    if (m) {
      for (const s of m[1].split(",")) {
        const scope = s.trim();
        if (scope) scopes.add(scope);
      }
    }
  }
  if (scopes.size === 0) scopes.add("window");
  globalMetaCache.set(file, scopes);
  return scopes;
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
      if (e.name.endsWith(".any.js") && e.name !== "idlharness.any.js") {
        if (globalFilter || globalExclude) {
          const scopes = await parseGlobalMeta(p);
          if (globalFilter && !hasIntersection(scopes, globalFilter)) continue;
          if (globalExclude && hasIntersection(scopes, globalExclude)) continue;
        }
        out.push(p);
      }
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
    const why = globalFilter
      ? ` matching --global ${[...globalFilter].join(",")}`
      : globalExclude
        ? ` not matching --exclude-global ${[...globalExclude].join(",")}`
        : "";
    console.log(`  No .any.js test files found${why}`);
    return null;
  }

  if (globalFilter || globalExclude) {
    console.log(
      `=== ${comp} === (global filter active: ${files.length} file(s))`,
    );
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

// No test files resolved for any component: nothing to run, just exit.
if (summaries.length === 0) {
  console.error(`No test files found for components: ${components.join(", ")}`);
  G.process.exit(2);
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

// wptreport JSON (format consumed by wpt.fyi), one entry per test file.
if (logWptreport) {
  // Map a test status to the WPT subtest status vocabulary.
  function wptStatus(status) {
    if (status === "PASS") {
      return "PASS";
    } else if (status === "RUNNING") {
      return "TIMEOUT"; // promise_test never settled
    } else {
      return "FAIL";
    }
  }

  function wptTestPath(absFile) {
    const rel =
      absFile && absFile.startsWith(repoRoot + "/")
        ? absFile.slice(repoRoot.length)
        : absFile;
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
    const loadErr = recs.find((r) => r.kind === "load-error");
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
      entry.subtests = recs.map((r) => ({
        name: r.name,
        status: wptStatus(r.status),
        expected: "PASS",
        message: r.message || null,
      }));
    }
    reportResults.push(entry);
  }
  reportResults.sort((a, b) =>
    a.test < b.test ? -1 : a.test > b.test ? 1 : 0,
  );

  const report = {
    run_info: { product: "aike", platform: G.process.platform },
    time_start: timeStart,
    time_end: Date.now() / 1000,
    results: reportResults,
    subsuites: {},
  };
  await fs.writeText(logWptreport, JSON.stringify(report, null, 2) + "\n");
  console.log(`wptreport written to ${logWptreport}`);
}

G.process.exit(totalFailed > 0 ? 1 : 0);
