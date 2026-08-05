// 🧬 WHATWG HTML §2.7 — structuredClone 全局函数测试
//
// Import-free: runs in any JS environment with the structuredClone global.
// 遵循 structured clone algorithm：深拷贝容器、保留环/重复引用、克隆
// Date/RegExp/Error/ArrayBuffer/TypedArray/Map/Set，transfer 的 ArrayBuffer
// 被转移（源被 detach）。Symbol/函数/Promise/WeakMap/WeakSet → DataCloneError。

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
// 1. API shape + primitives
// ===================================================================

function testShapePrimitives() {
  console.log("\n=== structuredClone — shape & primitives ===");

  check("is a function", typeof structuredClone === "function");
  check("length === 2", structuredClone.length === 2, `got ${structuredClone.length}`);

  check("undefined", structuredClone(undefined) === undefined);
  check("null", structuredClone(null) === null);
  check("boolean", structuredClone(true) === true);
  check("number", structuredClone(42) === 42 && structuredClone(NaN) !== structuredClone(NaN));
  check("string", structuredClone("hello") === "hello");
}

// ===================================================================
// 2. deep clone of plain objects / arrays
// ===================================================================

function testDeepClone() {
  console.log("\n=== structuredClone — deep clone ===");

  const obj = { a: 1, b: { c: "x", d: [1, 2, { e: true }] }, f: null };
  const c = structuredClone(obj);
  check("different root", c !== obj);
  check("different nested object", c.b !== obj.b);
  check("different nested array", c.b.d !== obj.b.d);
  check("different inner object", c.b.d[2] !== obj.b.d[2]);
  check("values copied", c.a === 1 && c.b.c === "x" && c.b.d[2].e === true && c.f === null);

  const arr = [1, "two", [3]];
  const ca = structuredClone(arr);
  check("array different", ca !== arr);
  check("array nested different", ca[2] !== arr[2]);
  check("array values", ca[0] === 1 && ca[1] === "two" && ca[2][0] === 3);

  // getters run (own enumerable props are read via [[Get]])
  const g = { get x() { return 99; } };
  check("getter result cloned", structuredClone(g).x === 99);

  // non-enumerable / inherited props are NOT cloned
  const proto = { inherited: 1 };
  const withHidden = Object.create(proto);
  Object.defineProperty(withHidden, "hidden", { value: 5, enumerable: false });
  withHidden.own = 1;
  const ch = structuredClone(withHidden);
  check("own enumerable cloned", ch.own === 1);
  check("non-enumerable not cloned", !("hidden" in ch));
  check("inherited not cloned", !("inherited" in ch));
}

// ===================================================================
// 3. cycles + duplicate references
// ===================================================================

function testCyclesAndIdentity() {
  console.log("\n=== structuredClone — cycles & identity ===");

  const cyc = {};
  cyc.self = cyc;
  cyc.n = 5;
  const cc = structuredClone(cyc);
  check("self cycle preserved", cc.self === cc && cc.n === 5);

  const shared = { x: 1 };
  const dup = { p: shared, q: shared, arr: [shared] };
  const cd = structuredClone(dup);
  check("duplicate refs share one clone", cd.p === cd.q && cd.q === cd.arr[0]);
  check("clone differs from source", cd.p !== shared);
}

// ===================================================================
// 4. Map / Set
// ===================================================================

function testMapSet() {
  console.log("\n=== structuredClone — Map / Set ===");

  const key = { id: 1 };
  const m = new Map([[key, { v: 10 }], ["s", 20]]);
  const cm = structuredClone(m);
  check("is Map & not same", cm instanceof Map && cm !== m);
  check("size preserved", cm.size === 2);
  check("object key cloned (identity via deep eq)", [...cm.keys()][0].id === 1 && [...cm.keys()][0] !== key);
  check("value cloned", [...cm.values()][0].v === 10 && [...cm.values()][0] !== m.get(key));
  check("string key kept", cm.get("s") === 20);

  const s = new Set([{ z: 9 }, "lit"]);
  const cs = structuredClone(s);
  check("is Set & not same", cs instanceof Set && cs !== s);
  check("size preserved", cs.size === 2);
  const el = [...cs][0];
  check("element cloned", el.z === 9 && el !== [...s][0]);
}

// ===================================================================
// 5. ArrayBuffer / TypedArray / DataView
// ===================================================================

