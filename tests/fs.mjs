import * as fs from "fs";

// --- Helpers ---
let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  \u2705 ${label}`);
    pass++;
  } else {
    console.log(`  \u274c ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

// --- Config ---
const SAMPLE_JSON_FILE = "../models/tinyllama/tokenizer.json";

// ---------------------------------------------------------------------------
// FileHandle: open + text()
// ---------------------------------------------------------------------------
async function testOpenAndText() {
  console.log("\n=== open + text() ===");
  const fh = await fs.open(SAMPLE_JSON_FILE, "r");
  check("fd is number", typeof fh.fd === "number" && fh.fd > 0, `got ${fh.fd}`);
  const text = await fh.text();
  check("text length > 0", text.length > 0, `got ${text.length}`);
  check(
    "text starts with JSON",
    text.startsWith("{"),
    `got "${text.slice(0, 10)}"`,
  );
  await fh.close();
}

// ---------------------------------------------------------------------------
// FileHandle: open + bytes()
// ---------------------------------------------------------------------------
async function testOpenAndBytes() {
  console.log("\n=== open + bytes() ===");
  const fh = await fs.open(SAMPLE_JSON_FILE, "r");
  const buf = await fh.bytes();
  check("bytes byteLength > 0", buf.byteLength > 0, `got ${buf.byteLength}`);
  check("bytes is Uint8Array", buf instanceof Uint8Array);
  await fh.close();
}

// ---------------------------------------------------------------------------
// FileHandle: read() — read to EOF
// ---------------------------------------------------------------------------
async function testReadAll() {
  console.log("\n=== read() — read to EOF ===");
  const fh = await fs.open(SAMPLE_JSON_FILE, "r");
  const chunk = await fh.read();
  check(
    "read() byteLength > 0",
    chunk.byteLength > 0,
    `got ${chunk.byteLength}`,
  );
  check("read() returns Uint8Array", chunk instanceof Uint8Array);
  await fh.close();
}

// ---------------------------------------------------------------------------
// FileHandle: read(n) — read fixed size
// ---------------------------------------------------------------------------
async function testReadN() {
  console.log("\n=== read(64) ===");
  const fh = await fs.open(SAMPLE_JSON_FILE, "r");
  const head = await fh.read(64);
  check(
    "read(64) byteLength === 64",
    head.byteLength === 64,
    `got ${head.byteLength}`,
  );
  check(
    "read(64) starts with '{'",
    head[0] === 0x7b,
    `got 0x${head[0]?.toString(16)}`,
  );
  await fh.close();
}

// ---------------------------------------------------------------------------
// FileHandle: write(string) + read roundtrip
// ---------------------------------------------------------------------------
async function testWriteStringRoundtrip() {
  console.log("\n=== write(string) + read roundtrip ===");
  const EXPECTED_STR = "hello from FileHandle";
  const fhW = await fs.open("../tmp/test_fh_write.txt", "w");
  await fhW.write(EXPECTED_STR);
  await fhW.flush();
  await fhW.close();

  const fhR = await fs.open("../tmp/test_fh_write.txt", "r");
  const result = await fhR.text();
  check(
    "string roundtrip",
    result === EXPECTED_STR,
    `got ${JSON.stringify(result)}`,
  );
  await fhR.close();
}

// ---------------------------------------------------------------------------
// FileHandle: write(Uint8Array) + read roundtrip
// ---------------------------------------------------------------------------
async function testWriteBinaryRoundtrip() {
  console.log("\n=== write(Uint8Array) + read roundtrip ===");
  const EXPECTED_BIN = new Uint8Array([0x48, 0x45, 0x4c, 0x4c, 0x4f]);
  const fhW = await fs.open("../tmp/test_fh_write.bin", "w");
  await fhW.write(EXPECTED_BIN);
  await fhW.flush();
  await fhW.close();

  const fhR = await fs.open("../tmp/test_fh_write.bin", "r");
  const bin = await fhR.bytes();
  check(
    "binary length",
    bin.byteLength === EXPECTED_BIN.length,
    `got ${bin.byteLength}`,
  );
  check(
    "binary content",
    Array.from(bin).every((b, i) => b === EXPECTED_BIN[i]),
    `got [${Array.from(bin)}]`,
  );
  await fhR.close();
}

