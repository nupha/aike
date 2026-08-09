// grep.mjs — fs.grep 模块测试（ripgrep 风格内容搜索、ignore 支持、选项覆盖）
import * as fs from "fs";
import * as path from "path";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    pass++;
    console.log(`  ✅ ${label}`);
  } else {
    fail++;
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
  }
}
function eq(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}
const sort = (a) => a.slice().sort();

// ===================================================================
// 模块基础
// ===================================================================
console.log("\n=== 模块基础 ===");
check("fs.grep 是函数", typeof fs.grep === "function", typeof fs.grep);
check(
  "fs.grep.length 参数个数",
  fs.grep.length === 1,
  "length=" + fs.grep.length,
);

// ===================================================================
// 准备测试目录树
// ===================================================================
const Dir = path.resolve("../tmp", "grep_" + Date.now());
await fs.makeDir(path.join(Dir, "src"), { recursive: true });
await fs.makeDir(path.join(Dir, "sub"), { recursive: true });
// a.txt: one match on a line.
await fs.writeText(
  path.join(Dir, "a.txt"),
  "hello world\nfoo bar\nHELLO again\n",
);
// b.rs: two matches on the same line (multiple ranges).
await fs.writeText(
  path.join(Dir, "b.rs"),
  "matchOne and matchTwo on one line\n",
);
// src/lib.rs: no match.
await fs.writeText(path.join(Dir, "src", "lib.rs"), "nothing here\n");
// sub/.hidden.rs: hidden, matched only with dot.
await fs.writeText(path.join(Dir, "sub", ".hidden.rs"), "hello from hidden\n");
// ignored.txt: excluded via .gitignore.
await fs.writeText(path.join(Dir, ".gitignore"), "ignored.txt\n");
await fs.writeText(path.join(Dir, "ignored.txt"), "hello should be ignored\n");
const cwd = { cwd: Dir };

// ===================================================================
// 基础内容匹配
// ===================================================================
console.log("\n=== 基础匹配 ===");
{
  const r = await fs.grep("hello", cwd);
  const files = sort(r.map((m) => m.file));
  check(
    "hello 命中 a.txt（默认尊重 ignore，ignored.txt 应被跳过）",
    files.includes("a.txt") && !files.includes("ignored.txt"),
    JSON.stringify(files),
  );
  check("返回为数组", Array.isArray(r));
  check(
    "命中对象含 file/lineNumber/line/start/end/ranges",
    r.every(
      (m) =>
        typeof m.file === "string" &&
        typeof m.lineNumber === "number" &&
        typeof m.line === "string" &&
        typeof m.start === "number" &&
        typeof m.end === "number" &&
        Array.isArray(m.ranges),
    ),
  );
}
{
  const r = await fs.grep("hello", cwd);
  const a = r.find((m) => m.file === "a.txt");
  check(
    "a.txt 第 1 行命中 (lineNumber=1)",
    a && a.lineNumber === 1,
    JSON.stringify(a),
  );
  check(
    "a.txt 命中行内容正确",
    a && a.line.trim() === "hello world",
    a && a.line,
  );
  check(
    "a.txt 命中区间 [0,5)",
    a && a.start === 0 && a.end === 5,
    a && `${a.start}-${a.end}`,
  );
}

// ===================================================================
// caseSensitive
// ===================================================================
console.log("\n=== caseSensitive ===");
{
  const r = await fs.grep("HELLO", { ...cwd, caseSensitive: false });
  const lines = r.map((m) => m.line.trim()).sort();
  check(
    "caseSensitive=false 命中小写与大写",
    lines.includes("hello world") && lines.includes("HELLO again"),
    JSON.stringify(lines),
  );
}
{
  const r = await fs.grep("HELLO", { ...cwd, caseSensitive: true });
  check(
    "caseSensitive=true 只命中大写",
    r.length === 1 && r[0].line.includes("HELLO again"),
    JSON.stringify(r.map((m) => m.line.trim())),
  );
}

