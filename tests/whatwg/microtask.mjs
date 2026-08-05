// 🌊 WHATWG HTML §8.8 Microtask queuing — queueMicrotask
//
// Import-free: runs in any JS environment with global queueMicrotask.
// Verified against the WHATWG HTML spec contract:
//   1. queueMicrotask is a function; returns undefined.
//   2. Non-callable callback throws a TypeError (Web IDL VoidFunction).
//   3. Callback runs asynchronously, after the current synchronous code.
//   4. Callbacks run in FIFO order on the same microtask queue.
//   5. Microtasks run before timers, and before control returns to the loop.
//   6. A microtask queued from inside a microtask runs before timers too.
//   7. Callback is invoked with no arguments and `this` = undefined.
//   8. A throwing callback is reported but does not stop later microtasks.

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

// Give the event loop one turn so pending microtasks drain.
function flush() {
  return new Promise((r) => setTimeout(r, 0));
}

// ===================================================================
// 1. queueMicrotask exists and returns undefined
// ===================================================================

async function testShape() {
  console.log("\n=== queueMicrotask — shape ===");

  check("is a function", typeof queueMicrotask === "function", `got ${typeof queueMicrotask}`);
  check("length is 1", queueMicrotask.length === 1, `got ${queueMicrotask.length}`);

  const ret = queueMicrotask(() => {});
  check("returns undefined", ret === undefined, `got ${String(ret)}`);
  await flush();
}

// ===================================================================
// 2. Non-callable callback throws TypeError
// ===================================================================

async function testTypeValidation() {
  console.log("\n=== queueMicrotask — type validation ===");

  const badArgs = [undefined, null, 42, "str", true, {}, [], Symbol("s")];
  for (const v of badArgs) {
    let threw = false;
    let isTypeError = false;
    try {
      queueMicrotask(v);
    } catch (e) {
      threw = true;
      isTypeError = e instanceof TypeError;
    }
    check(
      `non-callable ${typeof v} throws TypeError`,
      threw && isTypeError,
      threw ? `threw ${isTypeError ? "TypeError" : "other"}` : "did not throw",
    );
  }

  // A callable must be accepted.
  let accepted = true;
  try {
    queueMicrotask(() => {});
  } catch {
    accepted = false;
  }
  check("callable accepted", accepted);
  await flush();
}

// ===================================================================
// 3. Callback runs asynchronously, after current synchronous code
// ===================================================================

async function testAsync() {
  console.log("\n=== queueMicrotask — async execution ===");

  const order = [];
  queueMicrotask(() => order.push("micro"));
  order.push("sync");
  check("not run synchronously", order.length === 1, `ran ${order.length} times`);
  await flush();
  check("runs after sync code", order.join(",") === "sync,micro", `got ${order.join(",")}`);
}

// ===================================================================
// 4. FIFO ordering of multiple microtasks
// ===================================================================

async function testFifo() {
  console.log("\n=== queueMicrotask — FIFO order ===");

  const order = [];
  queueMicrotask(() => order.push("a"));
  queueMicrotask(() => order.push("b"));
  queueMicrotask(() => order.push("c"));
  await flush();
  check("FIFO order preserved", order.join(",") === "a,b,c", `got ${order.join(",")}`);
}

// ===================================================================
// 5. Microtasks run before timers (setTimeout)
// ===================================================================

async function testBeforeTimers() {
  console.log("\n=== queueMicrotask — before timers ===");

  const order = [];
  queueMicrotask(() => order.push("micro"));
  setTimeout(() => order.push("timer"), 0);
  await new Promise((r) => setTimeout(r, 20));
  check("microtask before timer", order.join(",") === "micro,timer", `got ${order.join(",")}`);
}

// ===================================================================
// 6. Microtask nested from a microtask still runs before timers
// ===================================================================

async function testNested() {
  console.log("\n=== queueMicrotask — nested microtask ===");

  const order = [];
  queueMicrotask(() => {
    order.push("outer");
    queueMicrotask(() => order.push("inner"));
  });
  setTimeout(() => order.push("timer"), 0);
  await new Promise((r) => setTimeout(r, 20));
  check(
    "inner runs within same checkpoint before timer",
    order.join(",") === "outer,inner,timer",
    `got ${order.join(",")}`,
  );
}

// ===================================================================
// 7. Callback invoked with no args and `this` undefined
// ===================================================================

async function testInvocation() {
  console.log("\n=== queueMicrotask — invocation ===");

  let args;
  let thisVal;
  queueMicrotask(function (...a) {
    args = a;
    thisVal = this;
  });
  await flush();
  check("no arguments passed", Array.isArray(args) && args.length === 0, `got ${args?.length}`);
  check("this is undefined (strict)", thisVal === undefined, `got ${String(thisVal)}`);
}

// ===================================================================
// 8. Throwing callback is reported but doesn't stop later microtasks
// ===================================================================

async function testException() {
  console.log("\n=== queueMicrotask — callback exception ===");

  let later = false;
  queueMicrotask(() => {
    throw new Error("boom");
  });
  queueMicrotask(() => {
    later = true;
  });
  await flush();
  check("later microtask still ran", later, "");
}

// ===================================================================
// 9. Interleaves with promise reactions in queueing order
// ===================================================================

async function testPromiseInterleave() {
  console.log("\n=== queueMicrotask — promise interleaving ===");

  const order = [];
  queueMicrotask(() => order.push("micro"));
  Promise.resolve().then(() => order.push("promise"));
  await flush();
  check(
    "same queue, FIFO order",
    order.join(",") === "micro,promise",
    `got ${order.join(",")}`,
  );
}

// ===================================================================
// Runner
// ===================================================================

await testShape();
await testTypeValidation();
await testAsync();
await testFifo();
await testBeforeTimers();
await testNested();
await testInvocation();
await testException();
await testPromiseInterleave();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