function testBuffers() {
  console.log("\n=== structuredClone — buffers ===");

  const ab = new ArrayBuffer(4);
  new Uint8Array(ab).set([1, 2, 3, 4]);
  const cab = structuredClone(ab);
  check("ArrayBuffer cloned", cab instanceof ArrayBuffer && cab !== ab && cab.byteLength === 4);
  check("bytes copied", Array.from(new Uint8Array(cab)).join(",") === "1,2,3,4");

  const ta = new Uint8Array([9, 8, 7]);
  const cta = structuredClone(ta);
  check("TypedArray cloned", cta instanceof Uint8Array && cta !== ta);
  check("typed array bytes", Array.from(cta).join(",") === "9,8,7");

  const i32 = new Int32Array([1, -2, 300000]);
  const ci32 = structuredClone(i32);
  check("Int32Array cloned", ci32 instanceof Int32Array && Array.from(ci32).join(",") === "1,-2,300000");

  // view with offset into a larger buffer
  const big = new Uint8Array([0, 0, 5, 6, 7, 0]);
  const view = new Uint16Array(big.buffer, 2, 2);
  const cview = structuredClone(view);
  check("offset view cloned", cview instanceof Uint16Array && cview.length === 2);
  check("offset view bytes", cview[0] === 0x0605 && cview[1] === 0x0007);

  const dv = new DataView(big.buffer, 1, 3);
  const cdv = structuredClone(dv);
  check("DataView cloned", cdv instanceof DataView && cdv.byteLength === 3 && cdv.byteOffset === 1);
  check("DataView bytes", cdv.getUint8(0) === 0 && cdv.getUint8(2) === 6);
}

// ===================================================================
// 6. Date / RegExp / Error
// ===================================================================

function testSpecial() {
  console.log("\n=== structuredClone — Date / RegExp / Error ===");

  const d = new Date(1234567890123);
  const cd = structuredClone(d);
  check("Date cloned", cd instanceof Date && cd !== d && cd.getTime() === 1234567890123);

  const invalid = new Date(NaN);
  const ci = structuredClone(invalid);
  check("invalid Date stays invalid", ci instanceof Date && Number.isNaN(ci.getTime()));

  const re = /ab+c/gi;
  re.lastIndex = 5;
  const cre = structuredClone(re);
  check("RegExp cloned", cre instanceof RegExp && cre !== re);
  check("RegExp source/flags", cre.source === "ab+c" && cre.flags === "gi");
  check("RegExp lastIndex preserved", cre.lastIndex === 5);

  for (const [name, make, ctor] of [
    ["Error", () => new Error("plain"), Error],
    ["TypeError", () => new TypeError("boom"), TypeError],
    ["RangeError", () => new RangeError("range"), RangeError],
  ]) {
    const e = make();
    const ce = structuredClone(e);
    check(`${name} cloned`, ce instanceof ctor && ce !== e && ce.message === e.message);
  }
}

// ===================================================================
// 7. transfer
// ===================================================================

function testTransfer() {
  console.log("\n=== structuredClone — transfer ===");

  const t = new ArrayBuffer(8);
  new Uint8Array(t).set([42, 7]);
  const c = structuredClone(new Uint8Array(t), { transfer: [t] });
  check("transferred data present", c instanceof Uint8Array && c[0] === 42 && c[1] === 7);
  check("original detached", t.byteLength === 0);

  const t2 = new ArrayBuffer(4);
  new Uint8Array(t2)[0] = 99;
  const c2 = structuredClone(t2, { transfer: [t2] });
  check("ArrayBuffer transfer", c2 instanceof ArrayBuffer && new Uint8Array(c2)[0] === 99);
  check("ArrayBuffer source detached", t2.byteLength === 0);

  let threw = false;
  try {
    structuredClone(1, { transfer: [42] });
  } catch (e) {
    threw = e.name === "DataCloneError";
  }
  check("non-ArrayBuffer in transfer → DataCloneError", threw);

  threw = false;
  try {
    structuredClone(1, { transfer: "nope" });
  } catch (e) {
    threw = e instanceof TypeError;
  }
  check("non-array transfer → TypeError", threw);
}

// ===================================================================
// 8. non-serializable → DataCloneError
// ===================================================================

function testUnserializable() {
  console.log("\n=== structuredClone — DataCloneError ===");

  const bad = [
    ["function", () => 1],
    ["symbol", Symbol("s")],
    ["promise", Promise.resolve(1)],
    ["weakmap", new WeakMap()],
    ["weakset", new WeakSet()],
  ];
  for (const [label, v] of bad) {
    let name;
    try {
      structuredClone(v);
    } catch (e) {
      name = e?.name;
    }
    check(`${label} throws DataCloneError`, name === "DataCloneError", `got ${name}`);
  }

  // nested unserializable value propagates
  let nested;
  try {
    structuredClone({ fn: () => {} });
  } catch (e) {
    nested = e?.name;
  }
  check("nested unserializable propagates", nested === "DataCloneError");
}

// ===================================================================
// Runner
// ===================================================================

testShapePrimitives();
testDeepClone();
testCyclesAndIdentity();
testMapSet();
testBuffers();
testSpecial();
testTransfer();
testUnserializable();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
