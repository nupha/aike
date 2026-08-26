// sandbox 模块测试 —— 目录白名单、读写控制、子进程继承
//
// 运行：aike tests/sandbox.mjs （需在 aike/ 目录下执行）
//
// API 形状：cwd = 子进程工作目录（macOS 上同时进写白名单）；
// mounts = Record<宿主机路径, {target?, writable?}>，target 缺省为同路径挂载。
//
// 平台说明：
// - macOS (sandbox-exec)：读全局放开（profile 简化设计），控制点是「写白名单」；
//   mount 权限按宿主机 source 路径生效，target 不做路径重映射。
// - Linux (namespaces+bind mount)：mount 按 target 重映射，测试对 target 的断言
//   仅在 linux 下执行。
// - nanosandbox 在 build() 时即校验 mount source 存在，故先建目录再 new Sandbox。

import * as fs from "fs";
import { Sandbox, isSupported, platformName } from "sandbox";

const IS_MACOS = platformName() === "macos";
const IS_LINUX = platformName() === "linux";

let passed = 0;
let failed = 0;

function check(name, cond) {
  if (cond) {
    passed++;
    console.log(`  ✅ ${name}`);
  } else {
    failed++;
    console.log(`  ❌ ${name}`);
  }
}

async function run(sb, cmd, args, opts) {
  return sb.run(cmd, args, opts);
}

// ---------------------------------------------------------------------------
// 准备目录（必须在 new Sandbox 之前 —— build() 会校验 source 存在）
// ---------------------------------------------------------------------------

const ROOT = `${process.cwd()}/target/sandbox-test`;
await fs.remove(ROOT).catch(() => {});
await fs.makeDir(`${ROOT}/wd`, { recursive: true });
await fs.makeDir(`${ROOT}/data`, { recursive: true });
await fs.makeDir(`${ROOT}/rw`, { recursive: true });
await fs.writeText(`${ROOT}/data/secret.txt`, "SECRET-CONTENT");

const FORBIDDEN = `${ROOT}/forbidden.txt`;
const HOME_PROBE = `${process.env.HOME}/.sbx-test-forbidden`;

// 共享配置：cwd 为工作目录（唯一常规可写区），data 只读挂载，rw 读写挂载
function makeSandbox() {
  return new Sandbox({
    cwd: `${ROOT}/wd`,
    mounts: {
      [`${ROOT}/data`]: { target: "/mnt-data" }, // ro + 重映射
      [`${ROOT}/rw`]: { target: "/mnt-rw", writable: true },
    },
  });
}

console.log("\n=== 平台 ===");
check(`isSupported() 为 true (${platformName()})`, isSupported());

// ---------------------------------------------------------------------------
console.log("\n=== 写白名单：默认拒绝 ===");
{
  const sb = makeSandbox();

  const r1 = await run(sb, "sh", ["-c", "echo ok > ./allowed.txt"]);
  check(
    "cwd 内写入成功",
    r1.exitCode === 0 && (await fs.exists(`${ROOT}/wd/allowed.txt`)),
  );

  const r2 = await run(sb, "sh", ["-c", `echo x > ${FORBIDDEN}`]);
  check(
    "白名单外（scratch 根）写入被拒",
    r2.exitCode !== 0 && !(await fs.exists(FORBIDDEN)),
  );

  const r3 = await run(sb, "sh", ["-c", `echo x > ${HOME_PROBE}`]);
  check("$HOME 写入被拒", r3.exitCode !== 0 && !(await fs.exists(HOME_PROBE)));

  if (IS_MACOS) {
    const r4 = await run(sb, "sh", ["-c", "echo x > /tmp/sbx-test-allow.txt"]);
    check("/tmp 可写（macOS 策略内建放行）", r4.exitCode === 0);
  }

  // macOS profile 的设计取舍：全局可读，控制点只在写
  if (IS_MACOS) {
    const r5 = await run(sb, "cat", ["/etc/hosts"]);
    check(
      "白名单外可读（macOS 全局读设计）",
      r5.exitCode === 0 && r5.stdout.length > 0,
    );
  }

  const r6 = await run(sb, "cat", [`${ROOT}/data/secret.txt`]);
  check("读取 ro 挂载内容正确", r6.stdout.trim() === "SECRET-CONTENT");
}

// ---------------------------------------------------------------------------
console.log("\n=== ro 挂载：只读 ===");
{
  const sb = makeSandbox();

  const r1 = await run(sb, "cat", [`${ROOT}/data/secret.txt`]);
  check(
    "ro source 可读",
    r1.exitCode === 0 && r1.stdout.trim() === "SECRET-CONTENT",
  );

  const r2 = await run(sb, "sh", [
    "-c",
    `echo hack >> ${ROOT}/data/secret.txt`,
  ]);
  check("ro source 写入被拒", r2.exitCode !== 0);

  const after = await fs.readText(`${ROOT}/data/secret.txt`);
  check("ro source 内容未被篡改", after === "SECRET-CONTENT");

  if (IS_LINUX) {
    const r3 = await run(sb, "cat", ["/mnt-data/secret.txt"]);
    check(
      "linux: target 路径重映射可见",
      r3.exitCode === 0 && r3.stdout.trim() === "SECRET-CONTENT",
    );
  }
}