// ===================================================================
// 单行多匹配 (ranges)
// ===================================================================
console.log("\n=== 单行多匹配 ===");
{
  const r = await fs.grep("match", { ...cwd, caseSensitive: true });
  const b = r.find((m) => m.file === "b.rs");
  check(
    "b.rs 单行含两个匹配",
    b && b.ranges.length === 2,
    b && JSON.stringify(b.ranges),
  );
  if (b) {
    // Pattern is "match" so each range is "match" inside matchOne/matchTwo.
    const words = b.ranges.map((rg) => b.line.substring(rg.start, rg.end));
    check(
      "ranges 对应两个 match 子串",
      eq(words, ["match", "match"]),
      JSON.stringify(words),
    );
  }
}

// ===================================================================
// ignore 支持
// ===================================================================
console.log("\n=== ignore (.gitignore) ===");
{
  const r = await fs.grep("hello", cwd);
  const files = r.map((m) => m.file);
  check(
    "默认尊重 .gitignore：ignored.txt 被跳过",
    !files.includes("ignored.txt"),
    JSON.stringify(files),
  );
}
{
  const r = await fs.grep("hello", { ...cwd, respectIgnore: false });
  const files = r.map((m) => m.file);
  check(
    "respectIgnore=false：ignored.txt 被搜到",
    files.includes("ignored.txt"),
    JSON.stringify(files),
  );
}

// ===================================================================
// dot — 隐藏文件
// ===================================================================
console.log("\n=== dot ===");
{
  const r = await fs.grep("hidden", cwd);
  check(
    "dot=false（默认）跳过隐藏文件",
    !r.some((m) => m.file.includes(".hidden.rs")),
    JSON.stringify(r.map((m) => m.file)),
  );
}
{
  const r = await fs.grep("hidden", { ...cwd, dot: true });
  check(
    "dot=true 命中隐藏文件",
    r.some((m) => m.file === "sub/.hidden.rs"),
    JSON.stringify(r.map((m) => m.file)),
  );
}

// ===================================================================
// paths / include / exclude
// ===================================================================
console.log("\n=== paths / include / exclude ===");
{
  const r = await fs.grep("hello", { ...cwd, paths: "*.txt" });
  const files = Array.from(new Set(r.map((m) => m.file))).sort();
  check(
    "paths=*.txt 只搜 a.txt",
    eq(files, ["a.txt"]),
    JSON.stringify(files),
  );
}
{
  const r = await fs.grep("hello", { ...cwd, include: ["*.rs"] });
  check(
    "include=*.rs 不含 a.txt",
    !r.some((m) => m.file === "a.txt"),
    JSON.stringify(r.map((m) => m.file)),
  );
}
{
  const r = await fs.grep("hello", { ...cwd, exclude: ["a.txt"] });
  check(
    "exclude=a.txt 排除该文件",
    !r.some((m) => m.file === "a.txt"),
    JSON.stringify(r.map((m) => m.file)),
  );
}

// ===================================================================
// absolute
// ===================================================================
console.log("\n=== absolute ===");
{
  const r = await fs.grep("hello", { ...cwd, absolute: true });
  check(
    "absolute=true 返回绝对路径",
    r.every((m) => path.isAbsolute(m.file)),
    JSON.stringify(r.map((m) => m.file)),
  );
}

// ===================================================================
// maxResults
// ===================================================================
console.log("\n=== maxResults ===");
{
  const r = await fs.grep("hello", { ...cwd, maxResults: 1 });
  check("maxResults=1 限制返回条数", r.length <= 1, "len=" + r.length);
}

// ===================================================================
// 错误处理
// ===================================================================
console.log("\n=== 错误处理 ===");
let threw = false;
let msg = "";
try {
  await fs.grep("(", cwd);
} catch (e) {
  threw = true;
  msg = String(e);
}
check("非法正则 reject", threw, msg);
check(
  "非法正则抛出 SyntaxError",
  threw && /invalid grep pattern|SyntaxError/.test(msg),
  msg,
);

// ===================================================================
// 清理
// ===================================================================
await fs.removeDir(Dir, { recursive: true }).catch(() => {});

// ===================================================================
// 结果汇总
// ===================================================================
console.log(
  `\n  ✅ Pass: ${pass}  ❌ Fail: ${fail}  ${fail > 0 ? "" : "ALL TESTS PASSED"}`,
);
if (fail > 0) process.exit(1);
