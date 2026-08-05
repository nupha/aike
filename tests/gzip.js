// 🗜️ Aike.gzip / Aike.gunzip — 字节级 gzip 压缩工具测试
//
// Import-free: runs in any JS environment with the Aike namespace.
// 非 WHATWG 标准（标准为 CompressionStream/DecompressionStream），
// aike 增强便捷函数（memo §5.1）。Contract:
//   - Aike.gzip(data)   →  RFC 1952 gzip member（magic 1f 8b, deflate）
//   - Aike.gunzip(data) →  解出原始字节；非法 gzip 数据抛 RangeError
//   - 非 BufferSource（非 Uint8Array/ArrayBuffer）抛 TypeError

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

function bytesEq(a, b) {
  if (!(a instanceof Uint8Array) || !(b instanceof Uint8Array)) return false;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function textToBytes(s) {
  return new TextEncoder().encode(s);
}

function textFromBytes(u8) {
  return new TextDecoder().decode(u8);
}

// 固定已知向量："hello world\n" 的 gzip（mtime=0, XFL=0, OS=0xff），来自
// zlib 测试语料，用于验证 gunzip 能解出与 flate2 无关的外部标准 gzip 流。
const KNOWN_GZIP_HELLO =
  "1f8b08000000000000ffcb48cdc9c95728cf2fca49e102002d3b08af0c000000";

// ===================================================================
// 1. API shape
// ===================================================================

function testShape() {
  console.log("\n=== Aike.gzip / Aike.gunzip — shape ===");

  check("Aike.gzip is a function", typeof Aike.gzip === "function");
  check("Aike.gunzip is a function", typeof Aike.gunzip === "function");
  check("Aike.gzip.length === 1", Aike.gzip.length === 1, `got ${Aike.gzip.length}`);
  check("Aike.gunzip.length === 1", Aike.gunzip.length === 1, `got ${Aike.gunzip.length}`);
  check("not a WHATWG global (per spec)", typeof globalThis.gzip === "undefined");
}

// ===================================================================
// 2. gzip produces a valid RFC 1952 gzip stream
// ===================================================================

function testGzipFormat() {
  console.log("\n=== Aike.gzip — format ===");

  const gz = Aike.gzip(textToBytes("hello world\n"));
  check("returns Uint8Array", gz instanceof Uint8Array);
  check("magic bytes 1f 8b", gz[0] === 0x1f && gz[1] === 0x8b, `got ${gz[0]}, ${gz[1]}`);
  check("deflate method (08)", gz[2] === 0x08, `got ${gz[2].toString(16)}`);

  // gzip of a fixed input is byte-for-byte stable (deterministic).
  const gz2 = Aike.gzip(textToBytes("hello world\n"));
  check("deterministic output", bytesEq(gz, gz2), "");
  check(
    "matches known RFC 1952 vector",
    hexToBytes(KNOWN_GZIP_HELLO) instanceof Uint8Array &&
      bytesEq(gz, hexToBytes(KNOWN_GZIP_HELLO)),
    `got ${bytesToHex(gz)}`,
  );

  const empty = Aike.gzip(new Uint8Array(0));
  check("empty input still a gzip member", empty.length === 20, `got length ${empty.length}`);
  check("empty member magic", empty[0] === 0x1f && empty[1] === 0x8b, "");

  const ab = new Uint8Array([1, 2, 3]).buffer;
  check("accepts ArrayBuffer", Aike.gzip(ab) instanceof Uint8Array);
}

// ===================================================================
// 3. gzip/gunzip round-trip
// ===================================================================

function testRoundTrip() {
  console.log("\n=== Aike.gzip ↔ Aike.gunzip round-trip ===");

  const cases = [
    ["empty", new Uint8Array(0)],
    ["text", textToBytes("hello world\n")],
    ["all bytes 0..255", Uint8Array.from({ length: 256 }, (_, i) => i)],
    ["compressible run", new Uint8Array(10000).fill(0x61)], // 'a' x 10000
    ["binary-ish", Uint8Array.from([0, 255, 1, 254, 128, 64, 32, 16])],
  ];

  for (const [label, data] of cases) {
    const back = Aike.gunzip(Aike.gzip(data));
    check(`${label} round-trips`, bytesEq(back, data), `got length ${back?.length}`);
  }

  // gunzip can decode the fixed external vector.
  check(
    "gunzip decodes known vector",
    textFromBytes(Aike.gunzip(hexToBytes(KNOWN_GZIP_HELLO))) === "hello world\n",
    `got ${textFromBytes(Aike.gunzip(hexToBytes(KNOWN_GZIP_HELLO)))}`,
  );

  // Compression actually compresses.
  const big = new Uint8Array(10000).fill(0x61);
  const small = Aike.gzip(big);
  check("compresses compressible data", small.length < big.length, `gzip size ${small.length}`);
}

// ===================================================================
// 4. strict type validation
// ===================================================================

function testTypeErrors() {
  console.log("\n=== Aike.gzip / Aike.gunzip — type errors ===");

  const bad = [undefined, null, "abc", 42, {}, true];
  for (const v of bad) {
    let gzType = false;
    try {
      Aike.gzip(v);
    } catch (e) {
      gzType = e instanceof TypeError;
    }
    let unType = false;
    try {
      Aike.gunzip(v);
    } catch (e) {
      unType = e instanceof TypeError;
    }
    const t = v === null ? "null" : typeof v;
    check(`Aike.gzip(${t}) throws TypeError`, gzType);
    check(`Aike.gunzip(${t}) throws TypeError`, unType);
  }
}

// ===================================================================
// 5. gunzip rejects invalid gzip data
// ===================================================================

function testGunzipInvalid() {
  console.log("\n=== Aike.gunzip — invalid data ===");

  const invalid = [
    "empty bytes",
    new Uint8Array(0),
    "not gzip at all",
    new TextEncoder().encode("this is not gzip data"),
    "random bytes",
    new Uint8Array([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]),
    "gzip magic but truncated",
    new Uint8Array([0x1f, 0x8b, 0x08]),
    "truncated real gzip",
    Aike.gzip(textToBytes("hello")).subarray(0, 15),
  ];

  for (let i = 0; i < invalid.length; i += 2) {
    const label = invalid[i];
    const data = invalid[i + 1];
    let isRange = false;
    try {
      Aike.gunzip(data);
    } catch (e) {
      isRange = e instanceof RangeError;
    }
    check(`gunzip rejects ${label}`, isRange);
  }

  check(
    "gunzip result is Uint8Array",
    Aike.gunzip(Aike.gzip(textToBytes("x"))) instanceof Uint8Array,
  );
}

// ===================================================================
// Runner
// ===================================================================

testShape();
testGzipFormat();
testRoundTrip();
testTypeErrors();
testGunzipInvalid();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
