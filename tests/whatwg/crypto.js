// 🔐 WHATWG Web Crypto — crypto / crypto.subtle 标准兼容测试
//
// Import-free: runs in any JS environment with the Web Crypto globals.
// Test vectors are cross-checked against pyca `cryptography` and the NIST
// GCM/HMAC test cases.

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

const hex = (b) =>
  Array.from(new Uint8Array(b), (x) => x.toString(16).padStart(2, "0")).join("");

const unhex = (s) => new Uint8Array(s.match(/../g).map((x) => parseInt(x, 16)));

async function rejects(label, name, fn) {
  try {
    await fn();
    check(label, false, "did not reject");
  } catch (e) {
    check(label, e.name === name, `threw ${e.name}`);
  }
}

function rejectsSync(label, name, fn) {
  try {
    fn();
    check(label, false, "did not throw");
  } catch (e) {
    check(label, e.name === name, `threw ${e.name}`);
  }
}

// ===================================================================
// 1. crypto object / getRandomValues / randomUUID / sha256Hex
// ===================================================================

function testCryptoBasics() {
  console.log("\n=== crypto object ===");
  check("crypto is object", typeof crypto === "object");
  check("crypto.subtle exists", typeof crypto.subtle === "object");
  check("Crypto/SubtleCrypto/CryptoKey classes", typeof Crypto === "function" && typeof SubtleCrypto === "function" && typeof CryptoKey === "function");

  const buf = new Uint8Array(16);
  const ret = crypto.getRandomValues(buf);
  check("getRandomValues returns same array", ret === buf);
  check("getRandomValues filled", buf.some((b) => b !== 0));
  check("getRandomValues Int32Array", crypto.getRandomValues(new Int32Array(4)) instanceof Int32Array);
  check("getRandomValues Uint8ClampedArray", crypto.getRandomValues(new Uint8ClampedArray(4)) instanceof Uint8ClampedArray);

  rejectsSync("getRandomValues rejects Float32Array", "TypeError", () => crypto.getRandomValues(new Float32Array(4)));
  rejectsSync("getRandomValues rejects Float64Array", "TypeError", () => crypto.getRandomValues(new Float64Array(4)));
  rejectsSync("getRandomValues rejects DataView", "TypeError", () => crypto.getRandomValues(new DataView(new ArrayBuffer(8))));
  rejectsSync("getRandomValues rejects >65536 bytes", "QuotaExceededError", () => crypto.getRandomValues(new Uint8Array(65537)));

  const u = crypto.randomUUID();
  check("randomUUID format", /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u), u);
  check("randomUUID unique", crypto.randomUUID() !== u);

  check(
    "sha256Hex('abc')",
    crypto.sha256Hex(new TextEncoder().encode("abc")) === "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
  check(
    "sha256Hex(empty)",
    crypto.sha256Hex(new Uint8Array(0)) === "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  );
  rejectsSync("sha256Hex rejects string", "TypeError", () => crypto.sha256Hex("abc"));
}

// ===================================================================
// 2. subtle.digest
// ===================================================================

async function testDigest() {
  console.log("\n=== digest ===");
  const data = new TextEncoder().encode("abc");
  for (const [alg, expected] of [
    ["SHA-1", "a9993e364706816aba3e25717850c26c9cd0d89d"],
    ["SHA-256", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"],
    ["SHA-384", "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7"],
    ["SHA-512", "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"],
  ]) {
    const d = await crypto.subtle.digest(alg, data);
    check(`digest ${alg}`, hex(d) === expected, hex(d));
    // Case-insensitive algorithm name
    const d2 = await crypto.subtle.digest(alg.toLowerCase(), data);
    check(`digest ${alg.toLowerCase()} (case-insensitive)`, hex(d2) === expected);
  }
  const d = await crypto.subtle.digest("SHA-256", new Uint8Array(0));
  check("digest empty", hex(d) === "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
  check("digest returns ArrayBuffer", d instanceof ArrayBuffer);
  await rejects("digest unknown algorithm", "NotSupportedError", () => crypto.subtle.digest("MD5", data));
  await rejects("digest rejects string data", "TypeError", () => crypto.subtle.digest("SHA-256", "abc"));
}

// ===================================================================
// 3. HMAC sign / verify
// ===================================================================

async function testHmac() {
  console.log("\n=== HMAC sign/verify ===");
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode("key"),
    { name: "HMAC", hash: "SHA-256" },
    true,
    ["sign", "verify"],
  );
  check("key.type", key.type === "secret");
  check("key.extractable", key.extractable === true);
  check("key.algorithm", JSON.stringify(key.algorithm) === JSON.stringify({ name: "HMAC", hash: "SHA-256", length: 24 }));
  check("key.usages", key.usages.join(",") === "sign,verify");

  const data = new TextEncoder().encode("The quick brown fox jumps over the lazy dog");
  const expected = "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8";
  const sig = await crypto.subtle.sign("HMAC", key, data);
  check("sign RFC4231 vector", hex(sig) === expected, hex(sig));
  check("sign length", sig.byteLength === 32);
  check("verify ok", await crypto.subtle.verify("HMAC", key, sig, data) === true);
  check("verify wrong data", await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode("...")) === false);
  check("verify wrong signature", await crypto.subtle.verify("HMAC", key, new Uint8Array(32), data) === false);

  // sign with algorithm object (hash must match key)
  const sig2 = await crypto.subtle.sign({ name: "HMAC", hash: "SHA-256" }, key, data);
  check("sign with algorithm object", hex(sig2) === expected);

  // HMAC-SHA1/SHA384/SHA512 keys
  const key512 = await crypto.subtle.importKey("raw", new TextEncoder().encode("key"), { name: "HMAC", hash: "SHA-512" }, false, ["sign"]);
  const sig512 = await crypto.subtle.sign("HMAC", key512, data);
  check(
    "sign HMAC-SHA512",
    hex(sig512) === "b42af09057bac1e2d41708e48a902e09b5ff7f12ab428a4fe86653c73dd248fb82f948a549f7b791a5b41915ee4d1ec3935357e4e2317250d0372afa2ebeeb3a",
  );
  check("non-extractable key cannot export", "InvalidAccessError");
  await rejects("exportKey non-extractable", "InvalidAccessError", () => crypto.subtle.exportKey("raw", key512));

  const encKey = await crypto.subtle.importKey("raw", new Uint8Array(16), { name: "AES-GCM" }, true, ["encrypt"]);
  const verifyOnly = await crypto.subtle.importKey("raw", new TextEncoder().encode("k"), { name: "HMAC", hash: "SHA-256" }, true, ["verify"]);
  await rejects("sign wrong-usage key", "InvalidAccessError", () => crypto.subtle.sign("HMAC", verifyOnly, data));
  await rejects("sign non-HMAC key", "InvalidAccessError", () => crypto.subtle.sign("HMAC", encKey, data));
  await rejects("sign bad algorithm", "NotSupportedError", () => crypto.subtle.sign("RSA", key, data));
}

// ===================================================================
// 4. AES-GCM encrypt / decrypt
// ===================================================================

async function testAesGcm() {
  console.log("\n=== AES-GCM ===");
  const key = await crypto.subtle.importKey(
    "raw",
    unhex("feffe9928665731c6d6a8f9467308308"),
    { name: "AES-GCM" },
    true,
    ["encrypt", "decrypt"],
  );
  const iv = unhex("cafebabefacedbaddecaf888");
  const pt = unhex(
    "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
  );

  // NIST GCM test case 3 (with AAD) — cross-checked with pyca cryptography.
  const aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad }, key, pt);
  check(
    "encrypt NIST TC3",
    hex(ct) === "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985da80ce830cfda02da2a218a1744f4c76",
    hex(ct),
  );
  const back = await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: aad }, key, ct);
  check("decrypt roundtrip", hex(back) === hex(pt));

  // NIST GCM test case 2 (empty AAD).
  const ct2 = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, pt);
  check(
    "encrypt NIST TC2",
    hex(ct2) === "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f59854d5c2af327cd64a62cf35abd2ba6fab4",
    hex(ct2),
  );

  // Empty plaintext (tag-only output) with empty / non-block-aligned AAD.
  const emptyCt = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, new Uint8Array(0));
  check("encrypt empty plaintext", hex(emptyCt) === "3247184b3c4f69a44dbcd22887bbb418", hex(emptyCt));
  const aad19 = unhex("feedfacedeadbeeffeedfacedeadbeefabadd2");
  const emptyCtAad = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad19 }, key, new Uint8Array(0));
  check("encrypt empty plaintext + 19-byte AAD", hex(emptyCtAad) === "658fc92f41cdeed4261e5e523326730a", hex(emptyCtAad));

  // decrypt with wrong AAD must fail
  await rejects("decrypt wrong AAD", "OperationError", () =>
    crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: new Uint8Array(4) }, key, ct),
  );
  // decrypt with tampered ciphertext must fail
  const tampered = new Uint8Array(ct);
  tampered[0] ^= 1;
  await rejects("decrypt tampered ciphertext", "OperationError", () =>
    crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: aad }, key, tampered),
  );
  await rejects("encrypt missing iv", "TypeError", () => crypto.subtle.encrypt({ name: "AES-GCM" }, key, pt));
  await rejects("encrypt bad tagLength", "OperationError", () =>
    crypto.subtle.encrypt({ name: "AES-GCM", iv, tagLength: 100 }, key, pt),
  );
  await rejects("encrypt unsupported algorithm", "NotSupportedError", () => crypto.subtle.encrypt({ name: "AES-CBC", iv }, key, pt));

  // a key with only the encrypt usage cannot decrypt
  const encOnly = await crypto.subtle.importKey("raw", new Uint8Array(16), { name: "AES-GCM" }, true, ["encrypt"]);
  await rejects("decrypt without decrypt usage", "InvalidAccessError", () =>
    crypto.subtle.decrypt({ name: "AES-GCM", iv }, encOnly, ct),
  );

  // 192 / 256-bit keys roundtrip
  for (const len of [24, 32]) {
    const k = crypto.getRandomValues(new Uint8Array(len));
    const gk = await crypto.subtle.importKey("raw", k, { name: "AES-GCM" }, true, ["encrypt", "decrypt"]);
    const i = crypto.getRandomValues(new Uint8Array(12));
    const msg = new TextEncoder().encode("hello");
    const c = await crypto.subtle.encrypt({ name: "AES-GCM", iv: i }, gk, msg);
    const p = await crypto.subtle.decrypt({ name: "AES-GCM", iv: i }, gk, c);
    check(`aes-gcm ${len * 8}-bit roundtrip`, new TextDecoder().decode(p) === "hello");
  }
}

