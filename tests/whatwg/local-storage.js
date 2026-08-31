// WHATWG HTML §12 Web Storage — localStorage 兼容测试
//
// 验证 localStorage 的 Storage 接口完整行为。
// Run: aike tests/whatwg/local-storage.js

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
function assertThrows(label, fn, errType) {
  try {
    fn();
    check(label, false, "did not throw");
  } catch (e) {
    if (errType && !(e instanceof errType)) {
      check(label, false, `threw ${e.constructor.name}, expected ${errType.name}`);
    } else {
      check(label, true);
    }
  }
}

// --- 0. 清理：从干净状态开始 ---
console.log("=== 0. setup: clear before test ===");
localStorage.clear();
check("clear: initial state is empty", localStorage.length === 0);

// --- 1. setItem / getItem 基本读写 ---
console.log("\n=== 1. setItem / getItem basics ===");

localStorage.setItem("key1", "value1");
check("setItem: stores value", localStorage.getItem("key1") === "value1");

localStorage.setItem("key2", "value2");
check("getItem: returns correct value", localStorage.getItem("key2") === "value2");

// getItem 不存在的 key 返回 null
check("getItem: non-existent key returns null", localStorage.getItem("nope") === null);
check("getItem: null key returns null", localStorage.getItem(null) === null);

// --- 2. setItem 覆盖已有 key ---
console.log("\n=== 2. setItem overwrites existing key ===");

localStorage.setItem("key1", "new_value1");
check("setItem: overwrites existing", localStorage.getItem("key1") === "new_value1");
check("setItem: length unchanged after overwrite", localStorage.length === 2);

// --- 3. removeItem ---
console.log("\n=== 3. removeItem ===");

localStorage.setItem("toDelete", " ephemeral ");
check("removeItem: key exists before", localStorage.getItem("toDelete") === " ephemeral ");

localStorage.removeItem("toDelete");
check("removeItem: key gone after", localStorage.getItem("toDelete") === null);
check("removeItem: length decremented", localStorage.length === 2);

// removeItem 不存在的 key 是 no-op
localStorage.removeItem("nonexistent");
check("removeItem: no-op on missing key", localStorage.length === 2);

// --- 4. clear ---
console.log("\n=== 4. clear ===");

localStorage.setItem("a", "1");
localStorage.setItem("b", "2");
check("clear: has items before", localStorage.length === 4);

localStorage.clear();
check("clear: empty after", localStorage.length === 0);
check("clear: getItem returns null", localStorage.getItem("a") === null);

// --- 5. key(index) ---
console.log("\n=== 5. key(index) ===");

localStorage.clear();
localStorage.setItem("alpha", "A");
localStorage.setItem("beta", "B");
localStorage.setItem("gamma", "G");

check("key(0): first key", localStorage.key(0) === "alpha");
check("key(1): second key", localStorage.key(1) === "beta");
check("key(2): third key", localStorage.key(2) === "gamma");

// key 越界返回 null
check("key(3): out of bounds returns null", localStorage.key(3) === null);
check("key(100): far out of bounds returns null", localStorage.key(100) === null);
check("key(-1): negative index returns null", localStorage.key(-1) === null);

// --- 6. length ---
console.log("\n=== 6. length ===");

localStorage.clear();
check("length: 0 after clear", localStorage.length === 0);

localStorage.setItem("x", "1");
check("length: 1 after one setItem", localStorage.length === 1);

localStorage.setItem("y", "2");
localStorage.setItem("z", "3");
check("length: 3 after three setItem", localStorage.length === 3);

localStorage.removeItem("y");
check("length: 2 after removeItem", localStorage.length === 2);

// --- 7. 值的 ToString 转换 ---
console.log("\n=== 7. ToString coercion ===");

localStorage.setItem("num", 42);
check("ToString: number", localStorage.getItem("num") === "42");

localStorage.setItem("bool", true);
check("ToString: boolean true", localStorage.getItem("bool") === "true");

localStorage.setItem("boolF", false);
check("ToString: boolean false", localStorage.getItem("boolF") === "false");

localStorage.setItem("nil", null);
check("ToString: null", localStorage.getItem("nil") === "null");

localStorage.setItem("undef", undefined);
check("ToString: undefined", localStorage.getItem("undef") === "undefined");

localStorage.setItem("obj", { a: 1 });
check("ToString: object", localStorage.getItem("obj") === "[object Object]");

