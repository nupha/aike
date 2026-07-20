// jsonl.mjs — 验证 JSONL 模块的新 API（返回普通数组 T[]，无 JSONLArray 包装）
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
check("parse 是函数", typeof JSONL.parse === "function");
check("stringify 是函数", typeof JSONL.stringify === "function");
check("read 是函数", typeof JSONL.read === "function");
check("append 是函数", typeof JSONL.append === "function");
check("write 是函数", typeof JSONL.write === "function");
check("parseChunk 是函数", typeof JSONL.parseChunk === "function");
check("parseStream 是函数", typeof JSONL.parseStream === "function");
// 已移除的 API 不应存在
check("JSONLArray 类已移除", typeof JSONL.JSONLArray === "undefined");
check("tryParse 已移除", typeof JSONL.tryParse === "undefined");
check("fromArray 已移除", typeof JSONL.fromArray === "undefined");
check("readFromFile 已重命名移除", typeof JSONL.readFromFile === "undefined");
check("countLinesFromFile 已移除", typeof JSONL.countLinesFromFile === "undefined");

// ===================================================================
// parse — 跳过畸形行、返回普通数组
// ===================================================================
console.log("\n=== parse ===");
// 默认 onError="replace"：畸形行被 null 占位（无 failsafe 时用 JSON null）
const records = JSONL.parse('{"id":1}\n{invalid}\n{"id":2}');
check("parse 返回普通数组", Array.isArray(records), typeof records);
check("parse length=3 (replace 默认占位)", records.length === 3, "length=" + records.length);
check("records[0]", eq(records[0], { id: 1 }), JSON.stringify(records[0]));
check("records[2]", eq(records[2], { id: 2 }), JSON.stringify(records[2]));
check("records[1] 为 null 占位", records[1] === null, JSON.stringify(records[1]));
// 普通数组的 push / slice / splice
const len = records.push({ id: 3 });
check("push 返回新长度", len === 4, "len=" + len);
check("push 后 length=4", records.length === 4);
check(
  "stringify(数组)",
  JSONL.stringify(records) === '{"id":1}\nnull\n{"id":2}\n{"id":3}\n',
  JSONL.stringify(records),
);
const sliced = records.slice(0, 2);
check("slice(0,2)", sliced.length === 2 && eq(sliced[0], { id: 1 }));
records.splice(1, 0, { id: 9 });
check(
  "splice(1,0,item) 插入",
  records.length === 5 && eq(records[1], { id: 9 }),
  "len=" + records.length,
);
// Symbol.iterator
let sum = 0;
for (const v of records) sum += (v && v.id) || 0;
check("for..of 遍历", sum === 1 + 9 + 2 + 3, "sum=" + sum);

// ===================================================================
// parse onError="replace" / "throw" / "skip"
// ===================================================================
console.log("\n=== parse onError ===");
const rep = JSONL.parse('{"id":1}\n{invalid}\n{"id":2}', {
  onError: "replace",
  failsafe: { id: -1 },
});
check(
  "replace 用 failsafe 替代畸形行",
  rep.length === 3 && eq(rep[1], { id: -1 }),
);
let threw = false;
try {
  JSONL.parse('{"id":1}\n{invalid}\n{"id":2}', { onError: "throw" });
} catch (e) {
  threw = true;
}
// 原 parse 的 onError="throw" 退化为 skip（保持原行为：parse 永不抛，畸形行跳过）
check("parse(onError=throw) 退化为 skip（不抛）", threw === false);
const skipped = JSONL.parse("{invalid}\n{also bad}", { onError: "skip" });
check("完全无效(skip) 返回空数组", Array.isArray(skipped) && skipped.length === 0);

// ===================================================================
// stringify — 数组或单个对象
// ===================================================================
console.log("\n=== stringify ===");
check(
  "stringify(数组)",
  JSONL.stringify([{ a: 1 }, { a: 2 }]) === '{"a":1}\n{"a":2}\n',
);
check(
  "stringify(单个对象)",
  JSONL.stringify({ a: 1 }) === '{"a":1}\n',
);

// ===================================================================
// parseChunk — 返回 { values, attempted }
// ===================================================================
console.log("\n=== parseChunk ===");
const c1 = JSONL.parseChunk('{"id":1}\n{"id":2}\n{"id":3}\n', {
  onError: "skip",
});
check("parseChunk values", c1.values.length === 3);
check("parseChunk attempted", c1.attempted === 3, "attempted=" + c1.attempted);
// 无状态解析：text.lines() 会把最后一段（即使无结尾换行）当作一行解析
const partial = '{"id":1}\n{"id":2}\n{"id":3';
const c2 = JSONL.parseChunk(partial);
check(
  "parseChunk 解析全部行",
  c2.values.length === 3,
  "len=" + c2.values.length,
);
check("parseChunk attempted=3", c2.attempted === 3);

