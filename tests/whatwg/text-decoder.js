// 📝 WHATWG TextDecoder — Encoding 标准兼容测试
//
// Import-free: runs in any JS environment with global TextDecoder.

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
// 1. Basic encoding tests
// ===================================================================

function testEncodingBasics() {
  console.log("\n=== Encoding Basics ===");

  // UTF-8
  const d = new TextDecoder("utf-8");
  const r1 = d.decode(new Uint8Array([72, 101, 108, 108, 111]));
  check("UTF-8 'Hello'", r1 === "Hello", `got "${r1}"`);

  // Chinese characters
  const r2 = d.decode(new Uint8Array([228, 184, 173, 230, 150, 135]));
  check("UTF-8 '中文'", r2 === "\u4e2d\u6587", `got "${r2}"`);

  // Default encoding (UTF-8)
  const dd = new TextDecoder();
  const r3 = dd.decode(new Uint8Array([87, 111, 114, 108, 100]));
  check("default encoding 'World'", r3 === "World", `got "${r3}"`);

  // UTF-16LE
  const d16le = new TextDecoder("utf-16le");
  const r4 = d16le.decode(
    new Uint8Array([72, 0, 101, 0, 108, 0, 108, 0, 111, 0]),
  );
  check("UTF-16LE 'Hello'", r4 === "Hello", `got "${r4}"`);

  // UTF-16BE
  const d16be = new TextDecoder("utf-16be");
  const r5 = d16be.decode(
    new Uint8Array([0, 72, 0, 101, 0, 108, 0, 108, 0, 111]),
  );
  check("UTF-16BE 'Hello'", r5 === "Hello", `got "${r5}"`);

  // ISO-8859-1
  const dl1 = new TextDecoder("iso-8859-1");
  const r6 = dl1.decode(new Uint8Array([72, 101, 108, 108, 111]));
  check("ISO-8859-1 'Hello'", r6 === "Hello", `got "${r6}"`);

  // Empty input
  const r7 = d.decode(new Uint8Array([]));
  check("empty input", r7 === "", `got "${r7}"`);

  // Unsupported encoding
  try {
    new TextDecoder("unsupported-encoding");
    check("unsupported encoding error", false, "should have thrown");
  } catch (_) {
    check("unsupported encoding error", true);
  }
}

// ===================================================================
// 2. Input type tests
// ===================================================================

function testInputTypes() {
  console.log("\n=== Input Types ===");
  const d = new TextDecoder("utf-8");

  // Uint8Array
  const r1 = d.decode(new Uint8Array([72, 101, 108, 108, 111]));
  check("Uint8Array input", r1 === "Hello", `got "${r1}"`);

  // String input (returned as-is per implementation)
  const r2 = d.decode("Hello");
  check("string input", r2 === "Hello", `got "${r2}"`);

  // Empty string
  const r3 = d.decode("");
  check("empty string", r3 === "", `got "${r3}"`);

  // Chinese characters
  const r4 = d.decode(new Uint8Array([228, 184, 173, 230, 150, 135]));
  check("Chinese characters", r4 === "\u4e2d\u6587", `got "${r4}"`);

  // Emoji (U+1F600 = F0 9F 98 80 in UTF-8)
  const r5 = d.decode(new Uint8Array([0xf0, 0x9f, 0x98, 0x80]));
  check("emoji U+1F600", r5 === "\u{1F600}", `got "${r5}"`);

  // ArrayBuffer input
  const ab = new Uint8Array([72, 105]).buffer;
  const r6 = d.decode(ab);
  check("ArrayBuffer input", r6 === "Hi", `got "${r6}"`);
}

// ===================================================================
// 3. Property tests
// ===================================================================

function testProperties() {
  console.log("\n=== Properties ===");

  const d = new TextDecoder("utf-8");
  check("encoding 'utf-8'", d.encoding === "utf-8", `got ${d.encoding}`);
  check("fatal default false", d.fatal === false, `got ${d.fatal}`);
  check("ignoreBOM default false", d.ignoreBOM === false, `got ${d.ignoreBOM}`);

  const dFatal = new TextDecoder("utf-8", { fatal: true });
  check("fatal option true", dFatal.fatal === true, `got ${dFatal.fatal}`);

  const dIgnBom = new TextDecoder("utf-8", { ignoreBOM: true });
  check(
    "ignoreBOM option true",
    dIgnBom.ignoreBOM === true,
    `got ${dIgnBom.ignoreBOM}`,
  );

  const d16 = new TextDecoder("utf-16le");
  check(
    "encoding 'utf-16le'",
    d16.encoding === "utf-16le",
    `got ${d16.encoding}`,
  );
}

