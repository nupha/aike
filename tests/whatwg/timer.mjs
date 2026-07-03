// ⏱️ WHATWG Timers — setTimeout / setInterval / clearTimeout / clearInterval
//
// Import-free: runs in any JS environment with global timer functions.

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
// 1. setTimeout — basic fire
// ===================================================================

async function testSetTimeoutFire() {
  console.log("\n=== setTimeout — basic fire ===");

  let fired = false;
  const id = setTimeout(() => {
    fired = true;
  }, 10);

  check("returns number", typeof id === "number", `got ${typeof id}`);
  check("positive id", id > 0, `got ${id}`);

  await new Promise((r) => setTimeout(r, 50));
  check("callback fired", fired, "");
}

// ===================================================================
// 2. setTimeout — clearTimeout prevents fire
// ===================================================================

async function testClearTimeout() {
  console.log("\n=== setTimeout — clearTimeout ===");

  let fired = false;
  const id = setTimeout(() => {
    fired = true;
  }, 20);

  clearTimeout(id);
  await new Promise((r) => setTimeout(r, 60));
  check("callback blocked after clear", !fired, "should NOT have fired");
}

// ===================================================================
// 3. setTimeout — 0 delay fires asynchronously
// ===================================================================

async function testZeroDelay() {
  console.log("\n=== setTimeout — zero delay ===");

  let fired = false;
  setTimeout(() => {
    fired = true;
  }, 0);

  // Should NOT fire synchronously
  check("not fired synchronously", !fired, "");

  await new Promise((r) => setTimeout(r, 20));
  check("fired after yield", fired, "");
}

// ===================================================================
// 4. setTimeout — multiple independent timers
// ===================================================================

async function testMultipleTimers() {
  console.log("\n=== setTimeout — multiple timers ===");

  const order = [];
  setTimeout(() => order.push("a"), 30);
  setTimeout(() => order.push("b"), 10);
  setTimeout(() => order.push("c"), 20);

  await new Promise((r) => setTimeout(r, 60));
  check("all fired", order.length === 3, `got ${order.length}`);
  check(
    "order respects delay",
    order[0] === "b",
    `got ${JSON.stringify(order)}`,
  );
  check(
    "order respects delay",
    order[1] === "c",
    `got ${JSON.stringify(order)}`,
  );
  check(
    "order respects delay",
    order[2] === "a",
    `got ${JSON.stringify(order)}`,
  );
}

// ===================================================================
// 5. setTimeout — returns unique IDs
// ===================================================================

async function testUniqueIds() {
  console.log("\n=== setTimeout — unique IDs ===");

  const id1 = setTimeout(() => {}, 50);
  const id2 = setTimeout(() => {}, 50);
  const id3 = setTimeout(() => {}, 50);

  check("IDs are distinct", id1 !== id2 && id2 !== id3 && id1 !== id3, "");

  clearTimeout(id1);
  clearTimeout(id2);
  clearTimeout(id3);
}

// ===================================================================
// 6. setInterval — basic repeat
// ===================================================================

async function testSetIntervalRepeat() {
  console.log("\n=== setInterval — basic repeat ===");

  let count = 0;
  const id = setInterval(() => {
    count++;
  }, 20);

  check("returns number", typeof id === "number", `got ${typeof id}`);
  check("positive id", id > 0, `got ${id}`);

  await new Promise((r) => setTimeout(r, 75));
  clearInterval(id);

  check("fired at least twice", count >= 2, `count=${count}`);

  // Extra wait: no more fires after clear
  const frozen = count;
  await new Promise((r) => setTimeout(r, 60));
  check(
    "no fire after clear",
    count === frozen,
    `count went ${frozen}→${count}`,
  );
}

// ===================================================================
// 7. setInterval — clearInterval stops repeat
// ===================================================================

async function testClearInterval() {
  console.log("\n=== setInterval — clearInterval ===");

  let count = 0;
  const id = setInterval(() => {
    count++;
  }, 15);

  await new Promise((r) => setTimeout(r, 40));
  clearInterval(id);

  const frozen = count;
  await new Promise((r) => setTimeout(r, 50));
  check(
    "no repeat after clearInterval",
    count === frozen,
    `count went ${frozen}→${count}`,
  );
}

// ===================================================================
// 8. clearTimeout — no-arg / invalid arg is safe
// ===================================================================

async function testClearEdgeCases() {
  console.log("\n=== clearTimeout / clearInterval — edge cases ===");

  let ok = true;
  try {
    clearTimeout();
    clearTimeout(undefined);
    clearTimeout(null);
    clearTimeout(0);
    clearTimeout(-1);
    clearInterval();
    clearInterval(undefined);
    clearInterval(0);
    clearInterval(-1);
  } catch (e) {
    ok = false;
  }
  check("no crash on invalid/empty args", ok);

  // Also no crash on unknown (non-existent) timer ID
  try {
    clearTimeout(999999);
    clearInterval(999999);
  } catch (e) {
    ok = false;
  }
  check("no crash on unknown timer ID", ok);
}

// ===================================================================
// 9. setTimeout — callback exception does not crash
// ===================================================================

async function testCallbackError() {
  console.log("\n=== setTimeout — callback error ===");

  // Should not crash the runtime
  setTimeout(() => {
    throw new Error("intentional error");
  }, 10);

  // A subsequent timer should still work
  let fired = false;
  setTimeout(() => {
    fired = true;
  }, 30);

  await new Promise((r) => setTimeout(r, 60));
  check("subsequent timer still fires after error", fired);
}

// ===================================================================
// 10. setInterval — returns distinct IDs from setTimeout
// ===================================================================

async function testIdDistinctFromSetTimeout() {
  console.log("\n=== setInterval — unique across setTimeout ===");

  const stId = setTimeout(() => {}, 50);
  const siId = setInterval(() => {}, 100);
  const stId2 = setTimeout(() => {}, 50);

  check("setTimeout vs setInterval IDs distinct", stId !== siId, "");
  check("setInterval vs setTimeout IDs distinct", siId !== stId2, "");

  clearTimeout(stId);
  clearInterval(siId);
  clearTimeout(stId2);
}

// ===================================================================
// Runner
// ===================================================================

async function runAll() {
  await testSetTimeoutFire();
  await testClearTimeout();
  await testZeroDelay();
  await testMultipleTimers();
  await testUniqueIds();
  await testSetIntervalRepeat();
  await testClearInterval();
  await testClearEdgeCases();
  await testCallbackError();
  await testIdDistinctFromSetTimeout();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
}

await runAll();
