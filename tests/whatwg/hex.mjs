// 🧬 WHATWG-style hex encoding — bytesToHex / hexToBytes
//
// Import-free: runs in any JS environment with the hex globals.
// Contract: bytesToHex encodes bytes to lowercase hex; hexToBytes decodes
// (both cases) back to a Uint8Array, throwing TypeError on a non-string and
// RangeError on odd length or non-hex characters.

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

function bytesToArr(u8) {
  return Array.from(u8);
}

// ===================================================================
// 1. bytesToHex basics
// ===================================================================

function testBytesToHex() {
  console.log("\n=== bytesToHex ===");

  check("empty", bytesToHex(new Uint8Array([])) === "", `got ${bytesToHex(new Uint8Array([]))}`);
  check(
    "single byte",
    bytesToHex(new Uint8Array([255])) === "ff",
    `got ${bytesToHex(new Uint8Array([255]))}`,
  );
  check(
    "vector [0,1,2,16,255]",
    bytesToHex(new Uint8Array([0, 1, 2, 16, 255])) === "00010210ff",
    `got ${bytesToHex(new Uint8Array([0, 1, 2, 16, 255]))}`,
  );
  check(
    "lowercase output",
    bytesToHex(new Uint8Array([0xab, 0xcd, 0xef])) === "abcdef",
    `got ${bytesToHex(new Uint8Array([0xab, 0xcd, 0xef]))}`,
  );

  const ab = new Uint8Array([0x1a, 0x2b]).buffer;
  check("accepts ArrayBuffer", bytesToHex(ab) === "1a2b", `got ${bytesToHex(ab)}`);

  check("function exists", typeof bytesToHex === "function");
}

// ===================================================================
// 2. bytesToHex strict type validation
// ===================================================================

function testBytesToHexErrors() {
  console.log("\n=== bytesToHex — type errors ===");

  for (const v of [undefined, null, "abc", 42, {}, true]) {
    let threw = false;
    try {
      bytesToHex(v);
    } catch (e) {
      threw = e instanceof TypeError;
    }
    check(`bytesToHex(${v === null ? "null" : typeof v}) throws TypeError`, threw);
  }
}

// ===================================================================
// 3. hexToBytes basics
// ===================================================================

function testHexToBytes() {
  console.log("\n=== hexToBytes ===");

  const empty = hexToBytes("");
  check("empty -> empty Uint8Array", bytesToArr(empty).length === 0, `got ${bytesToArr(empty)}`);

  const one = hexToBytes("ff");
  check("single byte", bytesToArr(one).join(",") === "255", `got ${bytesToArr(one)}`);

  const lower = hexToBytes("00010210ff");
  check(
    "lowercase decode",
    bytesToArr(lower).join(",") === "0,1,2,16,255",
    `got ${bytesToArr(lower)}`,
  );

  const upper = hexToBytes("00FF10AB");
  check(
    "uppercase decode",
    bytesToArr(upper).join(",") === "0,255,16,171",
    `got ${bytesToArr(upper)}`,
  );

  const mixed = hexToBytes("DeAdBeEf");
  check(
    "mixed-case decode",
    bytesToArr(mixed).join(",") === "222,173,190,239",
    `got ${bytesToArr(mixed)}`,
  );

  check("returns Uint8Array", hexToBytes("00") instanceof Uint8Array);
  check("function exists", typeof hexToBytes === "function");
}

// ===================================================================
// 4. hexToBytes strict validation
// ===================================================================

function testHexToBytesErrors() {
  console.log("\n=== hexToBytes — errors ===");

  for (const v of [undefined, null, 42, true, {}, new Uint8Array([1])]) {
    let threw = false;
    try {
      hexToBytes(v);
    } catch (e) {
      threw = e instanceof TypeError;
    }
    check(`hexToBytes(${v === null ? "null" : typeof v}) throws TypeError`, threw);
  }

  for (const bad of ["0", "abc", "zz", "00gg", "0x12"]) {
    let isRange = false;
    try {
      hexToBytes(bad);
    } catch (e) {
      isRange = e instanceof RangeError;
    }
    check(`hexToBytes("${bad}") throws RangeError`, isRange);
  }
}

// ===================================================================
// 5. round-trip
// ===================================================================

function testRoundTrip() {
  console.log("\n=== round-trip ===");

  const data = new Uint8Array([0, 15, 16, 127, 128, 200, 255]);
  const rt = hexToBytes(bytesToHex(data));
  check(
    "hexToBytes(bytesToHex(x)) === x",
    bytesToArr(rt).join(",") === bytesToArr(data).join(","),
    `got ${bytesToArr(rt)}`,
  );
}

// ===================================================================
// Runner
// ===================================================================

testBytesToHex();
testBytesToHexErrors();
testHexToBytes();
testHexToBytesErrors();
testRoundTrip();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