// ---------------------------------------------------------------------------
console.log("\n=== rw 挂载：读写 ===");
{
  const sb = makeSandbox();

  const r1 = await run(sb, "sh", ["-c", `echo RW-DATA > ${ROOT}/rw/out.txt`]);
  check("rw source 写入成功", r1.exitCode === 0);

  const content = await fs.readText(`${ROOT}/rw/out.txt`);
  check("宿主机读到写入内容", content.trim() === "RW-DATA");

  const r2 = await run(sb, "sh", ["-c", `echo x > ${ROOT}/rw/nested/deep.txt`]);
  check(
    "rw 内创建多级目录受限一致",
    r2.exitCode !== 0 || (await fs.exists(`${ROOT}/rw/nested/deep.txt`)),
  );
}

// ---------------------------------------------------------------------------
console.log("\n=== 子进程继承：spawn 出的程序同样受控 ===");
{
  const sb = makeSandbox();

  // 1) 直接子进程写白名单外
  const c1 = await run(sb, "sh", ["-c", `echo x > ${FORBIDDEN}`]);
  check(
    "子进程写白名单外被拒",
    c1.exitCode !== 0 && !(await fs.exists(FORBIDDEN)),
  );

  // 2) 孙进程（sh 里再 sh）写白名单外
  const c2 = await run(sb, "sh", ["-c", `sh -c 'echo x > ${FORBIDDEN}'`]);
  check(
    "孙进程写白名单外被拒",
    c2.exitCode !== 0 && !(await fs.exists(FORBIDDEN)),
  );

  // 3) 后台孙进程（& + wait）写白名单外 —— 证明限制随进程树传递，
  //    且不受父进程退出影响。POSIX `wait`（无参数）恒返回 0，
  //    故以内层命令输出的 DENIED/ESCAPED 标记为准。
  const c3 = await run(sb, "sh", [
    "-c",
    `sh -c 'if echo x > ${FORBIDDEN} 2>/dev/null; then echo ESCAPED; else echo DENIED; fi' & wait`,
  ]);
  check(
    "后台孙进程写白名单外被拒",
    c3.stdout.includes("DENIED") && !(await fs.exists(FORBIDDEN)),
  );

  // 4) 阳性对照：同样的嵌套结构写白名单内应成功（排除命令本身的问题）
  const c4 = await run(sb, "sh", [
    "-c",
    `sh -c 'echo INHERITED-OK > ${ROOT}/rw/inherit.txt' & wait`,
  ]);
  const inherited =
    c4.exitCode === 0 && (await fs.exists(`${ROOT}/rw/inherit.txt`));
  check("阳性对照：后台孙进程写白名单内成功", inherited);

  // 5) 后台孙进程尝试篡改 ro 挂载源
  const c5 = await run(sb, "sh", [
    "-c",
    `sh -c 'if echo hack >> ${ROOT}/data/secret.txt 2>/dev/null; then echo ESCAPED; else echo DENIED; fi' & wait`,
  ]);
  const secret = await fs.readText(`${ROOT}/data/secret.txt`);
  check(
    "后台孙进程篡改 ro 源被拒",
    c5.stdout.includes("DENIED") && secret === "SECRET-CONTENT",
  );

  if (IS_LINUX) {
    // linux 下 target 重映射后，后代经 target 路径写入也应落在宿主机 source
    const c6 = await run(sb, "sh", [
      "-c",
      "sh -c 'echo VIA-TARGET > /mnt-rw/tgt.txt' & wait",
    ]);
    const viaTarget =
      c6.exitCode === 0 &&
      (await fs.readText(`${ROOT}/rw/tgt.txt`).catch(() => ""));
    check(
      "linux: 后台孙进程经 target 写入落盘",
      viaTarget.trim() === "VIA-TARGET",
    );
  }
}

// ---------------------------------------------------------------------------
console.log("\n=== 构造校验：cwd 必填且必须是存在的目录 ===");
{
  const expectThrow = (name, fn) => {
    try {
      fn();
      check(`${name}`, false);
    } catch (e) {
      check(
        `${name} → ${e.constructor.name}: ${String(e.message).slice(0, 48)}`,
        true,
      );
    }
  };
  expectThrow("无参数被拒", () => new Sandbox());
  expectThrow("缺 cwd 被拒", () => new Sandbox({}));
  expectThrow("cwd 不存在被拒", () => new Sandbox({ cwd: "/no/such/dir" }));
  expectThrow("cwd 指向文件被拒", () =>
    new Sandbox({ cwd: `${ROOT}/data/secret.txt` }),
  );
}

// ---------------------------------------------------------------------------
console.log("\n=== 清理与结果 ===");
await fs.remove(ROOT).catch(() => {});
await fs.remove(HOME_PROBE).catch(() => {});

console.log(`\n=== 结果: ${passed} passed, ${failed} failed ===`);
if (failed > 0) process.exit(1);
