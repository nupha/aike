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
// grep 返回平台原生分隔符（Windows 为 \，与 fs.glob 一致），故用 path.join
// 构造期望路径，避免把 POSIX 写法写死在断言里。
const HIDDEN_RS = path.join("sub", ".hidden.rs");
const LIB_RS = path.join("src", "lib.rs");
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

// 无匹配边界
{
  const r = await fs.grep("zzz_no_such_pattern_xyz", cwd);
  check(
    "无匹配返回空数组",
    Array.isArray(r) && r.length === 0,
    JSON.stringify(r),
  );
}

// 多文件各自 lineNumber 正确
{
  const r = await fs.grep("hello", cwd);
  // a.txt 第 1 行、第 3 行(HELLO) 各命中一次
  const aLines = r
    .filter((m) => m.file === "a.txt")
    .map((m) => m.lineNumber)
    .sort();
  check(
    "a.txt 在第 1、3 行各命中（lineNumber 各自正确）",
    eq(aLines, [1, 3]),
    JSON.stringify(aLines),
  );
}

// 正则锚点 / 字符类（验证 regex 引擎能力）
{
  const r = await fs.grep("^foo", { ...cwd, caseSensitive: true });
  check(
    "行首锚点 ^foo 只命中 'foo bar'",
    r.length === 1 && r[0].line.trim() === "foo bar",
    JSON.stringify(r.map((m) => m.line.trim())),
  );
}
{
  const r = await fs.grep("bar$", { ...cwd, caseSensitive: true });
  check(
    "行尾锚点 bar$ 只命中 'foo bar'（多行模式下 $ 匹配行尾）",
    r.length === 1 && r[0].line.trim() === "foo bar",
    JSON.stringify(r.map((m) => m.line.trim())),
  );
}
{
  const r = await fs.grep("\\w+", {
    ...cwd,
    caseSensitive: true,
    maxResults: 1,
  });
  check("字符类 \\w+ 正常匹配", r.length === 1, JSON.stringify(r.length));
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
    r.some((m) => m.file === HIDDEN_RS),
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
  check("paths=*.txt 只搜 a.txt", eq(files, ["a.txt"]), JSON.stringify(files));
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
{
  // 多模式逗号分隔（实现按逗号拆分 paths）。
  // 注意：b.rs 内容不含 "hello"，故不会出现在结果里；改用两个都含 "hello"
  // 的文件：a.txt（含 hello）与 ignored.txt（需关闭 ignore 才会被搜到）。
  const r = await fs.grep("hello", {
    ...cwd,
    respectIgnore: false,
    paths: "a.txt,ignored.txt",
  });
  const files = Array.from(new Set(r.map((m) => m.file))).sort();
  check(
    "paths 逗号分隔多模式 'a.txt,ignored.txt'（关闭 ignore）",
    eq(files, ["a.txt", "ignored.txt"]),
    JSON.stringify(files),
  );
}
{
  // 递归 include：*.rs 命中 src/lib.rs；隐藏 .hidden.rs 需 dot
  const r = await fs.grep("nothing", { ...cwd, include: ["**/*.rs"] });
  check(
    "include=**/*.rs 递归命中 src/lib.rs",
    r.some((m) => m.file === LIB_RS),
    JSON.stringify(r.map((m) => m.file)),
  );
}
{
  // respectIgnore + exclude 组合
  const r = await fs.grep("hello", {
    ...cwd,
    respectIgnore: true,
    exclude: ["*.txt"],
  });
  check(
    "respectIgnore + exclude=*.txt 只剩非 txt（ignored.txt 本就被 ignore）",
    !r.some((m) => m.file.endsWith(".txt")),
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
{
  // 跨文件截断：a.txt 有 2 行匹配、b.rs 1 行、ignored.txt(关 ignore)不计
  // src/lib.rs 无匹配。总命中 > 1，设 maxResults=2 验证跨文件截断生效。
  const r = await fs.grep("hello", { ...cwd, maxResults: 2 });
  check("maxResults=2 跨文件截断", r.length <= 2, "len=" + r.length);
}

// ===================================================================
// mode: "files" (-l) / "count" (-c) 与 invert (-v) 组合
// ===================================================================
console.log("\n=== mode: files / count / invert ===");

// -l: 含匹配的文件
{
  const r = await fs.grep("hello", {
    ...cwd,
    respectIgnore: false,
    mode: "files",
  });
  check(
    "mode=files (-l) 列出含匹配的文件",
    eq(sort(r), ["a.txt", "ignored.txt"]),
    JSON.stringify(r),
  );
}
// -c: 每文件匹配行数
{
  const r = await fs.grep("hello", {
    ...cwd,
    respectIgnore: false,
    mode: "count",
  });
  const map = Object.fromEntries(r.map((c) => [c.file, c.count]));
  check(
    "mode=count (-c) a.txt 命中 2 行、ignored.txt 命中 1 行",
    map["a.txt"] === 2 && map["ignored.txt"] === 1,
    JSON.stringify(map),
  );
}
// -v: 反向匹配（content 模式）
{
  const r = await fs.grep("hello", { ...cwd, invert: true });
  // a.txt 三行中第 1、3 行含 hello，仅第 2 行 "foo bar" 反向保留
  const aLines = r.filter((m) => m.file === "a.txt").map((m) => m.line.trim());
  check(
    "invert=true (-v) 仅保留不含匹配的行",
    eq(aLines, ["foo bar"]),
    JSON.stringify(aLines),
  );
}
// -l -v: 不含匹配的文件（--files-without-match）
{
  const r = await fs.grep("hello", { ...cwd, mode: "files", invert: true });
  // dot 默认 false，sub/.hidden.rs 不在；b.rs/src/lib.rs 无 hello
  check(
    "mode=files + invert (--files-without-match)",
    eq(sort(r), ["b.rs", LIB_RS]),
    JSON.stringify(r),
  );
}
// -c -v: 每文件非匹配行数
{
  const r = await fs.grep("hello", { ...cwd, mode: "count", invert: true });
  const map = Object.fromEntries(r.map((c) => [c.file, c.count]));
  // a.txt 3 行 - 2 命中 = 1 非匹配；src/lib.rs 1 行非匹配；b.rs 1 行非匹配
  check(
    "mode=count + invert 统计非匹配行数",
    map["a.txt"] === 1 && map[LIB_RS] === 1 && map["b.rs"] === 1,
    JSON.stringify(map),
  );
}
// invert 反向模式下不含匹配偏移（ranges 为空数组）
{
  const r = await fs.grep("hello", { ...cwd, invert: true, maxResults: 1 });
  check(
    "invert 模式返回行 ranges 为空数组",
    r.length >= 1 && Array.isArray(r[0].ranges) && r[0].ranges.length === 0,
    JSON.stringify(r[0]),
  );
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
