// 📥 Aike.download — URL 下载（文件 / 字节 / abort 取消）测试
//
// Import-free aside from the in-test HTTP server ("http" module).
// Contract (memo §5.4):
//   - download(url, destPath?) → Promise<string | Uint8Array>
//   - destPath 给 → 流式写盘（tmp + 原子替换），resolve 路径
//   - 省略 destPath → resolve body 字节
//   - options.signal：预中止立即 reject AbortError；in-flight 中止取消下载
//     并清理部分 tmp 文件

import { HttpServer } from "http";
import * as fs from "fs";

const IS_WIN = process.platform === "windows";
const PORT = 18777;
const BASE = `http://127.0.0.1:${PORT}`;

const PAYLOAD = "download test payload ".repeat(100);

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

const svr = new HttpServer();

svr.get("/data", (req, res) => {
  res.setHeader("content-type", "application/octet-stream");
  res.end(PAYLOAD);
});

svr.get("/missing", (req, res) => {
  res.sendStatus(404);
});

// 慢速流式：每 200ms 一帧，共 8 帧（约 1.6s），用于 in-flight abort。
svr.get("/slow", (req, res) => {
  const sse = res.sse();
  let i = 0;
  const timer = setInterval(() => {
    i += 1;
    sse.write("chunk " + i);
    if (i >= 8) {
      clearInterval(timer);
      sse.close();
    }
  }, 200);
});

svr.listen({ host: "127.0.0.1", port: PORT });
await Aike.sleep(50);

// ===================================================================
// 1. API shape
// ===================================================================

function testShape() {
  console.log("\n=== Aike.download — shape ===");

  check("download is a function", typeof Aike.download === "function");
  check("download.length === 2", Aike.download.length === 2, `got ${Aike.download.length}`);
}

// ===================================================================
// 2. download to file
// ===================================================================

async function testToFile() {
  console.log("\n=== Aike.download — to file ===");

  const dest = Aike.tmpdir() + "/aike-dl-test.txt";
  const resolved = await Aike.download(`${BASE}/data`, dest);
  check("resolves with the dest path", resolved === dest, `got ${resolved}`);

  const got = await fs.readText(dest);
  check("file content matches payload", got === PAYLOAD, `got ${got.length} bytes`);

  // 临时文件已通过原子替换消失
  check("no leftover tmp file", !(await fs.exists(dest + ".tmp")));
  await fs.remove(dest);
}

// ===================================================================
// 3. download to bytes (no destPath)
// ===================================================================

async function testToBytes() {
  console.log("\n=== Aike.download — bytes ===");

  const bytes = await Aike.download(`${BASE}/data`);
  check("resolves with Uint8Array", bytes instanceof Uint8Array);
  check("bytes content matches", new TextDecoder().decode(bytes) === PAYLOAD);
}

// ===================================================================
// 4. errors
// ===================================================================

async function testErrors() {
  console.log("\n=== Aike.download — errors ===");

  let rejected = false;
  try {
    await Aike.download(`${BASE}/missing`);
  } catch {
    rejected = true;
  }
  check("404 rejects", rejected);

  let typeErr = false;
  try {
    await Aike.download("not a valid url");
  } catch (e) {
    typeErr = e instanceof TypeError;
  }
  check("invalid url rejects TypeError", typeErr);

  // 同步参数校验
  for (const [label, fn] of [
    ["download(42)", () => Aike.download(42)],
    ["download(url, 42)", () => Aike.download(`${BASE}/data`, 42)],
    ["download(url, dest, 42)", () => Aike.download(`${BASE}/data`, "x", 42)],
    ["signal must be AbortSignal", () =>
      Aike.download(`${BASE}/data`, undefined, { signal: {} })],
  ]) {
    let threw = false;
    try {
      fn();
    } catch (e) {
      threw = e instanceof TypeError;
    }
    check(`${label} throws TypeError`, threw);
  }
}

// ===================================================================
// 5. pre-aborted signal
// ===================================================================

async function testPreAborted() {
  console.log("\n=== Aike.download — pre-aborted signal ===");

  const c = new AbortController();
  c.abort();
  let name;
  try {
    await Aike.download(`${BASE}/data`, undefined, { signal: c.signal });
  } catch (e) {
    name = e?.name;
  }
  check("rejects with AbortError", name === "AbortError", `got ${name}`);
}

// ===================================================================
// 6. in-flight abort (半途中断) — no partial file left behind
// ===================================================================

async function testInFlightAbort() {
  console.log("\n=== Aike.download — in-flight abort (mid-stream) ===");

  const dest = Aike.tmpdir() + "/aike-dl-slow.txt";
  const c = new AbortController();
  const p = Aike.download(`${BASE}/slow`, dest, { signal: c.signal });

  // 轮询到部分临时文件出现，证明流式写盘已在进行（真正的半途中断，
  // 而非"还没开始就 abort"）。
  let partialSeen = false;
  for (let i = 0; i < 50 && !partialSeen; i++) {
    partialSeen = await fs.exists(dest + ".tmp");
    if (!partialSeen) await Aike.sleep(100);
  }
  check("partial temp file exists before abort (mid-stream)", partialSeen);

  // 中止前 promise 仍未 settle（传输未完成）。
  let settled = false;
  p.then(() => { settled = true; }, () => { settled = true; });
  await Aike.sleep(50);
  check("download still pending before abort", settled === false);

  const start = Date.now();
  c.abort();

  let name;
  try {
    await p;
  } catch (e) {
    name = e?.name;
  }
  const elapsed = Date.now() - start;

  check("rejects with AbortError", name === "AbortError", `got ${name}`);
  check("abort rejects promptly", elapsed < 1500, `took ${elapsed}ms`);
  check("no partial dest file", !(await fs.exists(dest)));
  check("leftover tmp cleaned up", !(await fs.exists(dest + ".tmp")));
}

// ===================================================================
// 7. abort without destPath (bytes mode)
// ===================================================================

async function testAbortBytes() {
  console.log("\n=== Aike.download — abort without destPath ===");

  const c = new AbortController();
  const p = Aike.download(`${BASE}/slow`, undefined, { signal: c.signal });

  await Aike.sleep(300);
  c.abort();

  let name;
  try {
    await p;
  } catch (e) {
    name = e?.name;
  }
  check("bytes-mode abort rejects AbortError", name === "AbortError", `got ${name}`);
}

// ===================================================================
// Runner
// ===================================================================

testShape();
await testToFile();
await testToBytes();
await testErrors();
await testPreAborted();
await testInFlightAbort();
await testAbortBytes();

svr.shutdown();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
