// Spawn tests — timeout, strict option validation, and child lifetime
// (fire-and-forget vs awaited vs abortOnParentExit).
// Child-lifetime scenarios run a nested aike (process.execPath) via the
// fixtures so each has a clean parent whose exit path we control.

import { sleepCmd, echoCmd } from "./fixtures/platform.mjs";
import * as path from "path";

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

const dec = new TextDecoder();
function strip(s) {
  return s.replace(/\u001b\[[0-9;]*m/g, "").trim();
}
function out(r) {
  return strip(dec.decode(r.stdout));
}
function err(r) {
  return strip(dec.decode(r.stderr));
}

// Unpack [cmd, args] tuples from platform.mjs into spawn calls.
function spawnSyncC(tuple, opts) {
  return opts === undefined
    ? Aike.spawnSync(tuple[0], tuple[1])
    : Aike.spawnSync(tuple[0], tuple[1], opts);
}
function spawnC(tuple, opts) {
  return opts === undefined
    ? Aike.spawn(tuple[0], tuple[1])
    : Aike.spawn(tuple[0], tuple[1], opts);
}

// fileURLToPath, not URL.pathname — pathname keeps the leading "/C:/" on
// Windows, which Win32 APIs reject (os error 123).
const FIXTURES = path.fileURLToPath(new URL("./fixtures/", import.meta.url));
function runFixture(flags, { timeout } = {}) {
  return Aike.spawnSync(process.execPath, ["-m", `${FIXTURES}child.mjs`, ...flags], { timeout });
}

function alivePid(pid) {
  if (process.platform === "windows") {
    const r = Aike.spawnSync("tasklist", ["/FI", `PID eq ${pid}`]);
    return out(r).includes(String(pid));
  }
  const r = Aike.spawnSync("kill", ["-0", String(pid)]);
  return r.exitCode === 0;
}

function reap(pid) {
  if (process.platform === "windows") {
    Aike.spawnSync("taskkill", ["/F", "/PID", String(pid)]);
  } else {
    Aike.spawnSync("kill", ["-9", String(pid)]);
  }
}

// ===================================================================
// spawnSync without timeout — baseline
// ===================================================================
function testSyncBaseline() {
  console.log("\n=== spawnSync without timeout ===");

  const r = spawnSyncC(echoCmd("plain"));
  check("normal run succeeds", r.success, `exitCode=${r.exitCode}`);
  check("normal run has real pid", r.pid > 0, `pid=${r.pid}`);
  check("stdout captured", out(r) === "plain", `got ${JSON.stringify(out(r))}`);
  check("killedByTimeout false", r.killedByTimeout === false);
}

// ===================================================================
// spawnSync with timeout
// ===================================================================
function testSyncTimeout() {
  console.log("\n=== spawnSync timeout ===");

  const fast = spawnSyncC(sleepCmd(1), { timeout: 10000 });
  check("fast child succeeds", fast.success, `exitCode=${fast.exitCode}`);
  check("fast child not timed out", fast.killedByTimeout === false);

  const t = Date.now();
  const slow = spawnSyncC(sleepCmd(10), { timeout: 700 });
  const elapsed = Date.now() - t;
  check("slow child timed out", slow.killedByTimeout === true, `timeout=${slow.killedByTimeout}`);
  check("slow child did not finish normally", !slow.success);
  check("slow child killed within limit", elapsed < 5000, `elapsed=${elapsed}ms`);
}

// ===================================================================
// spawn (async) timeout
// ===================================================================
async function testAsyncTimeout() {
  console.log("\n=== spawn timeout (async) ===");

  const fast = spawnC(sleepCmd(1), { timeout: 10000 });
  const [fe, fk] = await Promise.all([fast.exitCode, fast.killedByTimeout]);
  check("async fast exitCode", fe === 0, `exitCode=${fe}`);
  check("async fast not timed out", fk === false, `killed=${fk}`);

  const t = Date.now();
  const slow = spawnC(sleepCmd(10), { timeout: 700 });
  const [se, sk] = await Promise.all([slow.exitCode, slow.killedByTimeout]);
  const elapsed = Date.now() - t;
  check("async slow timed out", sk === true, `killed=${sk}`);
  check("async slow killed within limit", elapsed < 5000, `elapsed=${elapsed}ms`);
  check("async slow exitCode is not 0", se !== 0, `exitCode=${se}`);
}

// ===================================================================
// Tree kill: a background grandchild holds the stdout pipe; the timeout
// must kill the whole tree so spawnSync returns instead of hanging.
// ===================================================================
function testTreeKill() {
  console.log("\n=== timeout kills the whole tree ===");

  if (process.platform !== "windows") {
    console.log("  ⏭ skipped (Windows-only tree-kill check)");
    return;
  }

  const t = Date.now();
  const r = Aike.spawnSync(
    "powershell",
    ["-NoProfile", "-Command",
      "Start-Process -NoNewWindow cmd -ArgumentList '/c ping -n 20 127.0.0.1'; Start-Sleep -Seconds 20"],
    { timeout: 1500 },
  );
  const elapsed = Date.now() - t;
  check("tree killed and returned", r.killedByTimeout === true, `timeout=${r.killedByTimeout}`);
  check("returned within limit (no hang)", elapsed < 10000, `elapsed=${elapsed}ms`);
}

// ===================================================================
// Strict option validation — wrong types throw TypeError/RangeError.
// ===================================================================
function testStrictValidation() {
  console.log("\n=== strict option validation ===");

  function throws(fn, name) {
    try {
      fn();
      return null;
    } catch (e) {
      return e.name === name ? e : e.name;
    }
  }

  check("timeout:'abc' throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: "abc" }), "TypeError") !== null);
  check("timeout:100.5 throws RangeError", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: 100.5 }), "RangeError") !== null);
  check("timeout:0 throws RangeError", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: 0 }), "RangeError") !== null);
  check("timeout:-1 throws RangeError", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: -1 }), "RangeError") !== null);
  check("timeout:null throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: null }), "TypeError") !== null);
  check("timeout:undefined does not throw", throws(
    () => spawnSyncC(echoCmd("x"), { timeout: undefined }), "x") === null);

  check("cwd:123 throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { cwd: 123 }), "TypeError") !== null);
  check("env non-object throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { env: "x" }), "TypeError") !== null);
  check("env non-string value throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { env: { FOO: 1 } }), "TypeError") !== null);
  check("stdin:'foo' throws RangeError", throws(
    () => spawnSyncC(echoCmd("x"), { stdin: "foo" }), "RangeError") !== null);
  check("input:123 throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { input: 123 }), "TypeError") !== null);
  check("input:null throws TypeError", throws(
    () => spawnSyncC(echoCmd("x"), { input: null }), "TypeError") !== null);

  check("command:123 throws TypeError", throws(
    () => Aike.spawnSync(123), "TypeError") !== null);
  check("args:'str' throws TypeError", throws(
    () => Aike.spawnSync("sh", "str"), "TypeError") !== null);
  check("args:null throws TypeError", throws(
    () => Aike.spawnSync("sh", null), "TypeError") !== null);
  check("non-string arg element throws TypeError", throws(
    () => Aike.spawnSync("sh", [1]), "TypeError") !== null);
  check("options:'str' throws TypeError", throws(
    () => Aike.spawnSync("sh", [], "str"), "TypeError") !== null);
  check("options:null throws TypeError", throws(
    () => Aike.spawnSync("sh", [], null), "TypeError") !== null);

  check("async timeout:'abc' throws TypeError", throws(
    () => spawnC(sleepCmd(1), { timeout: "abc" }), "TypeError") !== null);
  check("abortOnParentExit:'yes' throws TypeError", throws(
    () => spawnC(sleepCmd(1), { abortOnParentExit: "yes" }), "TypeError") !== null);
}

// ===================================================================
// Fire-and-forget: aike must NOT wait for the child, and the child
// must survive the parent's normal exit.
// ===================================================================
function testFireForget() {
  console.log("\n=== fire-and-forget (unawaited, no option) ===");

  const t = Date.now();
  const r = runFixture([]);
  const elapsed = Date.now() - t;
  check("parent exits without waiting for child", r.exitCode === 0 && elapsed < 3000, `elapsed=${elapsed}ms exit=${r.exitCode}`);
  const pid = parseInt(out(r), 10);
  check("child pid printed", Number.isInteger(pid) && pid > 0, `got ${JSON.stringify(out(r))}`);
  const alive = alivePid(pid);
  check("child survives parent exit (reparented)", alive, `pid=${pid} stderr=${err(r)}`);
  if (alive) reap(pid);
}

// ===================================================================
// Awaited: parent waits for the child and gets its exit code.
// ===================================================================
function testAwaited() {
  console.log("\n=== awaited spawn ===");

  const t = Date.now();
  const r = runFixture(["--seconds", "2", "--wait"]);
  const elapsed = Date.now() - t;
  check("parent waits ~2s for child", elapsed >= 1500 && elapsed < 8000, `elapsed=${elapsed}ms`);
  check("exit code 0", out(r).includes("code 0"), out(r));
}

// ===================================================================
// abortOnParentExit: child is killed when parent exits normally.
// ===================================================================
function testAbortNormalExit() {
  console.log("\n=== abortOnParentExit, normal exit ===");

  const t = Date.now();
  const r = runFixture(["--abort"]);
  const elapsed = Date.now() - t;
  check("parent exits fast", r.exitCode === 0 && elapsed < 3000, `elapsed=${elapsed}ms`);
  const pid = parseInt(out(r), 10);
  const alive = alivePid(pid);
  check("child killed on parent exit", !alive, `pid=${pid}`);
  if (alive) reap(pid);
}

// ===================================================================
// abortOnParentExit + process.exit(0): same kill behavior.
// ===================================================================
function testAbortProcessExit() {
  console.log("\n=== abortOnParentExit + process.exit(0) ===");

  const r = runFixture(["--abort", "--process-exit"]);
  check("parent exited via process.exit", r.exitCode === 0, `exit=${r.exitCode} stderr=${err(r)}`);
  const pid = parseInt(out(r), 10);
  const alive = alivePid(pid);
  check("child killed on process.exit", !alive, `pid=${pid}`);
  if (alive) reap(pid);
}

// ===================================================================
// abortOnParentExit + external SIGTERM to the parent (unix only).
// ===================================================================
async function testAbortSignal() {
  console.log("\n=== abortOnParentExit, parent SIGTERMed ===");

  if (process.platform === "windows") {
    console.log("  ⏭ signal test skipped on windows (no SIGTERM)");
    return;
  }

  const r = Aike.spawn(process.execPath, ["-m", `${FIXTURES}child.mjs`, "--abort", "--wait"]);
  // Give the fixture time to spawn its own child before scanning.
  await new Promise((res) => setTimeout(res, 700));
  let childPid = null;

  const list = Aike.spawnSync("ps", ["ax", "-o", "pid,ppid,command"]);
  for (const line of dec.decode(list.stdout).split("\n")) {
    const m = line.trim().match(/^(\d+)\s+(\d+)\s+(.+)$/);
    if (m && Number(m[2]) === r.pid && /sleep/.test(m[3])) {
      childPid = Number(m[1]);
      break;
    }
  }
  check("found fixture's child", childPid !== null, `parent=${r.pid}`);
  const aliveBefore = childPid ? alivePid(childPid) : false;
  check("child alive before signal", aliveBefore);
  if (!aliveBefore) return;

  r.kill("SIGTERM");
  const rk = await r.exitCode;
  check("parent exited on SIGTERM", rk !== 0, `code=${rk}`);
  check("child killed on SIGTERM", !alivePid(childPid), `pid=${childPid}`);
}

// ===================================================================
// sandbox option — directory read/write restrictions, mount ro/rw,
// descendant inheritance, strict sandbox validation.
//
// Scratch dirs live under $HOME (NOT /tmp): the sandbox profile hardcodes
// /tmp as writable, so /tmp-based paths can't detect write restriction.
// Writes are asserted by the write's own rc (`...; echo rc=$?`), because
// a trailing `echo`/`wait` would mask a failed redirect.
// ===================================================================
async function testSandbox() {
  console.log("\n=== sandbox option: directory read/write restrictions ===");

  const fs = await import("fs");
  const base = `${process.env.HOME}/.aike-spawn-sandbox-test-${process.pid}`;
  const ws = `${base}/wd`;
  const ro = `${base}/ro-data`;
  const rw = `${base}/rw-data`;
  const outside = `${base}/outside.txt`;
  await fs.remove(base).catch(() => {});
  await fs.makeDir(ws, { recursive: true });
  await fs.makeDir(ro, { recursive: true });
  await fs.makeDir(rw, { recursive: true });
  await fs.writeText(`${ws}/ws-file.txt`, "WS");
  await fs.writeText(`${ro}/secret.txt`, "SECRET");
  await fs.writeText(`${rw}/rw-file.txt`, "RW");

  const mounts = {
    [ws]: { writable: true },               // cwd — writable
    [ro]: { target: "/mnt-data" },          // ro mount
    [rw]: { target: "/mnt-rw", writable: true }, // rw mount
  };
  const sandboxOpt = { cwd: ws, mounts, network: "host" };

  // rc of the write itself: 0 → allowed, non-zero → denied.
  async function writeRc(script) {
    const r = Aike.spawnSync("sh", ["-c", `${script} 2>/dev/null; echo rc=$?`], { sandbox: sandboxOpt });
    const m = out(r).match(/rc=(\d+)/);
    return m ? Number(m[1]) : -1;
  }
  function readOut(script) {
    const r = Aike.spawnSync("sh", ["-c", script], { sandbox: sandboxOpt });
    return out(r);
  }

  // ── reads: everything readable (global read, write is the control point) ──
  check("read cwd file", readOut(`cat ${ws}/ws-file.txt`) === "WS");
  check("read ro source file", readOut(`cat ${ro}/secret.txt`) === "SECRET");
  check("read rw source file", readOut(`cat ${rw}/rw-file.txt`) === "RW");
  check("read system file", readOut("cat /etc/hosts").length > 0);

  // ── writes per directory ──
  check("write inside cwd allowed", (await writeRc(`echo x > ${ws}/new.txt`)) === 0);
  check("write $HOME/outside (whitelist) denied", (await writeRc(`echo x > ${outside}`)) !== 0);
  check("outside file not created", !(await fs.exists(outside)));

  // ro mount source
  check("write ro source denied", (await writeRc(`echo x >> ${ro}/secret.txt`)) !== 0);
  check("ro source content unchanged", (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

  // rw mount source
  check("write rw source allowed", (await writeRc(`echo x > ${rw}/new.txt`)) === 0);
  check("rw source content persisted", (await fs.readText(`${rw}/new.txt`)) === "x\n" || (await fs.readText(`${rw}/new.txt`)) === "x");

  // ── descendant inheritance ──
  check("grandchild write to ro denied", (await writeRc(`sh -c 'echo x > ${ro}/secret.txt'`)) !== 0);
  check("grandchild ro content unchanged", (await fs.readText(`${ro}/secret.txt`)) === "SECRET");

  // `& wait` always returns 0 → assert host file state instead
  Aike.spawnSync("sh", ["-c", `sh -c 'echo bg > ${ro}/secret.txt' & wait`], { sandbox: sandboxOpt });
  check("bg grandchild ro write denied (host state)", (await fs.readText(`${ro}/secret.txt`)) === "SECRET");
  Aike.spawnSync("sh", ["-c", `sh -c 'echo bg-rw > ${rw}/inherit.txt' & wait`], { sandbox: sandboxOpt });
  check("bg grandchild rw write allowed (host state)", (await fs.readText(`${rw}/inherit.txt`)).includes("bg-rw"));

  // ── async spawn: streaming stdout + timeout kill ──
  {
    const p = Aike.spawn("sh", ["-c", "echo streamed"], { sandbox: sandboxOpt, timeout: 5000 });
    let stdout = "";
    p.stdout?.onData((c) => { if (c) stdout += dec.decode(c, { stream: true }); });
    const code = await p.exitCode;
    check("async sandbox exit 0", code === 0);
    check("async sandbox streams stdout", stdout.trim() === "streamed");

    const t0 = Date.now();
    const p2 = Aike.spawn("sleep", ["60"], { sandbox: sandboxOpt, timeout: 1500 });
    const code2 = await p2.exitCode;
    const killed2 = await p2.killedByTimeout;
    check("async sandbox timeout killed", killed2 === true && code2 !== 0);
    check("async sandbox timeout within limit", Date.now() - t0 < 5000);
  }

  // ── no sandbox option → unrestricted (baseline) ──
  {
    const r = Aike.spawnSync("sh", ["-c", `echo x > ${outside}`]);
    check("without sandbox, $HOME write allowed", r.exitCode === 0 && (await fs.exists(outside)));
  }

  // ── strict validation ──
  {
    const throws = (fn) => { try { fn(); return null; } catch (e) { return e; } };
    check("sandbox missing cwd throws", throws(() =>
      Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { mounts } })) !== null);
    check("sandbox.cwd non-string throws TypeError", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: 123, mounts } }));
      return e?.name === "TypeError";
    })());
    check("sandbox.cwd nonexistent throws RangeError", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: "/no/such/dir", mounts } }));
      return e?.name === "RangeError";
    })());
    check("sandbox as array throws", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: [] }));
      return e !== null;
    })());
    check("mounts as array throws", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: ws, mounts: [] } }));
      return e !== null;
    })());
    check("mount value non-object throws", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: ws, mounts: { [ro]: "rw" } } }));
      return e !== null;
    })());
    check("mount writable non-boolean throws", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: ws, mounts: { [ro]: { writable: 1 } } } }));
      return e !== null;
    })());
    check("sandbox.network invalid throws", (() => {
      const e = throws(() => Aike.spawnSync("sh", ["-c", "echo x"], { sandbox: { cwd: ws, network: "wifi" } }));
      return e !== null;
    })());
  }

  await fs.remove(base).catch(() => {});
}

await testSyncBaseline();
await testSyncTimeout();
await testAsyncTimeout();
await testTreeKill();
await testStrictValidation();
await testFireForget();
await testAwaited();
await testAbortNormalExit();
await testAbortProcessExit();
await testAbortSignal();
await testSandbox();

console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}