// ===================================================================
// read / append / write — 文件 I/O
// ===================================================================
console.log("\n=== read / append / write ===");
const fsPath = "/tmp/jsonl_test_" + Date.now() + ".jsonl";
await JSONL.append(fsPath, { id: 1 }, { id: 2 });
await JSONL.append(fsPath, { id: 3 });
const fromFile = await JSONL.read(fsPath);
check("read 读出 3 条", Array.isArray(fromFile) && fromFile.length === 3, "len=" + fromFile.length);
check(
  "read 内容正确",
  eq(fromFile[0], { id: 1 }) && eq(fromFile[2], { id: 3 }),
);
// read 完全无效 -> promise reject
let readThrew = false;
const badPath = "/tmp/jsonl_bad_" + Date.now() + ".jsonl";
fs.writeText(badPath, "{not json}\n{bad}\n");
try {
  await JSONL.read(badPath);
} catch (e) {
  readThrew = true;
}
check("read 完全无效 reject", readThrew === true);

// write 清空后重写
const rw = "/tmp/jsonl_rewrite_" + Date.now() + ".jsonl";
await JSONL.append(rw, { id: 1 }, { id: 2 }, { id: 3 });
await JSONL.write(rw, { a: 10 }, { a: 20 });
const rwBack = await JSONL.read(rw);
check(
  "write 清空+重写 -> 只剩新 2 条",
  rwBack.length === 2 && eq(rwBack[0], { a: 10 }) && eq(rwBack[1], { a: 20 }),
  "len=" + rwBack.length,
);
// write 后再 append 叠加
await JSONL.append(rw, { a: 30 });
const rwBack2 = await JSONL.read(rw);
check(
  "write 后 append 叠加 -> 3 条",
  rwBack2.length === 3 && eq(rwBack2[2], { a: 30 }),
  "len=" + rwBack2.length,
);
// write 无 values -> 清空为 0 条
const rwEmpty = "/tmp/jsonl_rewrite_empty_" + Date.now() + ".jsonl";
await JSONL.append(rwEmpty, { x: 1 }, { x: 2 });
await JSONL.write(rwEmpty);
const rwEmptyBack = await JSONL.read(rwEmpty);
check("write 无 values -> 清空为 0 条", rwEmptyBack.length === 0, "len=" + rwEmptyBack.length);

// ===================================================================
// parseStream — 返回 { iterator, lines }
// ===================================================================
console.log("\n=== parseStream ===");
const ps = JSONL.parseStream('{"id":1}\n{"id":2}\n{"id":3}\n{"id":4}\n');
check("parseStream lines=4", ps.lines === 4, "lines=" + ps.lines);
const out = [];
for await (const rec of ps.iterator) out.push(rec.id);
check(
  "parseStream 逐行产出 1,2,3,4",
  eq(out, [1, 2, 3, 4]),
  JSON.stringify(out),
);

// parseStream onError=replace
const bad = JSONL.parseStream('{"id":1}\n{bad}\n{"id":2}\n', {
  onError: "replace",
  failsafe: null,
});
const outBad = [];
for await (const rec of bad.iterator) outBad.push(rec);
check(
  "parseStream replace 畸形行",
  outBad.length === 3 && outBad[1] === null,
  JSON.stringify(outBad),
);

// ===================================================================
// spread — append(path, ...values)
// ===================================================================
console.log("\n=== spread ===");
const plainArr = [{ id: 1 }, { id: 2 }, { id: 3 }];
const sp1 = "/tmp/jsonl_spread_plain_" + Date.now() + ".jsonl";
await JSONL.append(sp1, ...plainArr);
const sp1Back = await JSONL.read(sp1);
check(
  "spread 普通数组 -> 每行一个元素",
  sp1Back.length === plainArr.length &&
    eq(sp1Back[0], { id: 1 }) && eq(sp1Back[2], { id: 3 }),
  "len=" + sp1Back.length,
);
// 不 spread 直接传单个对象 -> 仅 1 行
const sp3 = "/tmp/jsonl_spread_nospread_" + Date.now() + ".jsonl";
await JSONL.append(sp3, { wrap: plainArr });
const sp3Back = await JSONL.read(sp3);
check(
  "不 spread 传单个对象 -> 仅 1 行",
  sp3Back.length === 1,
  "len=" + sp3Back.length,
);

// ===================================================================
// 结果汇总
// ===================================================================
console.log(`\n==== 结果: ${pass} passed, ${fail} failed ====`);
if (fail > 0) process.exit(1);
