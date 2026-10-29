// yaml.mjs — 验证 YAML.parse / YAML.stringify
import { YAML } from "yaml";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  \u2705 ${label}`);
    pass++;
  } else {
    console.log(`  \u274C ${label}${detail ? " \u2014 " + detail : ""}`);
    fail++;
  }
}

// ===================================================================
// 模块基础
// ===================================================================
console.log("\n=== 模块基础 ===");

check("YAML 是对象", typeof YAML === "object" && YAML !== null);
check("YAML.parse 是函数", typeof YAML.parse === "function");
check("YAML.stringify 是函数", typeof YAML.stringify === "function");
check("YAML.prototype 为 undefined", YAML.prototype === undefined);

// ===================================================================
// YAML.parse — 基础类型
// ===================================================================
console.log("\n=== YAML.parse — 基础类型 ===");

// 字符串
{
  const v = YAML.parse("hello");
  check("裸字符串", v === "hello", `got ${JSON.stringify(v)}`);
}

{
  const v = YAML.parse('"hello world"');
  check("引号字符串", v === "hello world", `got ${JSON.stringify(v)}`);
}

// 数字
{
  const v = YAML.parse("42");
  check("整数", v === 42, `got ${v}`);
}

{
  const v = YAML.parse("3.14");
  check("浮点数", v === 3.14, `got ${v}`);
}

// 布尔值
{
  const v = YAML.parse("true");
  check("布尔 true", v === true, `got ${v}`);
}

{
  const v = YAML.parse("false");
  check("布尔 false", v === false, `got ${v}`);
}

// null
{
  const v = YAML.parse("null");
  check("null 关键字", v === null, `got ${v}`);
}

{
  const v = YAML.parse("~");
  check("~ 表示 null", v === null, `got ${v}`);
}

// ===================================================================
// YAML.parse — 复合类型
// ===================================================================
console.log("\n=== YAML.parse — 复合类型 ===");

// object (mapping)
{
  const v = YAML.parse("name: Alice\nage: 30");
  check("mapping 返回 object", typeof v === "object" && v !== null && !Array.isArray(v));
  check("mapping name", v.name === "Alice", `got ${v.name}`);
  check("mapping age", v.age === 30, `got ${v.age}`);
}

// 嵌套 object
{
  const v = YAML.parse("server:\n  host: localhost\n  port: 8080");
  check("嵌套 object", typeof v.server === "object" && v.server !== null);
  check("嵌套 host", v.server.host === "localhost", `got ${v.server.host}`);
  check("嵌套 port", v.server.port === 8080, `got ${v.server.port}`);
}

// array (sequence)
{
  const v = YAML.parse("- a\n- b\n- c");
  check("sequence 返回 array", Array.isArray(v));
  check("sequence 长度", v.length === 3, `got ${v.length}`);
  check("sequence[0]", v[0] === "a", `got ${v[0]}`);
  check("sequence[1]", v[1] === "b", `got ${v[1]}`);
  check("sequence[2]", v[2] === "c", `got ${v[2]}`);
}

// 混合嵌套
{
  const v = YAML.parse("items:\n  - name: foo\n    price: 10\n  - name: bar\n    price: 20");
  check("混合嵌套 items 是数组", Array.isArray(v.items));
  check("混合嵌套 items 长度", v.items.length === 2, `got ${v.items.length}`);
  check("混合嵌套 items[0].name", v.items[0].name === "foo", `got ${v.items[0].name}`);
  check("混合嵌套 items[0].price", v.items[0].price === 10, `got ${v.items[0].price}`);
  check("混合嵌套 items[1].name", v.items[1].name === "bar", `got ${v.items[1].name}`);
  check("混合嵌套 items[1].price", v.items[1].price === 20, `got ${v.items[1].price}`);
}

// 空 mapping
{
  const v = YAML.parse("{}\n");
  check("空 mapping", typeof v === "object" && v !== null && Object.keys(v).length === 0);
}

// 空 sequence
{
  const v = YAML.parse("[]\n");
  check("空 sequence", Array.isArray(v) && v.length === 0);
}

// 多行文本
{
  const v = YAML.parse("text: |\n  hello\n  world\n");
  check("块文本", typeof v.text === "string", `type ${typeof v.text}`);
  check("块文本含换行", v.text.includes("\n"), `got ${JSON.stringify(v.text)}`);
}

// 注释
{
  const v = YAML.parse("# 这是一条注释\nkey: value\n");
  check("跳过注释", v.key === "value", `got ${v.key}`);
}

// 锚点和别名（serde_yaml 反序列化为 JSON Value 时不支持 <<: merge key，跳过）

// ===================================================================
// YAML.stringify
// ===================================================================
console.log("\n=== YAML.stringify ===");

// 字符串
{
  const s = YAML.stringify("hello").trim();
  check("stringify 字符串", s === "hello", `got ${JSON.stringify(s)}`);
}

// 数字
{
  const s = YAML.stringify(42).trim();
  check("stringify 整数", s === "42", `got ${s}`);
}

{
  const s = YAML.stringify(3.14).trim();
  check("stringify 浮点数", s.startsWith("3.14"), `got ${s}`);
}

// 布尔值
{
  const s = YAML.stringify(true).trim();
  check("stringify true", s === "true", `got ${s}`);
}

{
  const s = YAML.stringify(false).trim();
  check("stringify false", s === "false", `got ${s}`);
}

// null
{
  const s = YAML.stringify(null).trim();
  check("stringify null", s === "null" || s === "~", `got ${s}`);
}

// object
{
  const s = YAML.stringify({ name: "test", value: 42 });
  check("stringify object", typeof s === "string", `type ${typeof s}`);
  check("stringify object 含 name", s.includes("name:"), `got ${s}`);
  check("stringify object 含 value 42", s.includes("42"), `got ${s}`);
}

// array
{
  const s = YAML.stringify([1, 2, 3]);
  check("stringify array", typeof s === "string");
  check("stringify array 含 1", s.includes("1"), `got ${s}`);
  check("stringify array 含 2", s.includes("2"), `got ${s}`);
  check("stringify array 含 3", s.includes("3"), `got ${s}`);
}

// 空对象
{
  const s = YAML.stringify({});
  check("stringify 空对象", s.trim() === "{}" || s.trim() === "", `got ${JSON.stringify(s)}`);
}

// 空数组
{
  const s = YAML.stringify([]);
  check("stringify 空数组", s.trim() === "[]" || s.trim() === "", `got ${JSON.stringify(s)}`);
}

// 嵌套
{
  const obj = { server: { host: "localhost", port: 8080 } };
  const s = YAML.stringify(obj);
  check("stringify 嵌套", typeof s === "string");
  check("stringify 嵌套含 host", s.includes("localhost"), `got ${s}`);
  check("stringify 嵌套含 port", s.includes("8080"), `got ${s}`);
}

// ===================================================================
// 往返一致性 (parse → stringify → parse)
// ===================================================================
console.log("\n=== 往返一致性 ===");

{
  const obj = { name: "Alice", age: 30, active: true, score: null };
  const yaml = YAML.stringify(obj);
  const back = YAML.parse(yaml);
  check("往返 name", back.name === "Alice", `got ${back.name}`);
  check("往返 age", back.age === 30, `got ${back.age}`);
  check("往返 active", back.active === true, `got ${back.active}`);
}

{
  const arr = ["x", "y", "z"];
  const yaml = YAML.stringify(arr);
  const back = YAML.parse(yaml);
  check("数组往返", Array.isArray(back), `type ${typeof back}`);
  check("数组往返长度", back.length === 3, `got ${back.length}`);
  check("数组往返[0]", back[0] === "x", `got ${back[0]}`);
}

// ===================================================================
// 错误处理
// ===================================================================
console.log("\n=== 错误处理 ===");

// 非法 YAML 抛出错误
{
  let threw = false;
  try {
    YAML.parse(": broken indentation\n  bad");
  } catch (_) {
    threw = true;
  }
  check("非法 YAML 抛出错误", threw);
}

{
  const v = YAML.parse("");
  check("空字符串 返回 undefined", v === undefined, `got ${v}`);
}

// stringify 无法序列化的值
{
  let threw = false;
  try {
    YAML.stringify(undefined);
  } catch (_) {
    threw = true;
  }
  check("stringify undefined 抛出错误", threw);
}

{
  let threw = false;
  try {
    YAML.stringify(() => {});
  } catch (_) {
    threw = true;
  }
  check("stringify 函数 抛出错误", threw);
}

// ===================================================================
// 结果
// ===================================================================
console.log(`\n=== \u7ED3\u679C: ${pass} passed, ${fail} failed ===\n`);

if (fail > 0) {
  console.error("Some tests FAILED");
  process.exit(1);
}
