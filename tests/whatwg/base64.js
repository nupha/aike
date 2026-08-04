// 🔐 WHATWG Base64 — btoa / atob / bytesToBase64 / base64ToBytes 标准兼容测试
//
// Import-free: runs in any JS environment with the base64 globals.
// `atob`/`base64ToBytes` implement WHATWG forgiving-base64 (whitespace
// ignored, unpadded input accepted, `=` padding validated); `btoa` throws
// InvalidCharacterError on code points > 0xFF.

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

// ===================================================================
// 1. btoa basics
// ===================================================================

function testBtoaBasics() {
  console.log("\n=== btoa Basics ===");

  check("btoa('hello')", btoa("hello") === "aGVsbG8=", `got ${btoa("hello")}`);
  check("btoa('')", btoa("") === "", `got ${btoa("")}`);
  check("btoa single byte", btoa("A") === "QQ==", `got ${btoa("A")}`);
  check("btoa two bytes", btoa("ab") === "YWI=", `got ${btoa("ab")}`);
  check("btoa three bytes", btoa("abc") === "YWJj", `got ${btoa("abc")}`);

  // RFC 4648 test vectors
  check(
    "vector: 'foobar'",
    btoa("foobar") === "Zm9vYmFy",
    `got ${btoa("foobar")}`,
  );
}

// ===================================================================
// 2. atob basics + forgiving-base64
// ===================================================================

function testAtobBasics() {
  console.log("\n=== atob Basics ===");

  check("atob('aGVsbG8=')", atob("aGVsbG8=") === "hello", `got ${atob("aGVsbG8=")}`);
  check("atob('')", atob("") === "", `got ${atob("")}`);
  check(
    "atob unpadded accepted",
    atob("aGVsbG8") === "hello",
    `got ${atob("aGVsbG8")}`,
  );
  check(
    "atob 'a' (YQ==)",
    atob("YQ==") === "a",
    `got ${atob("YQ==")}`,
  );

  // Whitespace stripping (WHATWG step 1)
  check(
    "atob strips whitespace",
    atob(" Y Q = = ") === "a",
    `got ${atob(" Y Q = = ")}`,
  );
}

// ===================================================================
// 3. atob / btoa roundtrip and error cases
// ===================================================================

function testAtobErrors() {
  console.log("\n=== atob Error Cases ===");

  const throwsInvalid = (input, label) => {
    let threw = false;
    let name = "";
    try {
      atob(input);
    } catch (e) {
      threw = true;
      name = e.name;
    }
    check(label, threw && name === "InvalidCharacterError", `threw=${name}`);
  };

  throwsInvalid("!!!!", "invalid characters rejected");
  throwsInvalid("a", "length mod 4 == 1 rejected");
  throwsInvalid("YQ==bQ==", "mid-string padding rejected");
  throwsInvalid("ab===", "excess padding rejected");

  // Latin-1 roundtrip through btoa/atob
  const latin = "caf\u00e9 \u00fcber";
  check(
    "latin-1 roundtrip",
    atob(btoa(latin)) === latin,
    `got ${atob(btoa(latin))}`,
  );

  // btoa rejects non-Latin-1
  let threw = false;
  let name = "";
  try {
    btoa("\u4f60\u597d");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  check("btoa throws on >0xFF", threw && name === "InvalidCharacterError", `threw=${name}`);
}

// ===================================================================
// 4. bytesToBase64 / base64ToBytes
// ===================================================================

function testBytesBase64() {
  console.log("\n=== bytesToBase64 / base64ToBytes ===");

  check(
    "bytesToBase64([104,105])",
    bytesToBase64(new Uint8Array([104, 105])) === "aGk=",
    `got ${bytesToBase64(new Uint8Array([104, 105]))}`,
  );
  check(
    "bytesToBase64 empty",
    bytesToBase64(new Uint8Array([])) === "",
    `got ${bytesToBase64(new Uint8Array([]))}`,
  );
  check(
    "base64ToBytes('aGVsbG8=')",
    new TextDecoder().decode(base64ToBytes("aGVsbG8=")) === "hello",
    "",
  );
  check(
    "base64ToBytes unpadded",
    new TextDecoder().decode(base64ToBytes("aGVsbG8")) === "hello",
    "",
  );

  // Full byte range 0..255 roundtrips exactly
  const all = new Uint8Array(256);
  for (let i = 0; i < 256; i++) all[i] = i;
  const back = base64ToBytes(bytesToBase64(all));
  check(
    "full 0..255 roundtrip",
    back.length === 256 && back.every((v, i) => v === i),
    `len=${back.length}`,
  );

  // Returns a real Uint8Array
  check(
    "base64ToBytes returns Uint8Array",
    base64ToBytes("YQ==") instanceof Uint8Array,
    typeof base64ToBytes("YQ=="),
  );

  // Unicode roundtrip via TextEncoder/TextDecoder
  const text = "\u4f60\u597d world";
  check(
    "unicode roundtrip",
    new TextDecoder().decode(
      base64ToBytes(bytesToBase64(new TextEncoder().encode(text))),
    ) === text,
    "",
  );

  // Invalid input throws InvalidCharacterError
  let threw = false;
  let name = "";
  try {
    base64ToBytes("!!");
  } catch (e) {
    threw = true;
    name = e.name;
  }
  check("base64ToBytes rejects invalid", threw && name === "InvalidCharacterError", `threw=${name}`);
}

// ===================================================================
// Runner
// ===================================================================

function runAll() {
  testBtoaBasics();
  testAtobBasics();
  testAtobErrors();
  testBytesBase64();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
}

runAll();