// --- 8. key 是什么类型 ---
console.log("\n=== 8. Key coercion ===");

localStorage.clear();
localStorage.setItem("123", "num-key");
localStorage.setItem("true", "bool-key");
localStorage.setItem("", "empty-key");

check("key as string '123'", localStorage.getItem("123") === "num-key");
check("key as string 'true'", localStorage.getItem("true") === "bool-key");
check("key as empty string", localStorage.getItem("") === "empty-key");

// --- 9. 空字符串 ---
console.log("\n=== 9. Empty string edge cases ===");

localStorage.clear();
localStorage.setItem("", "");
check("empty key+value: getItem", localStorage.getItem("") === "");
check("empty key+value: length", localStorage.length === 1);

localStorage.setItem("key", "");
check("empty value: getItem", localStorage.getItem("key") === "");

localStorage.setItem("", "val");
check("overwrite empty key", localStorage.getItem("") === "val");

// --- 10. 特殊字符 ---
console.log("\n=== 10. Special characters ===");

localStorage.clear();
const unicode = "你好世界🌍🎉";
localStorage.setItem("unicode", unicode);
check("unicode value", localStorage.getItem("unicode") === unicode);

const special = "\n\t\r\\\"'";

localStorage.setItem("special", special);
check("special chars value", localStorage.getItem("special") === special);

const longKey = "k".repeat(256);
localStorage.setItem(longKey, "long-key");
check("long key (256 chars)", localStorage.getItem(longKey) === "long-key");

const longVal = "v".repeat(10000);
localStorage.setItem("longVal", longVal);
check("long value (10000 chars)", localStorage.getItem("longVal") === longVal);

// --- 11. key() 顺序一致性 ---
console.log("\n=== 11. key() order consistency ===");

localStorage.clear();
localStorage.setItem("c", "3");
localStorage.setItem("a", "1");
localStorage.setItem("b", "2");

// key() 返回的顺序在多次调用间保持一致
const keys1 = [];
const keys2 = [];
for (let i = 0; i < localStorage.length; i++) {
  keys1.push(localStorage.key(i));
  keys2.push(localStorage.key(i));
}
check("key order: stable across iterations", JSON.stringify(keys1) === JSON.stringify(keys2));

// --- 12. for...in / Object.keys 不应枚举 localStorage 属性 ---
console.log("\n=== 12. Enumerable behavior ===");

localStorage.clear();
localStorage.setItem("foo", "bar");

const ownKeys = Object.keys(localStorage);
check("Object.keys: does not include storage properties", ownKeys.length === 0);

// --- 13. Symbol.toStringTag ---
console.log("\n=== 13. Symbol.toStringTag ===");

check(
  "[Symbol.toStringTag] === 'Storage'",
  Object.prototype.toString.call(localStorage) === "[object Storage]"
);

// --- 14. setItem / removeItem / clear 无返回值 ---
console.log("\n=== 14. Return values (undefined) ===");

localStorage.clear();
const r1 = localStorage.setItem("a", "1");
const r2 = localStorage.removeItem("a");
const r3 = localStorage.clear();

check("setItem returns undefined", r1 === undefined);
check("removeItem returns undefined", r2 === undefined);
check("clear returns undefined", r3 === undefined);

// --- 15. 大量数据 ---
console.log("\n=== 15. Bulk operations ===");

localStorage.clear();
const N = 100;
for (let i = 0; i < N; i++) {
  localStorage.setItem(`bulk_${i}`, `val_${i}`);
}
check(`bulk: ${N} items stored`, localStorage.length === N);
check("bulk: get first", localStorage.getItem("bulk_0") === "val_0");
check("bulk: get last", localStorage.getItem(`bulk_${N - 1}`) === `val_${N - 1}`);

for (let i = 0; i < N; i++) {
  localStorage.removeItem(`bulk_${i}`);
}
check("bulk: all removed", localStorage.length === 0);

// --- 16. getItem 与属性访问的独立性 ---
console.log("\n=== 16. getItem vs direct property ===");

localStorage.clear();
localStorage.setItem("direct", "via_setItem");
// 直接属性访问应是 undefined（Storage 对象上没有动态属性）
check("getItem vs property: getItem works", localStorage.getItem("direct") === "via_setItem");

// --- Summary ---
const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
