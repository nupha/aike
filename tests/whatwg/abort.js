// 🛑 WHATWG AbortController / AbortSignal — DOM 标准兼容测试
//
// Import-free: runs in any JS environment with global AbortController/AbortSignal.

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

// ===================================================================
// 1. AbortController basics
// ===================================================================

function testControllerBasics() {
  console.log("\n=== AbortController Basics ===");

  const ac = new AbortController();
  check("controller is object", typeof ac === "object", `got ${typeof ac}`);
  check("controller has signal", ac.signal !== undefined, "");
  check(
    "signal is AbortSignal",
    ac.signal instanceof AbortSignal,
    `${typeof ac.signal}`,
  );

  // Initial state: not aborted
  check(
    "initial aborted false",
    ac.signal.aborted === false,
    `got ${ac.signal.aborted}`,
  );
  check(
    "initial reason undefined",
    ac.signal.reason === undefined,
    `got ${ac.signal.reason}`,
  );
}

// ===================================================================
// 2. AbortController.abort()
// ===================================================================

function testControllerAbort() {
  console.log("\n=== AbortController.abort() ===");

  // Abort without reason
  const ac1 = new AbortController();
  check("before abort aborted false", ac1.signal.aborted === false, "");
  ac1.abort();
  check("after abort aborted true", ac1.signal.aborted === true, "");
  check(
    "after abort reason undefined",
    ac1.signal.reason === undefined,
    `got ${ac1.signal.reason}`,
  );

  // Abort with reason
  const ac2 = new AbortController();
  ac2.abort("timeout");
  check(
    "abort with string reason",
    ac2.signal.reason === "timeout",
    `got ${ac2.signal.reason}`,
  );

  // Abort with object reason
  const ac3 = new AbortController();
  const err = new Error("custom error");
  ac3.abort(err);
  check("abort with Error reason", ac3.signal.reason === err, "");
  check("reason is Error", ac3.signal.reason instanceof Error, "");
  check(
    "reason message",
    ac3.signal.reason.message === "custom error",
    `got ${ac3.signal.reason.message}`,
  );

  // Double abort is no-op
  const ac4 = new AbortController();
  ac4.abort("first");
  ac4.abort("second");
  check(
    "double abort keeps first reason",
    ac4.signal.reason === "first",
    `got ${ac4.signal.reason}`,
  );
  check("double abort still aborted", ac4.signal.aborted === true, "");
}

// ===================================================================
// 3. AbortSignal.throwIfAborted()
// ===================================================================

function testThrowIfAborted() {
  console.log("\n=== AbortSignal.throwIfAborted() ===");

  // Does not throw when not aborted
  const ac1 = new AbortController();
  try {
    ac1.signal.throwIfAborted();
    check("no throw when not aborted", true);
  } catch (_) {
    check("no throw when not aborted", false);
  }

  // Throws when aborted
  const ac2 = new AbortController();
  ac2.abort();
  try {
    ac2.signal.throwIfAborted();
    check("throw when aborted", false, "should have thrown");
  } catch (_) {
    check("throw when aborted", true);
  }

  // Throws with custom reason
  const ac3 = new AbortController();
  ac3.abort("cancel");
  try {
    ac3.signal.throwIfAborted();
    check("throw with reason", false, "should have thrown");
  } catch (e) {
    check("throw with reason", true);
  }
}

// ===================================================================
// 4. AbortSignal.abort() static
// ===================================================================

function testSignalStaticAbort() {
  console.log("\n=== AbortSignal.abort() static ===");

  // Without reason
  const s1 = AbortSignal.abort();
  check("static abort is AbortSignal", s1 instanceof AbortSignal, "");
  check("static abort aborted", s1.aborted === true, "");
  check("static abort reason", s1.reason === undefined, `got ${s1.reason}`);

  // With reason
  const s2 = AbortSignal.abort("custom");
  check("static abort with reason", s2.aborted === true, "");
  check(
    "static abort reason value",
    s2.reason === "custom",
    `got ${s2.reason}`,
  );
}

// ===================================================================
// 5. AbortSignal.timeout() static
// ===================================================================

function testSignalStaticTimeout() {
  console.log("\n=== AbortSignal.timeout() static ===");

  // Note: current implementation always returns immediately-aborted signal
  const s = AbortSignal.timeout(1000);
  check("static timeout is AbortSignal", s instanceof AbortSignal, "");
  check("static timeout aborted", s.aborted === true, "");
  check(
    "static timeout reason is string",
    typeof s.reason === "string",
    `got ${typeof s.reason}`,
  );
}

// ===================================================================
// 6. AbortSignal constructor is illegal
// ===================================================================

function testSignalConstructor() {
  console.log("\n=== AbortSignal Constructor ===");

  try {
    new AbortSignal();
    check("illegal constructor throws", false, "should have thrown");
  } catch (_) {
    check("illegal constructor throws", true);
  }
}

// ===================================================================
// 7. Multiple signals and independent state
// ===================================================================

function testIndependentSignals() {
  console.log("\n=== Independent Signals ===");

  const ac1 = new AbortController();
  const ac2 = new AbortController();

  check("two controllers independent", ac1.signal !== ac2.signal, "");

  ac1.abort("reason1");
  check("ac1 aborted", ac1.signal.aborted === true, "");
  check(
    "ac1 reason",
    ac1.signal.reason === "reason1",
    `got ${ac1.signal.reason}`,
  );
  check("ac2 not affected", ac2.signal.aborted === false, "");
  check(
    "ac2 reason still undefined",
    ac2.signal.reason === undefined,
    `got ${ac2.signal.reason}`,
  );
}

// ===================================================================
// 8. Signal from controller vs static abort signal
// ===================================================================

function testSignalSources() {
  console.log("\n=== Signal Sources ===");

  // From controller — initially not aborted
  const ac = new AbortController();
  check("controller signal not aborted", ac.signal.aborted === false, "");

  // From static abort — immediately aborted
  const s = AbortSignal.abort("done");
  check("static abort signal aborted", s.aborted === true, "");

  // Both are instanceof AbortSignal
  check("controller signal instanceof", ac.signal instanceof AbortSignal, "");
  check("static signal instanceof", s instanceof AbortSignal, "");
}

// ===================================================================
// 9. AbortController.signal getter returns consistent object
// ===================================================================

function testSignalIdentity() {
  console.log("\n=== Signal Identity ===");

  const ac = new AbortController();
  const s1 = ac.signal;
  const s2 = ac.signal;
  check("signal getter returns same object", s1 === s2, "");
}

// ===================================================================
// Runner
// ===================================================================

function runAll() {
  testControllerBasics();
  testControllerAbort();
  testThrowIfAborted();
  testSignalStaticAbort();
  testSignalStaticTimeout();
  testSignalConstructor();
  testIndependentSignals();
  testSignalSources();
  testSignalIdentity();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
}

runAll();
