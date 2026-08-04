// glob.mjs — fs.glob 模块测试（模式匹配、globstar、cwd/absolute、dot、exclude、caseSensitive）
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
check("fs.glob 是函数", typeof fs.glob === "function", typeof fs.glob);
check("fs.glob.length 参数个数", fs.glob.length === 1, "length=" + fs.glob.length);

// ===================================================================
// 准备测试目录树
// ===================================================================
const Dir = path.resolve("../tmp", "glob_" + Date.now());
await fs.makeDir(path.join(Dir, "src"), { recursive: true });
await fs.makeDir(path.join(Dir, "deep", "x"), { recursive: true });
await fs.makeDir(path.join(Dir, "sub"), { recursive: true });
await fs.writeText(path.join(Dir, "a.txt"), "x");
await fs.writeText(path.join(Dir, "b.rs"), "x");
await fs.writeText(path.join(Dir, "main.rs"), "x");
await fs.writeText(path.join(Dir, ".hidden.txt"), "x");
await fs.writeText(path.join(Dir, "src", "lib.rs"), "x");
await fs.writeText(path.join(Dir, "src", "mod.rs"), "x");
await fs.writeText(path.join(Dir, "deep", "x", "y.rs"), "x");
await fs.writeText(path.join(Dir, "sub", ".hidden.rs"), "x");
const cwd = { cwd: Dir };
// Marker file in the process cwd, used to verify the default cwd is the cwd.
const MARKER = "_glob_cwd_marker.json";
await fs.writeText(MARKER, "x");

// ===================================================================
// 基础模式
// ===================================================================
console.log("\n=== 基础模式 ===");
check(
  "*.rs",
  eq(sort(await fs.glob("*.rs", cwd)), ["b.rs", "main.rs"]),
  JSON.stringify(sort(await fs.glob("*.rs", cwd))),
);
check(
  "*.txt 不包含 .hidden.txt",
  eq(await fs.glob("*.txt", cwd), ["a.txt"]),
  JSON.stringify(await fs.glob("*.txt", cwd)),
);
check(
  "? 匹配单字符",
  eq(await fs.glob("?.rs", cwd), ["b.rs"]),
  JSON.stringify(await fs.glob("?.rs", cwd)),
);
check(
  "[ab] 字符类",
  eq(await fs.glob("[ab].txt", cwd), ["a.txt"]),
  JSON.stringify(await fs.glob("[ab].txt", cwd)),
);
check(
  "结果按字母序",
  eq(await fs.glob("*.rs", cwd), ["b.rs", "main.rs"]),
  JSON.stringify(await fs.glob("*.rs", cwd)),
);

// ===================================================================
// globstar ** 递归匹配
// ===================================================================
console.log("\n=== globstar ** ===");
const rsAll = ["b.rs", "main.rs", path.join("deep", "x", "y.rs"), path.join("src", "lib.rs"), path.join("src", "mod.rs")];
check(
  "**/*.rs 递归（默认排除隐藏）",
  eq(sort(await fs.glob("**/*.rs", cwd)), sort(rsAll)),
  JSON.stringify(sort(await fs.glob("**/*.rs", cwd))),
);
check(
  "src/*.rs",
  eq(await fs.glob("src/*.rs", cwd), [path.join("src", "lib.rs"), path.join("src", "mod.rs")]),
  JSON.stringify(await fs.glob("src/*.rs", cwd)),
);
check(
  "deep/*/*.rs 两级通配",
  eq(await fs.glob("deep/*/*.rs", cwd), [path.join("deep", "x", "y.rs")]),
  JSON.stringify(await fs.glob("deep/*/*.rs", cwd)),
);
check(
  "**/mod.rs",
  eq(await fs.glob("**/mod.rs", cwd), [path.join("src", "mod.rs")]),
  JSON.stringify(await fs.glob("**/mod.rs", cwd)),
);

