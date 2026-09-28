// Aike.onSignal tests.
//
// Coverage:
//   * namespace / argument validation / registration semantics (all platforms)
//   * real signal delivery through the fixture (Unix: process.kill to a child)
//   * GC survival of the stored callback
//
// Windows can only raise a catchable signal as a console CTRL_C_EVENT, which a
// JS process cannot synthesize portably — that path is covered separately by
// tests/signals/ctrlc-test.ps1, run from a real console:
//
//   powershell -ExecutionPolicy Bypass -File tests\signals\ctrlc-test.ps1
//
// Usage: aike -m tests/signals/signal.mjs

import * as OS from "os";
import * as Path from "path";

// Resolve the fixture next to this test (works on Windows paths too).
const FIXTURE = Path.fileURLToPath(new URL("./fixture.mjs", import.meta.url));

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  \u2713 ${label}`);
    pass++;
  } else {
    console.log(`  \u2717 ${label}${detail ? " \u2014 " + detail : ""}`);
    fail++;
  }
}

// ─────────────────────────────────────────────────────────────────────────
// 1. namespace & argument validation
// ─────────────────────────────────────────────────────────────────────────
console.log("\n=== os.constants.signals ===");
check("os.constants.signals is an object", typeof OS.constants.signals === "object" && OS.constants.signals !== null);
check("signals.SIGINT is a number", typeof OS.constants.signals.SIGINT === "number");
check("signals.SIGTERM is a number", typeof OS.constants.signals.SIGTERM === "number");
check("Aike.signals is not aliased", !("signals" in Aike));
check("onSignal is a function", typeof Aike.onSignal === "function");

let err = null;
try {
  Aike.onSignal("SIGFOO", () => {});
} catch (e) {
  err = e;
}
check("unknown signal throws TypeError", err instanceof TypeError, String(err));

err = null;
try {
  Aike.onSignal("SIGINT", 42);
} catch (e) {
  err = e;
}
check("non-callable handler throws TypeError", err instanceof TypeError, String(err));

check("onSignal returns Aike (chainable)", Aike.onSignal("SIGINT", () => {}) === Aike);

err = null;
try {
  process.on("SIGINT", () => {});
} catch (e) {
  err = e;
}
check("process.on('SIGINT') throws TypeError (use Aike.onSignal)", err instanceof TypeError, String(err));

// unregister via null: first removal and a redundant second removal are both no-ops.
err = null;
try {
  Aike.onSignal("SIGINT", null);
  Aike.onSignal("SIGINT", null);
} catch (e) {
  err = e;
}
check("unregister(null) does not throw (incl. re-unregister)", err === null, String(err));

// ─────────────────────────────────────────────────────────────────────────
// 2. stored callback survives explicit GC (portable smoke)
// ─────────────────────────────────────────────────────────────────────────
console.log("\n=== GC survival ===");
Aike.onSignal("SIGINT", function probe() {});
for (let i = 0; i < 8; i++) {
  const junk = [];
  for (let k = 0; k < 5000; k++) junk.push({ x: k, s: "s" + k });
  Aike.gc();
}
let gcOk = true;
try {
  Aike.onSignal("SIGINT", null);
} catch (e) {
  gcOk = false;
}
check("registered handler survives GC and stays removable", gcOk);

// ─────────────────────────────────────────────────────────────────────────
// 3. real delivery
// ─────────────────────────────────────────────────────────────────────────
// SIGUSR1 has no aike default handler, so after unregister the OS default
// action (terminate) is what the "unregister" scenario must observe.
const SIG = "SIGUSR1";

async function runScenario(scenario) {
  const child = Aike.spawn(process.execPath, ["-m", FIXTURE, scenario, SIG], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "inherit",
  });

  let out = "";
  child.stdout.onLine((line) => {
    if (line !== null) out += line + "\n";
  });

  // Wait until the fixture has registered its handlers.
  for (let i = 0; i < 200 && !out.includes("READY"); i++) {
    await Aike.sleep(25);
  }

  // READY implies the child registered successfully and is alive; guard the
  // send anyway so a racing exit reports as a plain assertion failure.
  let sent = false;
  if (out.includes("READY")) {
    try {
      process.kill(child.pid, SIG);
      sent = true;
    } catch {
      sent = false;
    }
  }

  const code = await Promise.race([child.exitCode, Aike.sleep(5000).then(() => "timeout")]);
  const signal = await Promise.race([child.signalCode, Aike.sleep(1000).then(() => null)]);
  if (code === "timeout") child.kill("SIGKILL");
  return { out, code, signal, sent };
}

if (process.platform === "win32") {
  console.log("\n=== real delivery ===");
  console.log("  (skipped on Windows \u2014 run tests/signals/ctrlc-test.ps1 for CTRL_C_EVENT delivery)");
} else {
  console.log(`\n=== real delivery (${SIG}, child process) ===`);

  // control: no handler -> OS default action terminates the child.
  {
    const r = await runScenario("control");
    check(
      "control: terminated by the signal",
      r.sent && r.signal === SIG,
      `sent=${r.sent} code=${r.code} signal=${r.signal}`,
    );
    check("control: no handler output", !/HANDLER-/.test(r.out), r.out.trim());
  }

  // simple: one handler fires.
  {
    const r = await runScenario("simple");
    check(
      "simple: handler fires, exits 0",
      r.sent && r.code === 0 && r.out.includes("HANDLER-fired"),
      `sent=${r.sent} code=${r.code} out=${JSON.stringify(r.out.trim())}`,
    );
  }

  // gc: handler survives explicit GCs before delivery.
  {
    const r = await runScenario("gc");
    check(
      "gc: handler survives GC",
      r.sent && r.code === 0 && r.out.includes("HANDLER-fired"),
      `sent=${r.sent} code=${r.code} out=${JSON.stringify(r.out.trim())}`,
    );
  }

  // overwrite: last registration wins.
  {
    const r = await runScenario("overwrite");
    check(
      "overwrite: last registration wins",
      r.sent && r.code === 0 && r.out.includes("HANDLER-second") && !r.out.includes("HANDLER-first"),
      `sent=${r.sent} out=${JSON.stringify(r.out.trim())}`,
    );
  }

  // unregister: user handler removed, default action restored.
  {
    const r = await runScenario("unregister");
    check(
      "unregister: default action restored",
      r.sent && r.signal === SIG && !/HANDLER-/.test(r.out),
      `sent=${r.sent} code=${r.code} signal=${r.signal} out=${JSON.stringify(r.out.trim())}`,
    );
  }

  // reregister: unregister then register again -> the new handler fires.
  {
    const r = await runScenario("reregister");
    check(
      "reregister: second handler fires",
      r.sent && r.code === 0 && r.out.includes("HANDLER-second") && !r.out.includes("HANDLER-first"),
      `sent=${r.sent} out=${JSON.stringify(r.out.trim())}`,
    );
  }
}

// ─────────────────────────────────────────────────────────────────────────
console.log(`\n  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
  process.exit(1);
}
console.log("  ALL TESTS PASSED");
