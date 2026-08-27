/**
 * sandbox_shutdown_test.mjs — Verify sandbox subprocess shutdown/kill parity
 * with non-sandbox subprocess.
 *
 * Tests:
 *   - kill() sends configurable signal (default SIGKILL, custom SIGTERM)
 *   - shutdown() runs EOF → SIGTERM → SIGKILL ladder
 *   - shutdown() idempotency ("already_closed")
 *   - isRunning() reflects process state
 *
 * Usage:  aike -m tests/spawn/sandbox_shutdown_test.mjs
 */

const dec = new TextDecoder();
function strip(s) {
  return s.replace(/\u001b\[[0-9;]*m/g, "").trim();
}
function stdout(r) {
  return strip(dec.decode(r.stdout));
}

let passed = 0;
let failed = 0;

function check(label, cond, detail = "") {
  if (cond) {
    passed++;
    console.log(`  \x1b[32m✓\x1b[0m ${label}`);
  } else {
    failed++;
    console.log(`  \x1b[31m✗\x1b[0m ${label}${detail ? " — " + detail : ""}`);
  }
}

const fs = await import("fs");
const base = `${process.env.HOME}/.aike-sandbox-shutdown-${process.pid}`;
const ws = `${base}/ws`;

async function setup() {
  await fs.makeDir(ws, { recursive: true });
}

async function cleanup() {
  await fs.remove(base).catch(() => {});
}

function mkSandbox(overrides = {}) {
  return {
    cwd: ws,
    mounts: { [ws]: { writable: true } },
    network: "none",
    ...overrides,
  };
}

// ===================================================================
// 1. kill() — default SIGKILL
// ===================================================================
async function testKillDefault() {
  console.log("\n=== kill() default SIGKILL ===");

  const proc = Aike.spawn("sleep", ["60"], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  // Wait briefly for process to start, then kill.
  await new Promise((r) => setTimeout(r, 200));
  check("isRunning before kill", proc.isRunning());

  const killed = proc.kill();
  check("kill() returns true", killed);

  const code = await proc.exitCode;
  const sig = await proc.signalCode;
  check("exitCode !== 0", code !== 0);
  check("signalCode is SIGKILL", sig === "SIGKILL", `got ${sig}`);
}

// ===================================================================
// 2. kill("SIGTERM") — custom signal
// ===================================================================
async function testKillSignal() {
  console.log("\n=== kill(\"SIGTERM\") ===");

  // Trap SIGTERM to exit cleanly with code 0.
  const proc = Aike.spawn("sh", ["-c",
    "trap 'exit 0' TERM; sleep 60"
  ], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  await new Promise((r) => setTimeout(r, 200));
  const killed = proc.kill("SIGTERM");
  check("kill(\"SIGTERM\") returns true", killed);

  const code = await proc.exitCode;
  const sig = await proc.signalCode;
  // On macOS, sandbox-exec doesn't forward signals to the inner command,
  // so the child exits cleanly (code=0) without being killed by a signal.
  if (process.platform === "darwin") {
    check("exitCode === 0 (sandbox-exec exits cleanly on macOS)", code === 0, `got ${code}`);
    check("signalCode null on macOS (signal not forwarded)", sig === null, `got ${sig}`);
  } else {
    check("exitCode === 0 (clean exit)", code === 0, `got ${code}`);
    check("signalCode is SIGTERM", sig === "SIGTERM", `got ${sig}`);
  }
}

// ===================================================================
// 3. shutdown() — "term" stage (child exits on SIGTERM)
// ===================================================================
async function testShutdownTerm() {
  console.log("\n=== shutdown() term stage ===");

  const proc = Aike.spawn("sh", ["-c",
    "trap 'exit 42' TERM; sleep 60"
  ], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  await new Promise((r) => setTimeout(r, 200));
  const stage = await proc.shutdown({
    eofTimeoutMs: 500,
    termTimeoutMs: 2000,
    killTimeoutMs: 1000,
  });

  // On macOS, sandbox-exec doesn't forward SIGTERM, so the ladder always
  // escalates to "kill" stage. On Linux, the child receives SIGTERM.
  if (process.platform === "darwin") {
    check("shutdown stage is \"kill\" on macOS (SIGTERM not forwarded)",
      stage === "kill", `got ${stage}`);
  } else {
    check("shutdown stage is \"term\"", stage === "term", `got ${stage}`);
    const code = await proc.exitCode;
    check("exitCode is 42 (from trap)", code === 42, `got ${code}`);
  }
}

// ===================================================================
// 4. shutdown() — "kill" stage (child ignores SIGTERM)
// ===================================================================
async function testShutdownKill() {
  console.log("\n=== shutdown() kill stage ===");

  const proc = Aike.spawn("sh", ["-c",
    "trap '' TERM; sleep 60"  // ignore SIGTERM
  ], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  await new Promise((r) => setTimeout(r, 200));
  const t0 = Date.now();
  const stage = await proc.shutdown({
    eofTimeoutMs: 200,
    termTimeoutMs: 500,
    killTimeoutMs: 2000,
  });
  const elapsed = Date.now() - t0;

  check("shutdown stage is \"kill\"", stage === "kill", `got ${stage}`);
  check("completed within bounds", elapsed < 4000, `elapsed=${elapsed}ms`);
}

// ===================================================================
// 5. shutdown() — "eof" stage (child exits on stdin close)
// ===================================================================
async function testShutdownEof() {
  console.log("\n=== shutdown() eof stage ===");

  const proc = Aike.spawn("sh", ["-c", "cat"], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  await new Promise((r) => setTimeout(r, 200));
  const stage = await proc.shutdown({
    eofTimeoutMs: 2000,
    termTimeoutMs: 1000,
    killTimeoutMs: 1000,
  });

  check("shutdown stage is \"eof\"", stage === "eof", `got ${stage}`);
  const code = await proc.exitCode;
  check("exitCode === 0", code === 0, `got ${code}`);
}

// ===================================================================
// 6. shutdown() — idempotent
// ===================================================================
async function testShutdownIdempotent() {
  console.log("\n=== shutdown() idempotent ===");

  const proc = Aike.spawn("sleep", ["60"], {
    sandbox: mkSandbox(),
    timeout: 10000,
  });

  await new Promise((r) => setTimeout(r, 200));
  // First call runs the ladder.
  const p1 = proc.shutdown({ eofTimeoutMs: 100, termTimeoutMs: 100, killTimeoutMs: 500 });
  // Second call should return immediately.
  const p2 = proc.shutdown();

  const [s1, s2] = await Promise.all([p1, p2]);
  check("first shutdown completes", s1 === "kill" || s1 === "term" || s1 === "eof",
    `got ${s1}`);
  check("second shutdown is \"already_closed\"", s2 === "already_closed", `got ${s2}`);
}

// ===================================================================
// Run all
// ===================================================================
await setup();
try {
  await testKillDefault();
  await testKillSignal();
  await testShutdownTerm();
  await testShutdownKill();
  await testShutdownEof();
  await testShutdownIdempotent();
} finally {
  await cleanup();
}

console.log(`\n\x1b[1m${passed} passed, ${failed} failed\x1b[0m\n`);
if (failed > 0) process.exit(1);