// ---------------------------------------------------------------------------
// FileHandle: stat()
// ---------------------------------------------------------------------------
async function testStat() {
  console.log("\n=== stat() ===");
  const fh = await fs.open(SAMPLE_JSON_FILE, "r");
  const info = await fh.stat();
  check("stat.size > 0", info.size > 0, `got ${info.size}`);
  check("stat.isFile === true", info.isFile === true, `got ${info.isFile}`);
  check("stat.modifiedAt > 0", info.modifiedAt > 0, `got ${info.modifiedAt}`);
  await fh.close();
}

// ---------------------------------------------------------------------------
// FileHandle: onData(callback) — callback-driven async read
// ---------------------------------------------------------------------------
async function testOnData() {
  console.log("\n=== onData(callback) ===");
  const CONTENT = "onData test data!";
  const fhW = await fs.open("../tmp/test_onread.txt", "w");
  await fhW.write(CONTENT);
  await fhW.flush();
  await fhW.close();

  const fhR = await fs.open("../tmp/test_onread.txt", "r");
  const chunks = [];
  let done = false;
  await new Promise((resolve) => {
    fhR.onData((chunk) => {
      if (chunk === null) {
        done = true;
        resolve();
      } else {
        chunks.push(chunk);
      }
    });
  });

  let total = "";
  for (const c of chunks) {
    total += String.fromCharCode(...c);
  }
  check(
    "onData received chunks",
    chunks.length > 0,
    `got ${chunks.length} chunks`,
  );
  check("onData got EOF", done);
  check(
    "onData content matches",
    total === CONTENT,
    `got ${JSON.stringify(total)}`,
  );
  await fhR.close();
}

// ---------------------------------------------------------------------------
// FileHandle: [Symbol.asyncIterator] — for await...of
// ---------------------------------------------------------------------------
async function testAsyncIterator() {
  console.log("\n=== for await...of iteration ===");
  const CONTENT = "async iterator test data!";
  const fhW = await fs.open("../tmp/test_async_iter.txt", "w");
  await fhW.write(CONTENT);
  await fhW.flush();
  await fhW.close();

  const fhR = await fs.open("../tmp/test_async_iter.txt", "r");
  const chunks = [];
  for await (const chunk of fhR) {
    chunks.push(chunk);
  }
  let total = "";
  for (const c of chunks) {
    total += String.fromCharCode(...c);
  }
  check(
    "for await received chunks",
    chunks.length > 0,
    `got ${chunks.length} chunks`,
  );
  check(
    "for await each chunk is Uint8Array",
    chunks.every((c) => c instanceof Uint8Array),
  );
  check(
    "for await content matches",
    total === CONTENT,
    `got ${JSON.stringify(total)}`,
  );
  await fhR.close();
}

// ---------------------------------------------------------------------------
// fs.exists()
// ---------------------------------------------------------------------------
async function testExists() {
  console.log("\n=== exists() ===");
  check("exists on real file", await fs.exists(SAMPLE_JSON_FILE));
  check("exists on missing file", !(await fs.exists("../nonexistent_file_xy")));
  check("exists on directory", await fs.exists("../tmp"));
  check("exists on missing dir", !(await fs.exists("../nonexistent_dir_xy")));
}

