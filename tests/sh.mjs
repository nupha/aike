// 🐚 Aike.sh — 平台 shell 执行 + 捕获测试
//
// Only imports the built-in `os` module (for tmpdir); otherwise runs in any JS
// environment with the Aike namespace.
// 非 WHATWG 标准（memo §5.4）：命令字符串交给平台 shell 解释
// （POSIX `/bin/sh -c`，Windows `%COMSPEC%` i.e. cmd.exe /d /s /c），
// stdout/stderr 恒捕获并以 string 返回。

import * as os from "os";

const IS_WIN = process.platform === "win32";

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

// 归一化跨平台输出：Windows echo/cd 输出带 \r\n，路径大小写与尾分隔符各异。
function normPath(s) {
  let out = s.trim();
  out = out.replace(/[\\/]+$/, "");
  return IS_WIN ? out.toLowerCase() : out;
}

// ===================================================================
// 1. API shape
// ===================================================================

function testShape() {
  console.log("\n=== Aike.sh — shape ===");

  check("sh is a function", typeof Aike.sh === "function");
  check("sh.length === 1", Aike.sh.length === 1, `got ${Aike.sh.length}`);

  const r = Aike.sh("echo hello");
  check("returns an object", typeof r === "object" && r !== null);
  check("stdout is a string", typeof r.stdout === "string", `got ${typeof r.stdout}`);
  check("stderr is a string", typeof r.stderr === "string", `got ${typeof r.stderr}`);
  check("exitCode is a number", typeof r.exitCode === "number", `got ${typeof r.exitCode}`);
  check(
    "result keys are stdout/stderr/exitCode",
    Object.keys(r).sort().join(",") === "exitCode,stderr,stdout",
    `got ${Object.keys(r).sort()}`,
  );
}

// ===================================================================
// 2. basic output + shell syntax
// ===================================================================

function testBasic() {
  console.log("\n=== Aike.sh — output ===");

  const r = Aike.sh("echo hello");
  check("echo output captured", r.stdout.trim() === "hello", `got ${JSON.stringify(r.stdout)}`);
  check("exitCode 0 on success", r.exitCode === 0, `got ${r.exitCode}`);

  // Shell syntax: pipes work because the command runs through the shell.
  const piped = Aike.sh(IS_WIN ? "echo hello | findstr hello" : "echo hello | tr a-z A-Z");
  check("shell pipeline works", piped.stdout.trim() === (IS_WIN ? "hello" : "HELLO"), `got ${JSON.stringify(piped.stdout)}`);
  check("pipeline exitCode 0", piped.exitCode === 0, `got ${piped.exitCode}`);
}

// ===================================================================
// 3. exit code
// ===================================================================

function testExitCode() {
  console.log("\n=== Aike.sh — exit code ===");

  const r = Aike.sh(IS_WIN ? "exit /b 3" : "exit 3");
  check("captures non-zero exit code", r.exitCode === 3, `got ${r.exitCode}`);
}

// ===================================================================
// 4. check option
// ===================================================================

function testCheck() {
  console.log("\n=== Aike.sh — check option ===");

  let threw = false;
  let message = "";
  try {
    Aike.sh(IS_WIN ? "exit /b 5" : "exit 5", { check: true });
  } catch (e) {
    threw = true;
    message = e.message;
  }
  check("check throws on non-zero", threw);
  check("error message includes exit code", message.includes("5"), `got ${message}`);

  let ok = true;
  try {
    Aike.sh("echo ok", { check: true });
  } catch {
    ok = false;
  }
  check("check passes on success", ok);
}

// ===================================================================
// 5. env option
// ===================================================================

function testEnv() {
  console.log("\n=== Aike.sh — env option ===");

  const r = Aike.sh(IS_WIN ? "echo %FOO%" : "echo $FOO", { env: { FOO: "bar" } });
  check("env visible to shell", r.stdout.trim() === "bar", `got ${JSON.stringify(r.stdout)}`);
}

// ===================================================================
// 6. cwd option
// ===================================================================

function testCwd() {
  console.log("\n=== Aike.sh — cwd option ===");

  const tmp = os.tmpdir();
  const r = Aike.sh(IS_WIN ? "cd" : "pwd", { cwd: tmp });
  check("command runs in cwd", normPath(r.stdout) === normPath(tmp), `got ${JSON.stringify(r.stdout)} vs ${tmp}`);
}

// ===================================================================
// 7. input option
// ===================================================================

function testInput() {
  console.log("\n=== Aike.sh — input option ===");

  const r = Aike.sh(IS_WIN ? "more" : "cat", { input: "hello stdin" });
  check("input written to stdin", r.stdout.includes("hello stdin"), `got ${JSON.stringify(r.stdout)}`);
  check("input exitCode 0", r.exitCode === 0, `got ${r.exitCode}`);
}

// ===================================================================
// 8. timeout option
// ===================================================================

async function testTimeout() {
  console.log("\n=== Aike.sh — timeout option ===");

  const start = Date.now();
  const r = Aike.sh(IS_WIN ? "ping -n 3 127.0.0.1" : "sleep 2", { timeout: 300 });
  const elapsed = Date.now() - start;

  check("timeout kills promptly", elapsed < 1500, `took ${elapsed}ms`);
  check("timeout results in non-zero", r.exitCode !== 0, `got ${r.exitCode}`);
}

// ===================================================================
// 9. strict type validation
// ===================================================================

function testTypeErrors() {
  console.log("\n=== Aike.sh — type errors ===");

  for (const v of [undefined, null, 42, {}, true]) {
    let threw = false;
    try {
      Aike.sh(v);
    } catch (e) {
      threw = e instanceof TypeError;
    }
    check(`sh(${v === null ? "null" : typeof v}) throws TypeError`, threw);
  }

  let threw = false;
  try {
    Aike.sh("echo x", 42);
  } catch (e) {
    threw = e instanceof TypeError;
  }
  check("sh(cmd, non-object) throws TypeError", threw);

  let threwBool = false;
  try {
    Aike.sh("echo x", { check: "yes" });
  } catch (e) {
    threwBool = e instanceof TypeError;
  }
  check("sh(cmd, {check: non-bool}) throws TypeError", threwBool);
}

// ===================================================================
// Runner
// ===================================================================

testShape();
testBasic();
testExitCode();
testCheck();
testEnv();
testCwd();
testInput();
await testTimeout();
testTypeErrors();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
