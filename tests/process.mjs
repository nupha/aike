import * as Path from "path";

// --- Helpers ---
let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

// ===================================================================
// process.cwd / chdir tests
// ===================================================================
function testCwdChdir() {
  console.log("\n=== process.cwd/chdir ===");

  check("cwd is a function", typeof process.cwd === "function");
  const cwd = process.cwd();
  check("cwd returns a string", typeof cwd === "string");
  check("cwd is absolute path", Path.isAbsolute(cwd), `got ${cwd}`);
  check("cwd is non-empty", cwd.length > 0);

  check("chdir is a function", typeof process.chdir === "function");
  const orig = process.cwd();
  const parent = Path.dirname(orig);
  process.chdir(parent);
  check("chdir works", process.cwd() === parent, `got ${process.cwd()}`);
  process.chdir(orig);
  check("chdir restores", process.cwd() === orig);

  let chdirError = false;
  try {
    process.chdir("/this/dir/does/not/exist/for/sure/xyz");
  } catch (_) {
    chdirError = true;
  }
  check("chdir throws on invalid dir", chdirError);
}

// ===================================================================
// process.pid / ppid tests
// ===================================================================
function testPidPpid() {
  console.log("\n=== process.pid/ppid ===");

  check("pid is a number", typeof process.pid === "number", `got ${typeof process.pid}`);
  check("pid is positive", process.pid > 0, `got ${process.pid}`);
  check("ppid is a number", typeof process.ppid === "number", `got ${typeof process.ppid}`);
  check("ppid is positive", process.ppid > 0, `got ${process.ppid}`);
}

// ===================================================================
// process.arch / version / versions tests
// ===================================================================
function testVersionArch() {
  console.log("\n=== process.arch/version/versions ===");

  check("arch is a string", typeof process.arch === "string", `got ${typeof process.arch}`);
  check("arch is non-empty", process.arch.length > 0);

  check("version is a string", typeof process.version === "string", `got ${typeof process.version}`);
  check("version starts with 'v'", process.version.startsWith("v"), `got ${process.version}`);

  check("versions is an object", typeof process.versions === "object" && process.versions !== null);
  check("versions.aike is a string", typeof process.versions.aike === "string", `got ${typeof process.versions.aike}`);
  check("versions.aike is non-empty", process.versions.aike.length > 0);
}

// ===================================================================
// process.platform / argv / execArgv / execPath tests
// ===================================================================
function testRuntime() {
  console.log("\n=== process.platform/argv/execArgv/execPath ===");

  check("platform is a string", typeof process.platform === "string", `got ${typeof process.platform}`);
  check("platform is non-empty", process.platform.length > 0);
  const knownPlatforms = ["win32", "darwin", "linux", "android", "freebsd", "openbsd", "aix", "sunos"];
  check(
    "platform is a known value",
    knownPlatforms.includes(process.platform),
    `got ${JSON.stringify(process.platform)}`,
  );

  check("argv is an array", Array.isArray(process.argv), `got ${typeof process.argv}`);
  check("argv has at least 1 element", process.argv.length >= 1);
  check("argv[0] is a string", typeof process.argv[0] === "string");

  check("execArgv is an array", Array.isArray(process.execArgv), `got ${typeof process.execArgv}`);

  check("execPath is a string", typeof process.execPath === "string", `got ${typeof process.execPath}`);
  check("execPath is absolute", Path.isAbsolute(process.execPath), `got ${process.execPath}`);
}

// ===================================================================
// process.title get/set tests
// ===================================================================
function testTitle() {
  console.log("\n=== process.title ===");

  const orig = process.title;
  check("title is a string", typeof orig === "string", `got ${typeof orig}`);
  check("title is non-empty", orig.length > 0, `got ${JSON.stringify(orig)}`);

  process.title = "aike-process-test-title";
  check("title can be set", process.title === "aike-process-test-title", `got ${process.title}`);
  process.title = orig;
  check("title can be restored", process.title === orig, `got ${process.title}`);
}

// ===================================================================
// process.uptime / hrtime tests
// ===================================================================
function testUptimeHrtime() {
  console.log("\n=== process.uptime/hrtime ===");

  const up = process.uptime();
  check("uptime is a number", typeof up === "number", `got ${typeof up}`);
  check("uptime is >= 0", up >= 0, `got ${up}`);

  check("hrtime is a function", typeof process.hrtime === "function");
  const t0 = process.hrtime();
  check("hrtime returns array", Array.isArray(t0) && t0.length === 2, `got ${JSON.stringify(t0)}`);
  check("hrtime[0] is number (sec)", typeof t0[0] === "number", `got ${typeof t0[0]}`);
  check("hrtime[1] is number (ns)", typeof t0[1] === "number" && t0[1] >= 0 && t0[1] < 1e9, `got ${t0[1]}`);

  const start = process.hrtime();
  let busy = 0;
  for (let i = 0; i < 1e6; i++) busy += i;
  const diff = process.hrtime(start);
  check("hrtime diff works", Array.isArray(diff) && diff.length === 2, `got ${JSON.stringify(diff)}`);
  const diffNanos = diff[0] * 1e9 + diff[1];
  check("hrtime diff is >= 0", diffNanos >= 0, `got ${diffNanos}`);
}