// ---------------------------------------------------------------------------
// fs.makeDir / fs.removeDir
// ---------------------------------------------------------------------------
async function testMakeDirRemoveDir() {
  console.log("\n=== makeDir / removeDir ===");
  const BASE = "../tmp/test_makeDir";
  const NESTED = BASE + "/nested";

  // Clean leftovers
  await fs.removeDir(BASE).catch(() => {});

  // makeDir non-recursive
  await fs.makeDir(BASE);
  check("makeDir creates dir", await fs.exists(BASE));
  const st = await fs.stat(BASE);
  check("makeDir dir isDirectory", st.isDirectory);

  // makeDir on existing fails
  await fs
    .makeDir(BASE)
    .then(() => check("makeDir on existing fails", false, "expected error"))
    .catch(() => check("makeDir on existing fails", true));

  // makeDir recursive
  await fs.makeDir(NESTED, { recursive: true });
  check("makeDir recursive creates nested", await fs.exists(NESTED));

  // makeDir non-recursive with missing parent fails
  await fs
    .makeDir("../tmp/test_makeDir_orphan/a/b")
    .then(() => check("makeDir no-parent fails", false, "expected error"))
    .catch(() => check("makeDir no-parent fails", true));

  // removeDir empty
  await fs.removeDir(NESTED);
  check("removeDir removes empty dir", !(await fs.exists(NESTED)));

  // removeDir non-empty fails
  await fs.makeDir(NESTED, { recursive: true });
  await fs.writeText(NESTED + "/dummy.txt", "x");
  await fs
    .removeDir(NESTED)
    .then(() => check("removeDir non-empty fails", false, "expected error"))
    .catch(() => check("removeDir non-empty fails", true));

  // Clean up
  await fs.remove(NESTED + "/dummy.txt");
  await fs.removeDir(NESTED);
  await fs.removeDir(BASE);
  check("cleanup after makeDir/removeDir", !(await fs.exists(BASE)));

  // removeDir recursive: removes a non-empty directory tree
  const RT = "../tmp/test_removeDir_recursive";
  await fs.removeDir(RT, { recursive: true }).catch(() => {});
  await fs.makeDir(RT + "/deep", { recursive: true });
  await fs.writeText(RT + "/deep/leaf.txt", "x");
  await fs.writeText(RT + "/top.txt", "y");
  await fs.removeDir(RT, { recursive: true });
  check("removeDir recursive removes non-empty tree", !(await fs.exists(RT)));
  await fs.removeDir(RT, { recursive: true }).catch(() => {});
}

// ---------------------------------------------------------------------------
// fs.remove (file unlink)
// ---------------------------------------------------------------------------
async function testRemove() {
  console.log("\n=== remove() ===");
  const FILE = "../tmp/test_remove.txt";
  await fs.remove(FILE).catch(() => {});
  await fs.writeText(FILE, "to be removed");
  check("file exists before remove", await fs.exists(FILE));
  await fs.remove(FILE);
  check("file gone after remove", !(await fs.exists(FILE)));

  // remove on non-existent fails
  await fs
    .remove("../tmp/nonexistent_remove_xy")
    .then(() => check("remove non-existent fails", false, "expected error"))
    .catch(() => check("remove non-existent fails", true));
}

// ---------------------------------------------------------------------------
// Sandbox restriction
// ---------------------------------------------------------------------------
async function testSandbox() {
  console.log("\n=== Sandbox restriction ===");
  check("sandbox default null", fs.getSandbox() === null, "sandbox is null");

  const SANDBOX_FILE = "../tmp/sandbox_test.txt";
  await fs.writeText(SANDBOX_FILE, "inside sandbox content");

  fs.setSandbox("../tmp");
  check(
    "getSandbox returns string after set",
    typeof fs.getSandbox() === "string",
    "sandbox is string",
  );
  check("getSandbox not null", fs.getSandbox() !== null, "sandbox is not null");

  const inside = await fs.readText(SANDBOX_FILE);
  check(
    "read inside sandbox",
    inside === "inside sandbox content",
    `got ${JSON.stringify(inside)}`,
  );

  try {
    await fs.readText(SAMPLE_JSON_FILE);
    check("outside sandbox blocked", false, "expected error to be thrown");
  } catch (e) {
    check("outside sandbox blocked", true, e.message);
  }

  fs.setSandbox(null);
  check("getSandbox null after clear", fs.getSandbox() === null);

  const afterClear = await fs.readText(SAMPLE_JSON_FILE);
  check(
    "read outside works after clear",
    afterClear.length > 0,
    `got ${afterClear.length}`,
  );
}

// ---------------------------------------------------------------------------
// Run all
// ---------------------------------------------------------------------------
await testOpenAndText();
await testOpenAndBytes();
await testReadAll();
await testReadN();
await testWriteStringRoundtrip();
await testWriteBinaryRoundtrip();
await testStat();
await testOnData();
await testAsyncIterator();
await testExists();
await testMakeDirRemoveDir();
await testRemove();
await testSandbox();

// Summary
console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
