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
const SAMPLE_JSON_FILE = "test_sample.json";

// --- Setup: ensure sample file and temp dir exist ---
const TmpDir = "../tmp";
if (!(await fs.exists(TmpDir))) {
  await fs.makeDir(TmpDir, { recursive: true });
}
if (!(await fs.exists(SAMPLE_JSON_FILE))) {
  await fs.writeText(
    SAMPLE_JSON_FILE,
    '{"x":1,"y":2,"z":3,"nested":{"key":"value"},"padding":"ensureFileLargerThan64Bytes"}'
  );
}

// ---------------------------------------------------------------------------
// FileHandle: open + text()
// ---------------------------------------------------------------------------
async function testOpenAndText() {
  console.log("\n=== open + text() ===");
  let fh;
  try {
    fh = await fs.open(SAMPLE_JSON_FILE, "r");
  } catch (e) {
    console.error(e.message);
    throw e;
  }

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
  await fs.removeDir(BASE).catch(() => { });

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
  await fs.removeDir(RT, { recursive: true }).catch(() => { });
  await fs.makeDir(RT + "/deep", { recursive: true });
  await fs.writeText(RT + "/deep/leaf.txt", "x");
  await fs.writeText(RT + "/top.txt", "y");
  await fs.removeDir(RT, { recursive: true });
  check("removeDir recursive removes non-empty tree", !(await fs.exists(RT)));
  await fs.removeDir(RT, { recursive: true }).catch(() => { });
}