// ===================================================================
// process.memoryUsage / cpuUsage tests
// ===================================================================
function testUsage() {
  console.log("\n=== process.memoryUsage/cpuUsage ===");

  const mem = process.memoryUsage();
  check("memoryUsage returns object", typeof mem === "object" && mem !== null);
  check("rss is a number", typeof mem.rss === "number", `got ${typeof mem.rss}`);
  check("rss > 0", mem.rss > 0, `got ${mem.rss}`);
  for (const k of ["heapTotal", "heapUsed", "external"]) {
    check(`memoryUsage.${k} is a number`, typeof mem[k] === "number", `got ${typeof mem[k]}`);
    check(`memoryUsage.${k} >= 0`, mem[k] >= 0, `got ${mem[k]}`);
  }

  const cpu = process.cpuUsage();
  check("cpuUsage returns object", typeof cpu === "object" && cpu !== null);
  check("cpuUsage.user is a number", typeof cpu.user === "number", `got ${typeof cpu.user}`);
  check("cpuUsage.system is a number", typeof cpu.system === "number", `got ${typeof cpu.system}`);
  check("cpuUsage values >= 0", cpu.user >= 0 && cpu.system >= 0, `got ${JSON.stringify(cpu)}`);
}

// ===================================================================
// process.nextTick tests
// ===================================================================
async function testNextTick() {
  console.log("\n=== process.nextTick ===");

  check("nextTick is a function", typeof process.nextTick === "function");
  let ran = false;
  process.nextTick(() => {
    ran = true;
  });
  // nextTick callbacks run at the next microtask checkpoint; give the loop one turn.
  await Aike.sleep(0);
  check("nextTick callback ran", ran);
}

// ===================================================================
// process.umask tests
// ===================================================================
function testUmask() {
  console.log("\n=== process.umask ===");

  check("umask is a function", typeof process.umask === "function");
  if (process.platform !== "win32") {
    const prev = process.umask();
    check("umask() returns a number", typeof prev === "number", `got ${typeof prev}`);
    const next = process.umask(0o022);
    check("umask(set) returns previous", next === prev, `got ${next}, prev ${prev}`);
    process.umask(prev);
    check("umask restored", process.umask() === prev);
  } else {
    check("umask() returns 0 on Windows", process.umask() === 0, `got ${process.umask()}`);
  }
}

// ===================================================================
// process.kill tests
// ===================================================================
function testKill() {
  console.log("\n=== process.kill ===");

  check("kill is a function", typeof process.kill === "function");

  // A definitely non-existent pid should fail
  let noSuchPidError = false;
  try {
    process.kill(999999999, "SIGTERM");
  } catch (_) {
    noSuchPidError = true;
  }
  check("kill throws on non-existent pid", noSuchPidError);

  // Invalid pid (<= 0) throws TypeError
  let badPidError = false;
  try {
    process.kill(0, "SIGTERM");
  } catch (e) {
    badPidError = e instanceof TypeError;
  }
  check("kill throws TypeError on pid <= 0", badPidError);

  // Unknown signal name with a non-existent pid fails (no false success)
  let badSignalError = false;
  try {
    process.kill(999999998, "NOT_A_REAL_SIGNAL_99");
  } catch (_) {
    badSignalError = true;
  }
  check("kill fails on unknown signal", badSignalError);
}

// ===================================================================
// process.env / stdin / stdout / stderr sanity tests
// ===================================================================
function testStdio() {
  console.log("\n=== process.env/stdio ===");

  check("env is an object", typeof process.env === "object" && process.env !== null);
  const envKeys = Object.keys(process.env);
  const pathKey = envKeys.find(k => k.toUpperCase() === "PATH");
  check(
    "env has PATH (case-insensitive)",
    typeof pathKey === "string" && typeof process.env[pathKey] === "string",
    `got PATH=${typeof process.env.PATH}, Path=${typeof process.env.Path}`,
  );

  check("stdin.fd === 0", process.stdin.fd === 0, `got ${process.stdin.fd}`);
  check("stdout.fd === 1", process.stdout.fd === 1, `got ${process.stdout.fd}`);
  check("stderr.fd === 2", process.stderr.fd === 2, `got ${process.stderr.fd}`);
  process.stdout.writeSync("hello from process.stdout\n");
  check("stdout.writeSync works", true);
}

// ===================================================================
// process EventEmitter inheritance tests
// ===================================================================
function testEventEmitter() {
  console.log("\n=== process EventEmitter ===");

  check("process.on is a function", typeof process.on === "function");
  check("process.emit is a function", typeof process.emit === "function");
  check("process.once is a function", typeof process.once === "function");
  check("process.removeListener is a function", typeof process.removeListener === "function");
  check("process.eventNames is a function", typeof process.eventNames === "function");
  check("process.listenerCount is a function", typeof process.listenerCount === "function");
  check("process.removeAllListeners is a function", typeof process.removeAllListeners === "function");

  check("process.on returns this", process.on("greet", () => {}) === process);
  check("process.emit unknown returns false", process.emit("nope") === false);

  let got = null;
  process.on("ping", msg => {
    got = msg;
  });
  process.emit("ping", "hello");
  check("listener received args", got === "hello", `got ${got}`);

  let onceCount = 0;
  process.once("boom", () => {
    onceCount++;
  });
  process.emit("boom");
  process.emit("boom");
  check("once fired exactly once", onceCount === 1, `count ${onceCount}`);

  check("eventNames includes ping", process.eventNames().includes("ping"));
  check("listenerCount ping == 1", process.listenerCount("ping") === 1);
  process.removeAllListeners("ping");
  check("listenerCount ping == 0 after removeAll", process.listenerCount("ping") === 0);
}

// ===================================================================
// Main
// ===================================================================
await testCwdChdir();
await testPidPpid();
await testVersionArch();
await testRuntime();
await testTitle();
await testUptimeHrtime();
await testUsage();
await testNextTick();
await testUmask();
await testKill();
await testStdio();
await testEventEmitter();

// Summary
console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
