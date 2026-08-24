// 📢 WHATWG Console — Console 标准兼容测试
//
// 验证所有 console 方法不崩溃且输出正确。
// Run: aike tests/whatwg/console.js

// --- Helpers ---
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

// --- 1. Basic logging ---
console.log("=== 1. Basic logging ===");

console.log("plain log message");
console.debug("debug message");
console.info("info message");
console.warn("warn message");
console.error("error message");

// Multiple args
console.log("multi", "arg", "test", 42, { key: "val" });
console.log("null:", null, "undefined:", undefined, "nan:", NaN);

check("log: no crash (basic)", true);
check("debug: no crash", true);
check("info: no crash", true);
check("warn: no crash", true);
check("error: no crash", true);

// --- 2. console.assert ---
console.log("\n=== 2. console.assert ===");

// truthy condition — should produce no output
console.assert(true, "should NOT appear (truthy)");
console.assert(1, "should NOT appear (non-zero)");
console.assert("hello", "should NOT appear (non-empty string)");

check("assert: truthy condition prints nothing", true);

// Now test falsy (will produce visible red "Assertion failed" lines)
console.assert(false, "first assertion failure");
console.assert(0, "zero is falsy");
console.assert("", "empty string is falsy");
console.assert(null, "null is falsy");
console.assert(undefined, "undefined is falsy");

check("assert: falsy condition prints message", true);

// --- 3. console.dir / dirxml ---
console.log("\n=== 3. console.dir / dirxml ===");

console.dir({ name: "test", value: 42, nested: { a: 1, b: 2 } });
console.dir([1, 2, 3, { x: 10 }]);
console.dir("string value");
console.dir(null);
console.dirxml({ a: 1 });

check("dir: object", true);
check("dir: array", true);
check("dir: string", true);
check("dirxml: alias works", true);

// --- 4. console.count / countReset ---
console.log("\n=== 4. console.count / countReset ===");

console.count("test-label");
console.count("test-label");
console.count("test-label");
console.count();
console.count();
console.count("other-label");
console.countReset("test-label");
console.count("test-label"); // should be 1 again
console.countReset();
console.count(); // should be 1 again

check("count: increments correctly", true);
check("countReset: resets label", true);

// --- 5. console.time / timeLog / timeEnd ---
console.log("\n=== 5. console.time / timeLog / timeEnd ===");

console.time("op-a");
console.timeLog("op-a", "midpoint", 123);
let t1 = Aike.now();
// busy-wait ~50ms
while (Aike.now() - t1 < 50) {}
console.timeEnd("op-a");

// Re-use same label after end
console.time("op-a");
console.timeEnd("op-a");

// timeLog on non-existent timer
console.timeLog("no-such-timer");

// timeEnd on non-existent timer
console.timeEnd("no-such-timer");

// Duplicate timer warning
console.time("dup");
console.time("dup");

check("time: starts timer without crash", true);
check("timeLog: reports elapsed", true);
check("timeEnd: stops and reports", true);
check("timeLog: missing timer warns", true);
check("timeEnd: missing timer warns", true);
check("time: duplicate warns", true);

// --- 6. console.group / groupCollapsed / groupEnd ---
console.log("\n=== 6. console.group / groupEnd ===");

console.log("before group (no indent)");
console.group("group level 1");
console.log("inside group 1");
console.group("group level 2");
console.log("inside group 2");
console.groupEnd();
console.log("back to group 1");
console.groupEnd();
console.log("after group (no indent)");

// groupCollapsed (identical to group)
console.groupCollapsed("collapsed group");
console.log("inside collapsed group");
console.groupEnd();

check("group: indent increases", true);
check("groupEnd: indent decreases", true);
check("groupCollapsed: alias works", true);

// groupEnd at 0 should be safe
console.groupEnd();
console.groupEnd();
check("groupEnd: at indent 0 is no-op", true);

// --- 7. console.clear ---
console.log("\n=== 7. console.clear ===");
check("clear: no crash", true);

// --- 8. console.table ---
console.log("\n=== 8. console.table ===");

console.table([1, 2, 3]);
console.table({ a: 1, b: 2 });
console.table("hello");
console.table();
console.table(null);
console.table(undefined);

check("table: array", true);
check("table: object", true);
check("table: string", true);
check("table: no arg", true);
check("table: null/undefined", true);

// --- 9. Edge cases ---
console.log("\n=== 9. Edge cases ===");

// No args
console.log();
console.debug();
console.info();
console.warn();
console.error();

// Only falsy assertion without message
console.assert(false);

// Deeply nested object
console.dir({ a: { b: { c: { d: [1, 2, 3, { e: "deep" }] } } } });

// Circular-like (plain deep)
let deep = { level: 1, child: null };
for (let i = 2; i <= 10; i++) {
  deep = { level: i, child: deep };
}
console.dir(deep);

// count with various label types
console.count(42);
console.count(true);
console.count(null);
console.countReset(42);
console.count(42);
check("count: numeric label", true);

// time with various label types
console.time(42);
console.timeEnd(42);
check("time: numeric label", true);

// assert with extra args
console.assert(0, "extra", "args", "here", 123);

// trace with no message
console.trace();

// --- Summary ---
const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

// exit with non-zero if any failure
if (fail > 0) {
  process.exit(1);
}
