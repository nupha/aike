// Spawn timeout tests — Aike.spawn/spawnSync `timeout` option.

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
function out(r) {
  return dec.decode(r.stdout).trim();
}

// ===================================================================
// spawnSync without timeout — unchanged baseline behavior
// ===================================================================
function testSyncBaseline() {
  console.log("\n=== spawnSync without timeout ===");

  const r = Aike.spawnSync("cmd", ["/c", "echo plain"]);
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

  // Fast child finishes before the limit → not killed.
  const fast = Aike.spawnSync("cmd", ["/c", "echo quick"], {
    timeout: 10000,
  });
  check("fast child succeeds", fast.success, `exitCode=${fast.exitCode}`);
  check("fast child output", out(fast) === "quick");
  check("fast child not timed out", fast.killedByTimeout === false);

  // Slow child sleeps past the limit → killed.
  const t = Date.now();
  const slow = Aike.spawnSync(
    "powershell",
    ["-NoProfile", "-Command", "Start-Sleep -Seconds 10; 'late'"],
    { timeout: 700 },
  );
  const elapsed = Date.now() - t;
  check("slow child timed out", slow.killedByTimeout === true, `timeout=${slow.killedByTimeout}`);
  check("slow child did not finish normally", !slow.success);
  check("slow child killed within limit", elapsed < 5000, `elapsed=${elapsed}ms`);
  check("slow child no late output", !out(slow).includes("late"));
}

// ===================================================================
// spawn (async) timeout
// ===================================================================
async function testAsyncTimeout() {
  console.log("\n=== spawn timeout (async) ===");

  // Fast async child.
  const fast = Aike.spawn("cmd", ["/c", "echo quick"], { timeout: 10000 });
  const [fe, fk] = await Promise.all([fast.exitCode, fast.killedByTimeout]);
  check("async fast exitCode", fe === 0, `exitCode=${fe}`);
  check("async fast not timed out", fk === false, `killed=${fk}`);

  // Slow async child → killed by timeout.
  const t = Date.now();
  const slow = Aike.spawn("powershell", ["-NoProfile", "-Command", "Start-Sleep -Seconds 10; 'late'"], {
    timeout: 700,
  });
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

  // timeout
  check("timeout:'abc' throws TypeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { timeout: "abc" }), "TypeError") !== null);
  check("timeout:100.5 throws RangeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { timeout: 100.5 }), "RangeError") !== null);
  check("timeout:0 throws RangeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { timeout: 0 }), "RangeError") !== null);
  check("timeout:-1 throws RangeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { timeout: -1 }), "RangeError") !== null);
  check("timeout:null does not throw", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { timeout: null }), "x") === null);

  // other options
  check("cwd:123 throws TypeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { cwd: 123 }), "TypeError") !== null);
  check("env non-object throws TypeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { env: "x" }), "TypeError") !== null);
  check("env non-string value throws TypeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { env: { FOO: 1 } }), "TypeError") !== null);
  check("stdin:'foo' throws RangeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { stdin: "foo" }), "RangeError") !== null);
  check("input:123 throws TypeError", throws(
    () => Aike.spawnSync("cmd", ["/c", "echo"], { input: 123 }), "TypeError") !== null);

  // positional args
  check("command:123 throws TypeError", throws(
    () => Aike.spawnSync(123), "TypeError") !== null);
  check("args:'str' throws TypeError", throws(
    () => Aike.spawnSync("cmd", "str"), "TypeError") !== null);
  check("non-string arg element throws TypeError", throws(
    () => Aike.spawnSync("cmd", [1]), "TypeError") !== null);
  check("options:'str' throws TypeError", throws(
    () => Aike.spawnSync("cmd", [], "str"), "TypeError") !== null);

  // async also strict
  check("async timeout:'abc' throws TypeError", throws(
    () => Aike.spawn("cmd", [], { timeout: "abc" }), "TypeError") !== null);
}

await testSyncBaseline();
await testSyncTimeout();
await testAsyncTimeout();
await testTreeKill();
await testStrictValidation();

console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
