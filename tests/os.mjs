// 🖥️ os module — platform identity + XDG base directories
//
// Node-compatible: EOL, devNull, arch(), platform(), type(), version(),
// uptime(), endianness(), availableParallelism(), homedir(), tmpdir(),
// constants.{signals, priority, dlopen}.
// aike extensions: configDir, configLocalDir, dataDir, dataLocalDir,
// cacheDir, stateDir, runtimeDir, executableDir, preferenceDir, audioDir,
// desktopDir, documentDir, downloadDir, fontDir, pictureDir, publicDir,
// templateDir, videoDir. These return `string | null` — runtimeDir(),
// templateDir() and executableDir() are always null off Linux, and the
// others are null when $HOME (or the platform directory) is
// undeterminable. All 20 directory lookups share one native handler
// (`js_dir` in src/os.rs) distinguished by a magic constant.

import * as os from "os";
import * as path from "path";

const IS_WIN = process.platform === "win32";

let pass = 0;
let fail = 0;

function check(label, ok, detail) {
  if (ok) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

// Directory may be a string or null; either is valid, but a string must be
// absolute and non-empty.
function checkDir(label, v) {
  if (v === null) {
    check(`${label} → null (undeterminable)`, true);
    return;
  }
  const ok = typeof v === "string" && v.length > 0 && path.isAbsolute(v);
  check(`${label} → absolute path`, ok, `got ${JSON.stringify(v)}`);
}

// ===================================================================
// 1. Node platform identity
// ===================================================================

function testIdentity() {
  console.log("\n=== os — platform identity ===");

  check(
    "arch() is a string",
    typeof os.arch() === "string" && os.arch().length > 0,
  );
  check("platform() is a string", typeof os.platform() === "string");
  check("type() is a string", typeof os.type() === "string");
  check(
    "version() is a string",
    typeof os.version() === "string" && os.version().length > 0,
  );
  check(
    "uptime() is a number > 0",
    typeof os.uptime() === "number" && os.uptime() > 0,
    `got ${os.uptime()}`,
  );
  check("endianness() is LE or BE", ["LE", "BE"].includes(os.endianness()));
  check(
    "availableParallelism() >= 1",
    os.availableParallelism() >= 1,
    `got ${os.availableParallelism()}`,
  );

  // Node-compatible spellings.
  check(
    `platform() is "win32" on Windows`,
    IS_WIN ? os.platform() === "win32" : true,
    `got ${os.platform()}`,
  );
  check(
    `type() is "Windows_NT" on Windows`,
    IS_WIN ? os.type() === "Windows_NT" : true,
    `got ${os.type()}`,
  );

  // EOL and devNull are properties in Node, not functions.
  check(
    "EOL is a string property",
    typeof os.EOL === "string" && os.EOL.length > 0,
  );
  check(
    "devNull is a string property",
    typeof os.devNull === "string" && os.devNull.length > 0,
  );
  check(
    `EOL is "\\r\\n" on Windows, "\\n" elsewhere`,
    IS_WIN ? os.EOL === "\r\n" : os.EOL === "\n",
    `got ${JSON.stringify(os.EOL)}`,
  );
  check(
    `devNull is "\\\\.\\nul" on Windows, "/dev/null" elsewhere`,
    IS_WIN ? os.devNull === "\\\\.\\nul" : os.devNull === "/dev/null",
    `got ${os.devNull}`,
  );

  // Node documents os.platform() and process.platform as equivalent.
  check(
    "arch() matches process.arch",
    os.arch() === process.arch,
    `${os.arch()} vs ${process.arch}`,
  );
  check(
    "platform() matches process.platform",
    os.platform() === process.platform,
    `${os.platform()} vs ${process.platform}`,
  );

  // os.version() is the OS/kernel version, not the runtime version.
  check(
    "version() is not the runtime version",
    os.version() !== process.version,
    `both are ${os.version()}`,
  );
  // System uptime, not process uptime — it must exceed process uptime.
  check(
    "uptime() exceeds process.uptime()",
    os.uptime() > process.uptime() + 1,
    `os ${os.uptime()} vs proc ${process.uptime()}`,
  );
}

// ===================================================================
// 2. Directories
// ===================================================================

function testDirs() {
  console.log("\n=== os — directories ===");

  check("homedir() is a function", typeof os.homedir === "function");
  check("tmpdir() is a function", typeof os.tmpdir === "function");

  // tmpdir() always resolves; homedir() is null only on a broken host.
  check("tmpdir() returns a non-empty string", os.tmpdir().length > 0);
  check("tmpdir() is an absolute path", path.isAbsolute(os.tmpdir()));
  if (os.homedir() !== null) {
    check("homedir() is an absolute path", path.isAbsolute(os.homedir()));
  } else {
    check("homedir() → null (undeterminable)", true);
  }

  checkDir("configDir()", os.configDir());
  checkDir("dataDir()", os.dataDir());
  checkDir("dataLocalDir()", os.dataLocalDir());
  checkDir("cacheDir()", os.cacheDir());
  checkDir("stateDir()", os.stateDir());
  checkDir("runtimeDir()", os.runtimeDir());

  // runtimeDir() is XDG-only: always null on Windows and macOS.
  if (!IS_WIN && process.platform !== "darwin") {
    check(
      "runtimeDir() resolves on this POSIX platform",
      os.runtimeDir() !== null,
    );
  } else {
    check("runtimeDir() → null on Windows/macOS", os.runtimeDir() === null);
  }

  // --- dirs 6.0.0 extensions: local config / executable / preference ---
  checkDir("configLocalDir()", os.configLocalDir());
  checkDir("executableDir()", os.executableDir());
  checkDir("preferenceDir()", os.preferenceDir());

  // executableDir() is the per-user bin dir; dirs only defines it off
  // Windows/macOS.
  if (!IS_WIN && process.platform !== "darwin") {
    check(
      "executableDir() resolves on this POSIX platform",
      os.executableDir() !== null,
    );
  } else {
    check(
      "executableDir() → null on Windows/macOS",
      os.executableDir() === null,
    );
  }
  // preferenceDir(): a separate path only on macOS; elsewhere it equals
  // configDir().
  if (process.platform === "darwin") {
    check(
      "preferenceDir() is ~/Library/Preferences on macOS",
      os.preferenceDir() !== null &&
        os.preferenceDir().endsWith("/Library/Preferences"),
      `got ${JSON.stringify(os.preferenceDir())}`,
    );
  } else if (os.configDir() !== null) {
    check(
      "preferenceDir() === configDir() off macOS",
      os.preferenceDir() === os.configDir(),
      `${JSON.stringify(os.preferenceDir())} vs ${JSON.stringify(os.configDir())}`,
    );
  }
  // configLocalDir() differs from configDir() on Windows only.
  if (IS_WIN) {
    check(
      "configLocalDir() !== configDir() on Windows",
      os.configLocalDir() !== os.configDir(),
    );
  } else if (os.configDir() !== null) {
    check(
      "configLocalDir() === configDir() off Windows",
      os.configLocalDir() === os.configDir(),
    );
  }

  // --- dirs 6.0.0 extensions: user media / shell directories ---
  for (const name of [
    "audioDir",
    "desktopDir",
    "documentDir",
    "downloadDir",
    "fontDir",
    "pictureDir",
    "publicDir",
    "templateDir",
    "videoDir",
  ]) {
    check(`${name}() is a function`, typeof os[name] === "function");
    checkDir(`${name}()`, os[name]());
  }

  // templateDir() is XDG-only like runtimeDir(): null on Windows/macOS.
  if (!IS_WIN && process.platform !== "darwin") {
    check(
      "templateDir() resolves on this POSIX platform",
      os.templateDir() !== null,
    );
  } else {
    check("templateDir() → null on Windows/macOS", os.templateDir() === null);
  }

  // All 20 directory lookups share one native handler (js_dir in src/os.rs,
  // magic-selected) — assert the full surface is exported, and none of these
  // names leaks onto the Aike namespace.
  const dirFns = [
    "homedir",
    "tmpdir",
    "configDir",
    "configLocalDir",
    "dataDir",
    "dataLocalDir",
    "cacheDir",
    "stateDir",
    "runtimeDir",
    "executableDir",
    "preferenceDir",
    "audioDir",
    "desktopDir",
    "documentDir",
    "downloadDir",
    "fontDir",
    "pictureDir",
    "publicDir",
    "templateDir",
    "videoDir",
  ];
  check(
    "all 20 directory APIs are exported as functions",
    dirFns.every((n) => typeof os[n] === "function"),
    dirFns.filter((n) => typeof os[n] !== "function").join(", "),
  );
  check(
    "directory APIs are not aliased on Aike",
    dirFns.every((n) => !(n in Aike)),
  );
}

// ===================================================================
// 3. Ownership: `os` owns these, `Aike` must not re-alias them
// ===================================================================

function testOwnership() {
  console.log("\n=== os ownership ===");

  check("Aike.homedir is not aliased", !("homedir" in Aike));
  check("Aike.tmpdir is not aliased", !("tmpdir" in Aike));
  check("Aike.signals is not aliased", !("signals" in Aike));
}

// ===================================================================

function testConstants() {
  console.log("\n=== os.constants ===");

  check(
    "constants is an object",
    typeof os.constants === "object" && os.constants !== null,
  );
  const sig = os.constants.signals;
  check(
    "constants.signals is an object",
    typeof sig === "object" && sig !== null,
  );
  check("signals.SIGINT is a number", typeof sig.SIGINT === "number");
  check("signals.SIGTERM is a number", typeof sig.SIGTERM === "number");
  check(
    "signals.SIGUSR1 is number (Unix) or undefined (Windows)",
    sig.SIGUSR1 === undefined || typeof sig.SIGUSR1 === "number",
  );
  // The single canonical signal table — there is no Aike-side alias.
  check("SIGINT === 2", sig.SIGINT === 2);
  check("SIGTERM === 15", sig.SIGTERM === 15);

  const pri = os.constants.priority;
  check("priority is an object", typeof pri === "object" && pri !== null);
  for (const k of ["PRIORITY_LOW", "PRIORITY_NORMAL", "PRIORITY_HIGHEST"]) {
    check(
      `priority.${k} is a number`,
      typeof pri[k] === "number",
      `got ${pri[k]}`,
    );
  } // Node maps these to six priority constants, but the *numbers* are only
  // monotonic on POSIX: on Windows they are priority classes, whose ordering
  // differs. So check the platform's actual value set instead of ordering.
  const expectedPriority = IS_WIN
    ? { PRIORITY_LOW: 64, PRIORITY_NORMAL: 32, PRIORITY_HIGHEST: 256 }
    : { PRIORITY_LOW: 19, PRIORITY_NORMAL: 0, PRIORITY_HIGHEST: -20 };
  for (const [k, v] of Object.entries(expectedPriority)) {
    check(`priority.${k} is ${v}`, pri[k] === v, `got ${pri[k]}`);
  }
  // Ordering holds only for the POSIX nice values.
  if (!IS_WIN) {
    check(
      "priority ordering is monotonic (POSIX nice values)",
      pri.PRIORITY_LOW > pri.PRIORITY_BELOW_NORMAL &&
        pri.PRIORITY_BELOW_NORMAL > pri.PRIORITY_NORMAL &&
        pri.PRIORITY_NORMAL > pri.PRIORITY_ABOVE_NORMAL &&
        pri.PRIORITY_ABOVE_NORMAL > pri.PRIORITY_HIGH &&
        pri.PRIORITY_HIGH > pri.PRIORITY_HIGHEST,
      JSON.stringify(pri),
    );
  }

  // dlopen exists for shape compatibility; it is empty on Windows.
  const dlo = os.constants.dlopen;
  check("dlopen is an object", typeof dlo === "object" && dlo !== null);
  if (IS_WIN) {
    check(
      "dlopen is empty on Windows",
      Object.keys(dlo).length === 0,
      JSON.stringify(dlo),
    );
  } else {
    check("dlopen.RTLD_LAZY is a number", typeof dlo.RTLD_LAZY === "number");
    check("dlopen.RTLD_NOW is a number", typeof dlo.RTLD_NOW === "number");
  }
}

// ===================================================================
// Runner
// ===================================================================

testIdentity();
testDirs();
testConstants();
testOwnership();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