// ===================================================================
// 5. importKey / exportKey
// ===================================================================

async function testKeys() {
  console.log("\n=== importKey / exportKey ===");

  // raw HMAC import/export
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode("secret"), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
  const raw = new Uint8Array(await crypto.subtle.exportKey("raw", key));
  check("exportKey raw", new TextDecoder().decode(raw) === "secret");

  // jwk export/import roundtrip
  const jwk = await crypto.subtle.exportKey("jwk", key);
  check("jwk kty", jwk.kty === "oct");
  check("jwk alg", jwk.alg === "HS256");
  check("jwk ext", jwk.ext === true);
  check("jwk key_ops", JSON.stringify(jwk.key_ops) === '["sign"]');
  const key2 = await crypto.subtle.importKey("jwk", jwk, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]);
  const sig = await crypto.subtle.sign("HMAC", key2, new TextEncoder().encode("x"));
  check("jwk reimport sign works", sig.byteLength === 32);

  // jwk AES-GCM
  const aes = await crypto.subtle.importKey("raw", new Uint8Array(32), { name: "AES-GCM" }, true, ["encrypt"]);
  const jwka = await crypto.subtle.exportKey("jwk", aes);
  check("jwk AES alg", jwka.alg === "A256GCM");

  // key length validation
  await rejects("AES raw key 15 bytes", "DataError", () =>
    crypto.subtle.importKey("raw", new Uint8Array(15), { name: "AES-GCM" }, true, ["encrypt"]),
  );
  await rejects("AES raw key 17 bytes", "DataError", () =>
    crypto.subtle.importKey("raw", new Uint8Array(17), { name: "AES-GCM" }, true, ["encrypt"]),
  );
  await rejects("empty HMAC raw key", "DataError", () =>
    crypto.subtle.importKey("raw", new Uint8Array(0), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("HMAC import missing hash", "SyntaxError", () =>
    crypto.subtle.importKey("raw", new TextEncoder().encode("k"), { name: "HMAC" }, true, ["sign"]),
  );
  await rejects("empty usages", "SyntaxError", () =>
    crypto.subtle.importKey("raw", new TextEncoder().encode("k"), { name: "HMAC", hash: "SHA-256" }, true, []),
  );
  await rejects("bad usage", "SyntaxError", () =>
    crypto.subtle.importKey("raw", new TextEncoder().encode("k"), { name: "HMAC", hash: "SHA-256" }, true, ["encrypt"]),
  );
  await rejects("bad algorithm", "NotSupportedError", () =>
    crypto.subtle.importKey("raw", new Uint8Array(16), { name: "RSA" }, true, ["encrypt"]),
  );
  await rejects("bad format", "NotSupportedError", () =>
    crypto.subtle.importKey("pkcs8", new Uint8Array(16), { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("export bad format", "NotSupportedError", () => crypto.subtle.exportKey("spki", key));

  // jwk consistency checks
  await rejects("jwk wrong kty", "DataError", () =>
    crypto.subtle.importKey("jwk", { kty: "RSA", k: "a2V5" }, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("jwk bad alg", "DataError", () =>
    crypto.subtle.importKey("jwk", { kty: "oct", k: "a2V5", alg: "HS512" }, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("jwk bad key_ops", "DataError", () =>
    crypto.subtle.importKey("jwk", { kty: "oct", k: "a2V5", key_ops: ["verify"] }, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("jwk ext mismatch", "DataError", () =>
    crypto.subtle.importKey("jwk", { kty: "oct", k: "a2V5", ext: false }, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );
  await rejects("jwk invalid base64url", "DataError", () =>
    crypto.subtle.importKey("jwk", { kty: "oct", k: "!!!" }, { name: "HMAC", hash: "SHA-256" }, true, ["sign"]),
  );

  // illegal constructors
  rejectsSync("new Crypto() illegal", "TypeError", () => new Crypto());
  rejectsSync("new SubtleCrypto() illegal", "TypeError", () => new SubtleCrypto());
  rejectsSync("new CryptoKey() illegal", "TypeError", () => new CryptoKey());
}

// ===================================================================
// Runner
// ===================================================================

async function runAll() {
  testCryptoBasics();
  await testDigest();
  await testHmac();
  await testAesGcm();
  await testKeys();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  if (fail > 0) process.exit(1);
}

runAll();