// ===================================================================
// 4. BOM handling tests
// ===================================================================

function testBom() {
  console.log("\n=== BOM Handling ===");

  const d = new TextDecoder("utf-8");
  // BOM + "Hello" = EF BB BF 48 65 6C 6C 6F
  const data = new Uint8Array([0xef, 0xbb, 0xbf, 72, 101, 108, 108, 111]);
  const r1 = d.decode(data);
  check("UTF-8 BOM stripped", r1 === "Hello", `got "${r1}"`);

  // Second call without BOM — still works
  const r2 = d.decode(new Uint8Array([87, 111]));
  check("second decode still works", r2 === "Wo", `got "${r2}"`);

  // ignoreBOM: true — BOM not stripped
  const dIgn = new TextDecoder("utf-8", { ignoreBOM: true });
  const r3 = dIgn.decode(data);
  check("ignoreBOM keeps BOM", r3 !== "Hello", `got "${r3}"`);
}

// ===================================================================
// 5. Stream decoding (pending multi-byte sequences)
// ===================================================================

function testStream() {
  console.log("\n=== Stream Decoding ===");

  // 5a. Complete single-byte chars with stream:true / stream:false
  let d = new TextDecoder();
  const r1 = d.decode(new Uint8Array([72, 101, 108]), { stream: true });
  check("stream 'Hel' chunk", r1 === "Hel", `got "${r1}"`);
  const r2 = d.decode(new Uint8Array([108, 111]), { stream: false });
  check("stream 'lo' flush", r2 === "lo", `got "${r2}"`);

  // 5b. Split a 3-byte UTF-8 char across two chunks
  d = new TextDecoder();
  const r3 = d.decode(new Uint8Array([0xe4, 0xb8]), { stream: true });
  check("stream partial 3-byte (pending)", r3 === "", `got "${r3}"`);
  const r4 = d.decode(new Uint8Array([0xad]), { stream: false });
  check("stream completed 3-byte", r4 === "\u4e2d", `got "${r4}"`);

  // 5c. Multiple chunks, mixed ASCII and multi-byte
  d = new TextDecoder();
  let acc = "";
  acc += d.decode(new Uint8Array([72, 101, 108, 108, 111]), { stream: true });
  acc += d.decode(new Uint8Array([32]), { stream: true });
  acc += d.decode(new Uint8Array([0xe4]), { stream: true });
  acc += d.decode(new Uint8Array([0xb8, 0x96]), { stream: false });
  check(
    "stream multi-chunk 'Hello \u4e16'",
    acc === "Hello \u4e16",
    `got "${acc}"`,
  );

  // 5d. Split across 3 chunks (4-byte char)
  d = new TextDecoder();
  const c1 = d.decode(new Uint8Array([0xf0]), { stream: true });
  const c2 = d.decode(new Uint8Array([0x9f, 0x98]), { stream: true });
  const c3 = d.decode(new Uint8Array([0x80]), { stream: false });
  check(
    "stream 4-byte char across 3 chunks",
    c1 + c2 + c3 === "\u{1F600}",
    `got "${c1 + c2 + c3}"`,
  );

  // 5e. Multiple stream:true calls without flush are transparent
  d = new TextDecoder();
  const s1 = d.decode(new Uint8Array([72, 101]), { stream: true });
  const s2 = d.decode(new Uint8Array([108, 108]), { stream: true });
  const s3 = d.decode(new Uint8Array([111]), { stream: false });
  check(
    "stream three chunks 'Hello'",
    s1 + s2 + s3 === "Hello",
    `got "${s1 + s2 + s3}"`,
  );

  // 5f. UTF-16LE streaming: split a 2-byte unit
  d = new TextDecoder("utf-16le");
  const u1 = d.decode(new Uint8Array([72, 0]), { stream: true });
  const u2 = d.decode(new Uint8Array([101, 0]), { stream: false });
  check("stream UTF-16LE two chunks", u1 + u2 === "He", `got "${u1 + u2}"`);

  // 5g. Emoji byte-by-byte (4 separate chunks for a 4-byte char)
  d = new TextDecoder();
  const em1 = d.decode(new Uint8Array([0xf0]), { stream: true });
  const em2 = d.decode(new Uint8Array([0x9f]), { stream: true });
  const em3 = d.decode(new Uint8Array([0x98]), { stream: true });
  const em4 = d.decode(new Uint8Array([0x80]), { stream: false });
  check(
    "stream emoji byte-by-byte",
    em1 + em2 + em3 + em4 === "\u{1F600}",
    `got "${em1 + em2 + em3 + em4}"`,
  );

  // 5h. Multiple emoji characters streamed across chunks
  d = new TextDecoder();
  const mm1 = d.decode(new Uint8Array([0xf0, 0x9f]), { stream: true });
  const mm2 = d.decode(new Uint8Array([0x98, 0x80, 0xf0, 0x9f, 0x98]), {
    stream: true,
  });
  const mm3 = d.decode(new Uint8Array([0x8e]), { stream: false });
  check(
    "stream multiple emoji",
    mm1 + mm2 + mm3 === "\u{1F600}\u{1F60E}",
    `got "${mm1 + mm2 + mm3}"`,
  );

  // 5i. Truncated emoji — incomplete sequence flushed with stream:false
  d = new TextDecoder();
  d.decode(new Uint8Array([0xf0]), { stream: true });
  const tr1 = d.decode(new Uint8Array([]), { stream: false });
  check(
    "stream truncated lead byte on flush",
    tr1 === "\uFFFD",
    `got "${tr1}"`,
  );

  // 5j. Truncated sequence then completed in next chunk
  d = new TextDecoder();
  const tc1 = d.decode(new Uint8Array([0xf0]), { stream: true });
  const tc2 = d.decode(new Uint8Array([0x9f, 0x98, 0x80]), { stream: false });
  check(
    "stream truncated then completed",
    tc1 + tc2 === "\u{1F600}",
    `got "${tc1 + tc2}"`,
  );
}

