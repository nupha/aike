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

await testSyncBaseline();
await testSyncTimeout();
await testAsyncTimeout();
await testTreeKill();

console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
