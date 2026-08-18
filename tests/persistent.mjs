// Persistent dump/restore round-trip test for Aike.dumpValue / Aike.restoreValue.
// Covers: primitives, objects/arrays (incl. cyclic), functions/closures,
// generators, native-function behaviour and error paths.
//
// Run with:
//   cargo run -- -m tests/persistent.mjs

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

function assertThrows(desc, fn) {
  try {
    fn();
    check(desc, false, "expected throw, none happened");
  } catch (e) {
    check(desc, true, "threw " + e);
  }
}

// ===================================================================
// Primitives
// ===================================================================
console.log("\n=== Primitives ===");

check("undefined", Aike.restoreValue(Aike.dumpValue(undefined)) === undefined);
check("null", Aike.restoreValue(Aike.dumpValue(null)) === null);
check("bool true", Aike.restoreValue(Aike.dumpValue(true)) === true);
check("bool false", Aike.restoreValue(Aike.dumpValue(false)) === false);
check("int", Aike.restoreValue(Aike.dumpValue(42)) === 42);
check("negative int", Aike.restoreValue(Aike.dumpValue(-7)) === -7);
check("float", Aike.restoreValue(Aike.dumpValue(3.14159)) === 3.14159);
check("NaN", Number.isNaN(Aike.restoreValue(Aike.dumpValue(NaN))));
check("Infinity", Aike.restoreValue(Aike.dumpValue(Infinity)) === Infinity);
check("string ascii", Aike.restoreValue(Aike.dumpValue("hello world")) === "hello world");
check(
  "string utf8",
  Aike.restoreValue(Aike.dumpValue("你好，世界 🌍")) === "你好，世界 🌍",
);
check(
  "bigint",
  Aike.restoreValue(Aike.dumpValue(12345678901234567890123456789n)) ===
    12345678901234567890123456789n,
);
check(
  "symbol",
  Aike.restoreValue(Aike.dumpValue(Symbol.for("k"))) === Symbol.for("k"),
);

// ===================================================================
// Arrays / Objects
// ===================================================================
console.log("\n=== Arrays / Objects ===");

check(
  "flat array",
  JSON.stringify(Aike.restoreValue(Aike.dumpValue([1, 2, 3, "x", true, null]))) ===
    JSON.stringify([1, 2, 3, "x", true, null]),
);
check(
  "nested array",
  JSON.stringify(Aike.restoreValue(Aike.dumpValue([[1, [2, 3]], [4, 5]]))) ===
    JSON.stringify([[1, [2, 3]], [4, 5]]),
);
check(
  "object",
  JSON.stringify(Aike.restoreValue(Aike.dumpValue({ a: 1, b: "two", c: true }))) ===
    JSON.stringify({ a: 1, b: "two", c: true }),
);
check(
  "nested object",
  JSON.stringify(
    Aike.restoreValue(Aike.dumpValue({ x: { y: { z: 42 } }, arr: [1, 2] })),
  ) === JSON.stringify({ x: { y: { z: 42 } }, arr: [1, 2] }),
);
{
  const o = {};
  o.self = o;
  const back = Aike.restoreValue(Aike.dumpValue(o));
  check("cyclic object identity", back.self === back);
}

// ===================================================================
// Functions / Closures
// ===================================================================
console.log("\n=== Functions / Closures ===");

const inc = Aike.restoreValue(
  Aike.dumpValue(function (x) {
    return x + 1;
  }),
);
check("plain function call", inc(41) === 42);

const arrow = Aike.restoreValue(Aike.dumpValue((x) => x * 3));
check("arrow function", arrow(5) === 15);

const def = Aike.restoreValue(
  Aike.dumpValue(
    (function (a, b = 10) {
      return a + b;
    }),
  ),
);
check("default params", def(5) === 15);

const rest = Aike.restoreValue(
  Aike.dumpValue(
    (function (...args) {
      return args.length;
    }),
  ),
);
check("rest params", rest(1, 2, 3) === 3);

{
  const base = 100;
  const g = Aike.restoreValue(
    Aike.dumpValue(
      (function (x) {
        return x + base;
      }),
    ),
  );
  check("closure capture", g(5) === 105);
}

{
  const g = Aike.restoreValue(
    Aike.dumpValue(function (a) {
      return function (b) {
        return a * b;
      };
    }),
  );
  check("nested closure", g(3)(4) === 12);
}

const named = Aike.restoreValue(
  Aike.dumpValue(function myFunc(a, b) {
    return a + b;
  }),
);
check("function name preserved", named.name === "myFunc");
check("function length preserved", named.length === 2);

const gen = Aike.restoreValue(
  Aike.dumpValue(function* () {
    yield 1;
    yield 2;
  }),
);
check("generator yield", gen().next().value === 1);

// ===================================================================
// dumpValue returns Uint8Array
// ===================================================================
console.log("\n=== Return type ===");
check("dumpValue returns Uint8Array", Aike.dumpValue(1) instanceof Uint8Array);

// ===================================================================
// Error paths
// ===================================================================
console.log("\n=== Error paths ===");

assertThrows("restore bad magic", () =>
  Aike.restoreValue(new Uint8Array([1, 2, 3])),
);
assertThrows("restore truncated blob", () =>
  Aike.restoreValue(new Uint8Array([0x5a, 0x42, 0x43, 0x50, 0, 0, 0, 0, 4, 0])),
);
assertThrows("restore non-Uint8Array", () => Aike.restoreValue([1, 2, 3]));

// ===================================================================
// Summary
// ===================================================================
console.log(`\n  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