// ===================================================================
// 6. Fatal error behavior
// ===================================================================

function testFatal() {
  console.log("\n=== Fatal Error Behavior ===");

  // 6a. fatal=false (default) → U+FFFD for invalid byte
  const d1 = new TextDecoder();
  const r1 = d1.decode(new Uint8Array([72, 0xff, 111]));
  check(
    "non-fatal replaces with U+FFFD",
    r1 === "H\uFFFD" + "o",
    `got "${r1}"`,
  );

  // 6b. fatal=true in constructor throws
  const d2 = new TextDecoder("utf-8", { fatal: true });
  try {
    d2.decode(new Uint8Array([0xff]));
    check("fatal=true throws on invalid byte", false, "no error thrown");
  } catch (_) {
    check("fatal=true throws on invalid byte", true);
  }

  // 6c. Decode-time fatal override (true overrides constructor false)
  const d3 = new TextDecoder();
  try {
    d3.decode(new Uint8Array([0xff]), { fatal: true });
    check("decode-time fatal:true overrides", false, "no error thrown");
  } catch (_) {
    check("decode-time fatal:true overrides", true);
  }

  // 6d. Decode-time fatal override (false overrides constructor true)
  const d4 = new TextDecoder("utf-8", { fatal: true });
  const r4 = d4.decode(new Uint8Array([72, 0xff, 111]), { fatal: false });
  check(
    "decode-time fatal:false overrides",
    r4 === "H\uFFFD" + "o",
    `got "${r4}"`,
  );

  // 6e. Overlong sequence with fatal=true
  const d5 = new TextDecoder("utf-8", { fatal: true });
  try {
    d5.decode(new Uint8Array([0xc0, 0xaf]));
    check("fatal throws on overlong", false, "no error thrown");
  } catch (_) {
    check("fatal throws on overlong", true);
  }

  // 6f. Overlong with fatal=false → U+FFFD
  const d6 = new TextDecoder();
  const r6 = d6.decode(new Uint8Array([0xc0, 0xaf]));
  check("non-fatal replaces overlong", r6 === "\uFFFD\uFFFD", `got "${r6}"`);

  // 6g. Stray continuation byte with fatal=true
  const d7 = new TextDecoder("utf-8", { fatal: true });
  try {
    d7.decode(new Uint8Array([0x80]));
    check("fatal throws on stray continuation", false, "no error thrown");
  } catch (_) {
    check("fatal throws on stray continuation", true);
  }
}

// ===================================================================
// Runner
// ===================================================================

function runAll() {
  testEncodingBasics();
  testInputTypes();
  testProperties();
  testBom();
  testStream();
  testFatal();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
}

runAll();