// ===================================================================
// cwd 与绝对路径
// ===================================================================
console.log("\n=== cwd 与 absolute ===");
check(
  "cwd 默认相对返回",
  eq(sort(await fs.glob("*.rs", cwd)), sort(["b.rs", "main.rs"])),
);
check(
  "absolute: true 返回绝对路径",
  eq(sort(await fs.glob("*.rs", { ...cwd, absolute: true })),
    sort([path.join(Dir, "b.rs"), path.join(Dir, "main.rs")])),
  JSON.stringify(sort(await fs.glob("*.rs", { ...cwd, absolute: true }))),
);
check(
  "绝对模式 + absolute 结果一致",
  eq(sort(await fs.glob(path.join(Dir, "*.rs"), { absolute: true })),
    sort([path.join(Dir, "b.rs"), path.join(Dir, "main.rs")])),
);
check(
  "未指定 cwd 默认使用进程工作目录",
  (await fs.glob(MARKER, { absolute: true })).length === 1,
);
await fs.remove(MARKER).catch(() => { });

// ===================================================================
// dot — 隐藏文件
// ===================================================================
console.log("\n=== dot ===");
check(
  "dot=false（默认）排除隐藏",
  eq(sort(await fs.glob("**/*.rs", cwd)), sort(rsAll)),
);
const rsDot = [...rsAll, path.join("sub", ".hidden.rs")];
check(
  "dot=true 包含隐藏",
  eq(sort(await fs.glob("**/*.rs", { ...cwd, dot: true })), sort(rsDot)),
  JSON.stringify(sort(await fs.glob("**/*.rs", { ...cwd, dot: true }))),
);
check(
  "*.txt dot=true 包含 .hidden.txt",
  eq(sort(await fs.glob("*.txt", { ...cwd, dot: true })), [".hidden.txt", "a.txt"]),
);

// ===================================================================
// exclude — 排除
// ===================================================================
console.log("\n=== exclude ===");
check(
  "exclude 单个模式",
  eq(sort(await fs.glob("**/*.rs", { ...cwd, exclude: ["**/src/*"] })),
    sort(["b.rs", "main.rs", path.join("deep", "x", "y.rs")])),
  JSON.stringify(sort(await fs.glob("**/*.rs", { ...cwd, exclude: ["**/src/*"] }))),
);
check(
  "exclude 多个模式",
  eq(sort(await fs.glob("**/*.rs", { ...cwd, exclude: ["**/src/*", "**/deep/**/*.rs"] })),
    sort(["b.rs", "main.rs"])),
  JSON.stringify(sort(await fs.glob("**/*.rs", { ...cwd, exclude: ["**/src/*", "**/deep/**/*.rs"] }))),
);
check(
  "exclude 为空数组不排除",
  eq(sort(await fs.glob("*.rs", { ...cwd, exclude: [] })), sort(["b.rs", "main.rs"])),
);

// ===================================================================
// caseSensitive
// ===================================================================
console.log("\n=== caseSensitive ===");
check(
  "caseSensitive=false 忽略大小写",
  eq(sort(await fs.glob("*.RS", { ...cwd, caseSensitive: false })), sort(["b.rs", "main.rs"])),
  JSON.stringify(sort(await fs.glob("*.RS", { ...cwd, caseSensitive: false }))),
);
check(
  "caseSensitive=true（默认）区分大小写",
  eq(await fs.glob("*.RS", cwd), []),
  JSON.stringify(await fs.glob("*.RS", cwd)),
);

// ===================================================================
// 错误处理
// ===================================================================
console.log("\n=== 错误处理 ===");
let threw = false;
try {
  await fs.glob("src/[/", cwd);
} catch (e) {
  threw = true;
}
check("非法模式 reject", threw);

// ===================================================================
// 清理
// ===================================================================
await fs.removeDir(Dir, { recursive: true }).catch(() => { });

// ===================================================================
// 结果汇总
// ===================================================================
console.log(`\n  ✅ Pass: ${pass}  ❌ Fail: ${fail}  ${fail > 0 ? "" : "ALL TESTS PASSED"}`);
if (fail > 0) process.exit(1);
