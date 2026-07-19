// jsonl.mjs — 验证 JSONL 模块的全部 API
import * as fs from "fs";
import * as JSONL from "jsonl";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    pass++;
    console.log(`  ok: ${label}`);
  } else {
    fail++;
    console.log(`  FAIL: ${label}${detail ? " — " + detail : ""}`);
  }
}
function eq(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

// ===================================================================
// 模块基础
// ===================================================================
console.log("\n=== 模块基础 ===");
check("JSONL 是对象", typeof JSONL === "object" && JSONL !== null);
check("JSONLArray 是类", typeof JSONL.JSONLArray === "function");
check("parse 是函数", typeof JSONL.parse === "function");
check("tryParse 是函数", typeof JSONL.tryParse === "function");
check("read 是函数", typeof JSONL.readFromFile === "function");
check("stringify 是函数", typeof JSONL.stringify === "function");
check("append 是函数", typeof JSONL.appendToFile === "function");
check("parseChunk 是函数", typeof JSONL.parseChunk === "function");
check("parseStream 是函数", typeof JSONL.parseStream === "function");

// ===================================================================
// parse — 跳过畸形行、返回 JSONLArray
// ===================================================================
console.log("\n=== parse ===");
const records = JSONL.parse('{"id":1}\n{invalid}\n{"id":2}');
check(
  "parse 返回 JSONLArray(length=2)",
  records.length === 2,
  "length=" + records.length,
);
check("get(0)", eq(records.get(0), { id: 1 }), JSON.stringify(records.get(0)));
check("get(1)", eq(records.get(1), { id: 2 }), JSON.stringify(records.get(1)));
check(
  "push 返回新长度",
  records.push({ id: 3 }) === 3,
  "len=" + records.length,
);
check(
  "stringify",
  records.stringify() === '{"id":1}\n{"id":2}\n{"id":3}\n',
  records.stringify(),
);
check("toString == stringify", records.toString() === records.stringify());
check(
  "JSON.stringify(arr) 为普通数组",
  JSON.stringify(records) === '[{"id":1},{"id":2},{"id":3}]',
);

// 负索引
check("get(-1)", eq(records.get(-1), { id: 3 }));
check(
  "get(-99, fallback) returns fallback",
  records.get(-99, { x: 0 }) && eq(records.get(-99, { x: 0 }), { x: 0 }),
);
check("get(99, fallback) returns fallback", eq(records.get(99, "FB"), "FB"));
check("get(99) 越界 undefined", records.get(99) === undefined);

// slice / splice
const sliced = records.slice(0, 2);
check("slice(0,2)", sliced.length === 2 && eq(sliced.get(0), { id: 1 }));
const removed = records.splice(0, 1);
check("splice(0,1) 删除 1 条", records.length === 2, "len=" + records.length);
check(
  "splice 返回被删 JSONLArray",
  removed.length === 1 && eq(removed.get(0), { id: 1 }),
);
const removed2 = records.splice(1, 0, { id: 9 });
check(
  "splice(1,0,item) 插入",
  records.length === 3 && eq(records.get(1), { id: 9 }),
);
check("splice 返回空删除数组", removed2.length === 0);

// Symbol.iterator
let sum = 0;
for (const v of records) sum += v.id;
check("for..of 遍历", sum === 2 + 9 + 3, "sum=" + sum);

// toJSON
check(
  "toJSON 为普通数组",
  Array.isArray(records.toJSON()) && records.toJSON().length === 3,
);

// ===================================================================
// parse onError="replace" / "throw"
// ===================================================================
console.log("\n=== parse onError ===");
const rep = JSONL.parse('{"id":1}\n{invalid}\n{"id":2}', {
  onError: "replace",
  failsafe: { id: -1 },
});
check(
  "replace 用 failsafe 替代畸形行",
  rep.length === 3 && eq(rep.get(1), { id: -1 }),
);
let threw = false;
try {
  JSONL.parse('{"id":1}\n{invalid}\n{"id":2}', { onError: "throw" });
} catch (e) {
  threw = true;
}
check("throw 立即抛异常", threw === true);
let threwAll = false;
try {
  JSONL.parse("{invalid}\n{also bad}", { onError: "skip" });
} catch (e) {
  threwAll = true;
}
check("完全无效(skip) 抛 SyntaxError", threwAll === true);

// ===================================================================
// tryParse — 永不抛
// ===================================================================
console.log("\n=== tryParse ===");
const safe = JSONL.tryParse("{invalid}\n{also bad}");
check("tryParse 完全无效返回空数组", safe.length === 0);
const safe2 = JSONL.tryParse('{"a":1}\n{bad}\n{"a":2}', {
  onError: "replace",
  failsafe: null,
});
check("tryParse replace", safe2.length === 3 && safe2.get(1) === null);
let tpThrew = false;
try {
  JSONL.tryParse("{bad}", { onError: "throw" });
} catch (e) {
  tpThrew = true;
}
check("tryParse onError=throw 被降级为 skip（不抛）", tpThrew === false);

// ===================================================================
// stringify — JSONLArray 或数组
// ===================================================================
console.log("\n=== stringify ===");
check(
  "stringify(JSONLArray)",
  JSONL.stringify(records) === records.stringify(),
);
check(
  "stringify(普通数组)",
  JSONL.stringify([{ a: 1 }, { a: 2 }]) === '{"a":1}\n{"a":2}\n',
);
check(
  "stringify(单个对象)",
  JSONL.stringify({ a: 1 }) === '{"a":1}\n',
);

// ===================================================================
// parseChunk — 流式增量解析，永不抛
// ===================================================================
console.log("\n=== parseChunk ===");
// 完整行（带换行结尾）
const c1 = JSONL.parseChunk('{"id":1}\n{"id":2}\n{"id":3}\n', {
  onError: "skip",
});
check("parseChunk values", c1.values.length === 3);
check("parseChunk read 全部消费", c1.read === 27, "read=" + c1.read);
check("parseChunk done", c1.done === true);
check("parseChunk error 为 null", c1.error === null);
// 部分行（无换行结尾）
const partial = '{"id":1}\n{"id":2}\n{"id":3';
const c2 = JSONL.parseChunk(partial);
check(
  "parseChunk 只解析完整行",
  c2.values.length === 2,
  "len=" + c2.values.length,
);
check("parseChunk read 截到最后一个 \\n", c2.read === 18, "read=" + c2.read);
check("parseChunk done=false（有残余）", c2.done === false);
// 模拟流式累积
let buf = "";
let total = 0;
for (const chunk of ['{"id":1}\n{"id":2', '}\n{"id":3}\n']) {
  buf += chunk;
  const r = JSONL.parseChunk(buf);
  for (const v of r.values) {
    total += v.id;
  }
  buf = buf.slice(r.read);
}
check("流式累积解析出 1,2,3", total === 6, "total=" + total);
check("残余 buffer 为空", buf === "", "buf=" + JSON.stringify(buf));

// Uint8Array 输入
const ua = new Uint8Array([123, 34, 105, 100, 34, 58, 49, 125, 10]); // {"id":1}\n
const c3 = JSONL.parseChunk(ua);
check(
  "parseChunk 接受 Uint8Array",
  c3.values.length === 1 && eq(c3.values.get(0), { id: 1 }),
);

// ===================================================================
// read / append — 文件 I/O
// ===================================================================
console.log("\n=== read / append ===");
const fsPath = "/tmp/jsonl_test_" + Date.now() + ".jsonl";
await JSONL.appendToFile(fsPath, { id: 1 }, { id: 2 });
await JSONL.appendToFile(fsPath, { id: 3 });
const fromFile = await JSONL.readFromFile(fsPath);
check("read 读出 3 条", fromFile.length === 3, "len=" + fromFile.length);
check(
  "read 内容正确",
  eq(fromFile.get(0), { id: 1 }) && eq(fromFile.get(2), { id: 3 }),
);
// read 完全无效抛（promise reject）。append 永远写出合法 JSON，故用手写畸形文件。
let readThrew = false;
const badPath = "/tmp/jsonl_bad_" + Date.now() + ".jsonl";
fs.writeText(badPath, "{not json}\n{bad}\n");
try {
  await JSONL.readFromFile(badPath);
} catch (e) {
  readThrew = true;
}
check("read 完全无效 reject", readThrew === true);

// ===================================================================
// parseStream — AsyncIterable
// ===================================================================
console.log("\n=== parseStream ===");
function makeStream(chunks) {
  let i = 0;
  return {
    next() {
      if (i < chunks.length) {
        const chunk = chunks[i++];
        return Promise.resolve({ value: chunk, done: false });
      }
      return Promise.resolve({ value: undefined, done: true });
    },
  };
}
const streamChunks = ['{"id":1}\n{"id":2}\n', '{"id":3}\n', '{"id":4}\n'];
const out = [];
for await (const rec of JSONL.parseStream(
  makeStream(streamChunks),
)) {
  out.push(rec.id);
}
check(
  "parseStream 逐行产出 1,2,3,4",
  eq(out, [1, 2, 3, 4]),
  JSON.stringify(out),
);

// parseStream onError=replace
const streamBad = ['{"id":1}\n', "{bad}\n", '{"id":2}\n'];
const outBad = [];
for await (const rec of JSONL.parseStream(makeStream(streamBad), {
  onError: "replace",
  failsafe: null,
})) {
  outBad.push(rec);
}
check(
  "parseStream replace 畸形行",
  outBad.length === 3 && outBad[1] === null,
  JSON.stringify(outBad),
);

// ===================================================================
// 结果汇总
// ===================================================================
console.log(`\n==== 结果: ${pass} passed, ${fail} failed ====`);
if (fail > 0) process.exit(1);