// ---------------------------------------------------------------------------
// fs.remove (file unlink)
// ---------------------------------------------------------------------------
async function testRemove() {
  console.log("\n=== remove() ===");
  const FILE = "../tmp/test_remove.txt";
  await fs.remove(FILE).catch(() => { });
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
// fs line counting
// ---------------------------------------------------------------------------
async function testLineCounting() {
  console.log("\n=== Line counting (fs) ===");
  const c = "../tmp/jsonl_count_" + Date.now() + ".txt";
  await fs.writeText(c, "line1\nline2\n\nline3\n   \nline5\n");
  const all = await fs.countLines(c);
  check("fs.countLines counts all lines", all === 6, "all=" + all);
  const ne = await fs.countNonEmptyLines(c);
  check("fs.countNonEmptyLines skips blank lines", ne === 4, "ne=" + ne);

  // countLines / countNonEmptyLines on missing file resolve to 0 (no reject)
  const missing = "../tmp/jsonl_missing_" + Date.now() + ".jsonl";
  check(
    "fs.countLines missing file resolves 0",
    (await fs.countLines(missing)) === 0,
  );
  check(
    "fs.countNonEmptyLines missing file resolves 0",
    (await fs.countNonEmptyLines(missing)) === 0,
  );
}

// ---------------------------------------------------------------------------
// Edge-case: open non-existent file
// ---------------------------------------------------------------------------
async function testOpenNonexistent() {
  console.log("\n=== open non-existent ===");
  await fs
    .open("___does_not_exist___", "r")
    .then(() => check("open non-existent rejects", false, "expected error"))
    .catch(() => check("open non-existent rejects", true));
}

// ---------------------------------------------------------------------------
// Edge-case: write / append / readText on a fresh path
// ---------------------------------------------------------------------------
async function testWriteNewPath() {
  console.log("\n=== write & read fresh path ===");
  const f = "../tmp/test_fresh_" + Date.now() + ".txt";
  // Should create and write
  await fs.writeText(f, "fresh write");
  check("writeText created file", await fs.exists(f));
  const content = await fs.readText(f);
  check("readText after write", content === "fresh write", `got ${JSON.stringify(content)}`);
  // append
  await fs.appendText(f, " + appended");
  const appended = await fs.readText(f);
  check("appendText works", appended === "fresh write + appended", `got ${JSON.stringify(appended)}`);
  // cleanup
  await fs.remove(f);
  check("removed after test", !(await fs.exists(f)));
}

// ---------------------------------------------------------------------------
// fs.replaceTextUnique()
// ---------------------------------------------------------------------------
async function testReplaceTextUnique() {
  console.log("\n=== replaceTextUnique() ===");
  const F = "../tmp/test_replace.txt";
  const big = "../tmp/test_replace_big.bin";
  const MB = 1048576; // matches the internal streaming chunk size
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  // unique match -> replaced (returns count 1)
  await fs.writeText(F, "hello world hello");
  check(
    "unique match replaces",
    (await fs.replaceTextUnique(F, "world", "W O R L D")) === 1 &&
      (await fs.readText(F)) === "hello W O R L D hello",
  );

  // multiple matches -> 0, unchanged
  await fs.writeText(F, "aaa bbb aaa");
  check(
    "multiple matches -> 0",
    (await fs.replaceTextUnique(F, "aaa", "x")) === 0 &&
      (await fs.readText(F)) === "aaa bbb aaa",
  );

  // no match -> 0
  await fs.writeText(F, "abcdef");
  check(
    "no match -> 0",
    (await fs.replaceTextUnique(F, "zzz", "x")) === 0 &&
      (await fs.readText(F)) === "abcdef",
  );

  // overlapping matches count as >1 ("aa" in "aaa")
  await fs.writeText(F, "aaa");
  check(
    "overlapping matches -> 0",
    (await fs.replaceTextUnique(F, "aa", "x")) === 0 &&
      (await fs.readText(F)) === "aaa",
  );

  // missing file -> 0 (no reject)
  check(
    "missing file -> 0",
    (await fs.replaceTextUnique("../tmp/nonexistent_replace_xy", "a", "b")) === 0,
  );

  // empty oldText -> 0
  await fs.writeText(F, "abc");
  check(
    "empty oldText -> 0",
    (await fs.replaceTextUnique(F, "", "b")) === 0 &&
      (await fs.readText(F)) === "abc",
  );

  // oldText === replaceWith -> 0 (no-op)
  await fs.writeText(F, "abc");
  check(
    "no-op replacement -> 0",
    (await fs.replaceTextUnique(F, "abc", "abc")) === 0 &&
      (await fs.readText(F)) === "abc",
  );

  // unicode exact match
  await fs.writeText(F, "中文 测试 中文");
  check(
    "unicode match replaces",
    (await fs.replaceTextUnique(F, "测试", "替换")) === 1 &&
      (await fs.readText(F)) === "中文 替换 中文",
  );

  // multi-line replacement
  await fs.writeText(F, "line1\nTARGET\nline3\n");
  check(
    "multi-line replaces",
    (await fs.replaceTextUnique(F, "line1\nTARGET", "L1\nTGT")) === 1 &&
      (await fs.readText(F)) === "L1\nTGT\nline3\n",
  );

  // replacement larger than the needle
  await fs.writeText(F, "x[old]y");
  check(
    "larger replacement",
    (await fs.replaceTextUnique(F, "[old]", "[replacement text]")) === 1 &&
      (await fs.readText(F)) === "x[replacement text]y",
  );

  // needle at very start / end of file
  await fs.writeText(F, "START rest");
  check(
    "match at file start",
    (await fs.replaceTextUnique(F, "START", "BEGIN")) === 1 &&
      (await fs.readText(F)) === "BEGIN rest",
  );
  await fs.writeText(F, "rest END");
  check(
    "match at file end",
    (await fs.replaceTextUnique(F, "END", "FIN")) === 1 &&
      (await fs.readText(F)) === "rest FIN",
  );

  // needle straddling the 1 MiB streaming chunk boundary, unique
  const prefix = "x".repeat(MB - 3);
  await fs.writeBytes(big, enc.encode(prefix + "TARGET" + "y".repeat(10)));
  check(
    "match across chunk boundary",
    (await fs.replaceTextUnique(big, "TARGET", "DONE")) === 1 &&
      dec.decode(await fs.readBytes(big)) === prefix + "DONE" + "y".repeat(10),
  );

  // needle straddling the boundary but appears twice -> 0
  await fs.writeBytes(big, enc.encode(prefix + "TARGET" + "TARGET"));
  check(
    "duplicate across chunk boundary -> 0",
    (await fs.replaceTextUnique(big, "TARGET", "DONE")) === 0 &&
      dec.decode(await fs.readBytes(big)) === prefix + "TARGET" + "TARGET",
  );

  // overlapping matches straddling the boundary -> 0
  const m1 = MB - 2;
  await fs.writeBytes(big, enc.encode("b".repeat(m1) + "aaaaa" + "c".repeat(5)));
  check(
    "overlap across chunk boundary -> 0",
    (await fs.replaceTextUnique(big, "aaaa", "Z")) === 0 &&
      dec.decode(await fs.readBytes(big)) ===
        "b".repeat(m1) + "aaaaa" + "c".repeat(5),
  );

  // single-char needle in a multi-chunk file of identical bytes -> 0
  await fs.writeBytes(big, enc.encode("z".repeat(3 * MB)));
  check(
    "single-char needle in huge file -> 0",
    (await fs.replaceTextUnique(big, "z", "q")) === 0,
  );

  // cleanup
  await fs.remove(F).catch(() => { });
  await fs.remove(big).catch(() => { });
}

// ---------------------------------------------------------------------------
// fs.replaceTextAll()
// ---------------------------------------------------------------------------
async function testReplaceTextAll() {
  console.log("\n=== replaceTextAll() ===");
  const F = "../tmp/test_replace_all.txt";
  const big = "../tmp/test_replace_all_big.bin";
  const MB = 1048576;
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  // basic replace-all
  await fs.writeText(F, "hello world hello");
  check(
    "basic replace-all",
    (await fs.replaceTextAll(F, "hello", "hi")) === 2 &&
      (await fs.readText(F)) === "hi world hi",
  );

  // no match -> 0
  await fs.writeText(F, "abcdef");
  check(
    "no match -> 0",
    (await fs.replaceTextAll(F, "zzz", "x")) === 0 &&
      (await fs.readText(F)) === "abcdef",
  );

  // empty oldText -> 0
  await fs.writeText(F, "abc");
  check(
    "empty oldText -> 0",
    (await fs.replaceTextAll(F, "", "b")) === 0 &&
      (await fs.readText(F)) === "abc",
  );

  // no-op -> 0
  await fs.writeText(F, "abc");
  check(
    "no-op -> 0",
    (await fs.replaceTextAll(F, "abc", "abc")) === 0 &&
      (await fs.readText(F)) === "abc",
  );

  // single replacement
  await fs.writeText(F, "abc");
  check(
    "single replacement",
    (await fs.replaceTextAll(F, "b", "X")) === 1 &&
      (await fs.readText(F)) === "aXc",
  );

  // non-overlapping left-to-right ("aa" should only replace twice in "aaaa")
  await fs.writeText(F, "aaaa");
  check(
    "non-overlapping replace",
    (await fs.replaceTextAll(F, "aa", "b")) === 2 &&
      (await fs.readText(F)) === "bb",
  );

  // unicode
  await fs.writeText(F, "你好你好你好");
  check(
    "unicode replace-all",
    (await fs.replaceTextAll(F, "你好", "hello")) === 3 &&
      (await fs.readText(F)) === "hellohellohello",
  );

  // multi-line
  await fs.writeText(F, "a\na\na\n");
  check(
    "multi-line replace-all",
    (await fs.replaceTextAll(F, "a\n", "b\n")) === 3 &&
      (await fs.readText(F)) === "b\nb\nb\n",
  );

  // larger replacement
  await fs.writeText(F, "x[a]y[a]z");
  check(
    "larger replacement",
    (await fs.replaceTextAll(F, "[a]", "[REPL]")) === 2 &&
      (await fs.readText(F)) === "x[REPL]y[REPL]z",
  );

  // needle spanning chunk boundary
  const prefix = "x".repeat(MB - 2);
  const content = prefix + "ab" + "TARGET" + "y".repeat(1);
  await fs.writeBytes(big, enc.encode(content + content));
  const expected = prefix + "Z" + prefix + "Z";
  check(
    "replace-all across chunk boundary",
    (await fs.replaceTextAll(big, "abTARGETy", "Z")) === 2 &&
      dec.decode(await fs.readBytes(big)) === expected,
  );

  // missing file -> 0
  check(
    "missing file -> 0",
    (await fs.replaceTextAll("../tmp/nonexistent_replace_xy_all", "a", "b")) === 0,
  );

  // cleanup
  await fs.remove(F).catch(() => { });
  await fs.remove(big).catch(() => { });
}

// ---------------------------------------------------------------------------
// fs.readText / readBytes with { offset, length } options
// ---------------------------------------------------------------------------
async function testReadOptions() {
  console.log("\n=== readText/readBytes options ===");
  const F = "../tmp/test_read_opts.txt";
  // "0123456789abcdef" (16 bytes) + "中文" (中=bytes16-18, 文=bytes19-21) => 22 bytes
  const content = "0123456789abcdef" + "中文";
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  await fs.writeText(F, content);

  check("readBytes full", (await fs.readBytes(F)).byteLength === 22);
  check("readText full", (await fs.readText(F)) === content);

  check(
    "readBytes offset",
    dec.decode(await fs.readBytes(F, { offset: 5 })) === "56789abcdef中文",
  );
  check("readText offset", (await fs.readText(F, { offset: 5 })) === "56789abcdef中文");

  check(
    "readBytes length",
    dec.decode(await fs.readBytes(F, { length: 4 })) === "0123",
  );
  check("readText length", (await fs.readText(F, { length: 4 })) === "0123");

  check(
    "readBytes offset+length",
    dec.decode(await fs.readBytes(F, { offset: 2, length: 5 })) === "23456",
  );
  check("readText offset+length", (await fs.readText(F, { offset: 2, length: 5 })) === "23456");

  // length larger than remaining returns everything available
  check(
    "readBytes tail",
    dec.decode(await fs.readBytes(F, { offset: 19, length: 100 })) === "文",
  );
  check("readText tail", (await fs.readText(F, { offset: 19, length: 100 })) === "文");

  // offset past EOF -> empty
  check("readBytes offset beyond", (await fs.readBytes(F, { offset: 100 })).byteLength === 0);
  check("readText offset beyond", (await fs.readText(F, { offset: 100 })) === "");

  // offset splitting a multi-byte char: readBytes returns raw bytes,
  // readText throws because the range is not valid UTF-8
  const raw = await fs.readBytes(F, { offset: 20, length: 100 });
  check("readBytes split char raw", raw.byteLength === 2 && raw[0] === 0x96 && raw[1] === 0x87);
  let threw = false;
  try {
    await fs.readText(F, { offset: 20, length: 100 });
  } catch (e) {
    threw = true;
  }
  check("readText split char throws", threw);

  check("readText ascii range", (await fs.readText(F, { offset: 3, length: 4 })) === "3456");
  check("readText offset 0", (await fs.readText(F, { offset: 0 })) === content);
  check("readBytes length 0", (await fs.readBytes(F, { length: 0 })).byteLength === 0);
  check("readBytes negative clamps", (await fs.readBytes(F, { offset: -5, length: -1 })).byteLength === 0);
  check("readText empty options", (await fs.readText(F, {})) === content);

  await fs.remove(F).catch(() => { });
}

// ---------------------------------------------------------------------------
// Run all
// ---------------------------------------------------------------------------
const tests = [
  testOpenAndText, testOpenAndBytes, testReadAll, testReadN,
  testWriteStringRoundtrip, testWriteBinaryRoundtrip, testStat,
  testOnData, testAsyncIterator, testExists,
  testMakeDirRemoveDir, testRemove, testLineCounting,
  testOpenNonexistent, testWriteNewPath, testReplaceTextUnique,
  testReplaceTextAll, testReadOptions,
];
for (const t of tests) {
  try {
    await t();
  } catch (e) {
    console.log(`  ❌ ${t.name} threw:`, e?.message ?? String(e));
    fail++;
  }
}

// Summary
console.log(`\n${"=".repeat(40)}`);
console.log(`  Pass: ${pass}  Fail: ${fail}`);
if (fail > 0) {
  console.log("  SOME TESTS FAILED");
} else {
  console.log("  ALL TESTS PASSED");
}
