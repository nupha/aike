// 🗺️ path module — fileURLToPath / pathToFileURL (URL ↔ path conversion)
//
// Follows Node semantics (url.fileURLToPath / url.pathToFileURL):
// - fileURLToPath accepts a file: URL string or URL object, returns a path.
// - pathToFileURL resolves to an absolute path and returns a URL object.
// Tests are platform-aware (win32 vs posix).

import * as path from "path";

const IS_WIN = process.platform === "windows";

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
// 1. API shape
// ===================================================================

async function testShape() {
  console.log("\n=== path URL API — shape ===");

  check("fileURLToPath is a function", typeof path.fileURLToPath === "function");
  check("pathToFileURL is a function", typeof path.pathToFileURL === "function");

  const u = path.pathToFileURL(IS_WIN ? "C:\\x" : "/x");
  check("pathToFileURL returns a URL", u instanceof URL, `got ${u?.constructor?.name}`);
}

// ===================================================================
// 2. fileURLToPath basics
// ===================================================================

async function testFileURLToPath() {
  console.log("\n=== fileURLToPath — conversion ===");

  if (IS_WIN) {
    check(
      "drive path",
      path.fileURLToPath("file:///C:/foo/bar.txt") === "C:\\foo\\bar.txt",
      `got ${path.fileURLToPath("file:///C:/foo/bar.txt")}`,
    );
    check(
      "percent-encoded space decoded",
      path.fileURLToPath("file:///C:/foo%20bar/baz.txt") === "C:\\foo bar\\baz.txt",
      `got ${path.fileURLToPath("file:///C:/foo%20bar/baz.txt")}`,
    );
    check(
      "localhost host is dropped",
      path.fileURLToPath("file://localhost/C:/x") === "C:\\x",
      `got ${path.fileURLToPath("file://localhost/C:/x")}`,
    );
    check(
      "UNC host share",
      path.fileURLToPath("file://server/share/x") === "\\\\server\\share\\x",
      `got ${path.fileURLToPath("file://server/share/x")}`,
    );
  } else {
    check(
      "absolute path",
      path.fileURLToPath("file:///foo/bar.txt") === "/foo/bar.txt",
      `got ${path.fileURLToPath("file:///foo/bar.txt")}`,
    );
    check(
      "percent-encoded space decoded",
      path.fileURLToPath("file:///foo%20bar/baz.txt") === "/foo bar/baz.txt",
      `got ${path.fileURLToPath("file:///foo%20bar/baz.txt")}`,
    );
    check(
      "localhost host is dropped",
      path.fileURLToPath("file://localhost/foo/x") === "/foo/x",
      `got ${path.fileURLToPath("file://localhost/foo/x")}`,
    );
  }

  // Accepts a URL object.
  const objUrl = new URL(IS_WIN ? "file:///C:/x" : "file:///x");
  check(
    "accepts URL object",
    path.fileURLToPath(objUrl) === (IS_WIN ? "C:\\x" : "/x"),
    `got ${path.fileURLToPath(objUrl)}`,
  );
}

// ===================================================================
// 3. fileURLToPath errors
// ===================================================================

async function testFileURLToPathErrors() {
  console.log("\n=== fileURLToPath — errors ===");

  const mustTypeError = (label, fn) => {
    let isType = false;
    try {
      fn();
    } catch (e) {
      isType = e instanceof TypeError;
    }
    check(label, isType);
  };

  mustTypeError("non-file scheme throws TypeError", () => path.fileURLToPath("http://a/b"));
  mustTypeError("encoded / rejected", () =>
    path.fileURLToPath(IS_WIN ? "file:///C:/a%2Fb" : "file:///a%2Fb"),
  );
  if (IS_WIN) {
    mustTypeError("encoded \\ rejected", () => path.fileURLToPath("file:///C:/a%5Cb"));
  } else {
    mustTypeError("non-empty host rejected", () => path.fileURLToPath("file://host/x"));
  }
  mustTypeError("relative non-file string rejected", () => path.fileURLToPath("C:\\foo"));
  for (const v of [undefined, null, 42, {}, true, Symbol("x")]) {
    mustTypeError(`non-string/non-URL (${v === null ? "null" : typeof v}) rejected`, () =>
      path.fileURLToPath(v),
    );
  }
}

// ===================================================================
// 4. pathToFileURL basics
// ===================================================================

async function testPathToFileURL() {
  console.log("\n=== pathToFileURL — conversion ===");

  if (IS_WIN) {
    check(
      "windows path -> file URL",
      path.pathToFileURL("C:\\foo\\bar.txt").href === "file:///C:/foo/bar.txt",
      `got ${path.pathToFileURL("C:\\foo\\bar.txt").href}`,
    );
    check(
      "forward slashes accepted",
      path.pathToFileURL("C:/foo/bar.txt").href === "file:///C:/foo/bar.txt",
      `got ${path.pathToFileURL("C:/foo/bar.txt").href}`,
    );
    check(
      "trailing separator preserved",
      path.pathToFileURL("C:\\foo\\").href === "file:///C:/foo/",
      `got ${path.pathToFileURL("C:\\foo\\").href}`,
    );
    check(
      "space percent-encoded",
      path.pathToFileURL("C:\\foo bar.txt").href === "file:///C:/foo%20bar.txt",
      `got ${path.pathToFileURL("C:\\foo bar.txt").href}`,
    );
  } else {
    check(
      "posix path -> file URL",
      path.pathToFileURL("/foo/bar.txt").href === "file:///foo/bar.txt",
      `got ${path.pathToFileURL("/foo/bar.txt").href}`,
    );
    check(
      "trailing separator preserved",
      path.pathToFileURL("/foo/").href === "file:///foo/",
      `got ${path.pathToFileURL("/foo/").href}`,
    );
    check(
      "space percent-encoded",
      path.pathToFileURL("/foo bar.txt").href === "file:///foo%20bar.txt",
      `got ${path.pathToFileURL("/foo bar.txt").href}`,
    );
  }

  let threw = false;
  try {
    path.pathToFileURL(42);
  } catch (e) {
    threw = e instanceof TypeError;
  }
  check("non-string path throws TypeError", threw);
}

// ===================================================================
// 5. round-trip
// ===================================================================

async function testRoundTrip() {
  console.log("\n=== fileURLToPath ↔ pathToFileURL round-trip ===");

  const target = IS_WIN ? "C:\\foo\\bar baz.txt" : "/foo/bar baz.txt";
  const back = path.fileURLToPath(path.pathToFileURL(target));
  check("round-trips to the original path", back === target, `got ${back}`);
}

// ===================================================================
// Runner
// ===================================================================

await testShape();
await testFileURLToPath();
await testFileURLToPathErrors();
await testPathToFileURL();
await testRoundTrip();

const total = pass + fail;
console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);

if (fail > 0) {
  process.exit(1);
}
