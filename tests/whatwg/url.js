// 🌐 WHATWG URL / URLSearchParams — URL 标准兼容测试
//
// Import-free: runs in any JS environment with global URL/URLSearchParams.

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
// 1. URL constructor
// ===================================================================

function testURLConstructor() {
  console.log("\n=== URL Constructor ===");

  // Basic URL parsing
  const u1 = new URL("https://example.com");
  check("basic URL", u1.href === "https://example.com/", `got "${u1.href}"`);

  // URL with path
  const u2 = new URL("https://example.com/foo/bar");
  check(
    "URL with path",
    u2.href === "https://example.com/foo/bar",
    `got "${u2.href}"`,
  );

  // URL with trailing slash normalization
  const u3 = new URL("https://example.com");
  check("trailing slash", u3.pathname === "/", `got "${u3.pathname}"`);

  // URL with base
  const u4 = new URL("/path/to/page", "https://example.com");
  check(
    "relative with base",
    u4.href === "https://example.com/path/to/page",
    `got "${u4.href}"`,
  );

  // URL with base (no leading slash)
  const u5 = new URL("relative/path", "https://example.com/base/");
  check(
    "relative no leading slash",
    u5.href === "https://example.com/base/relative/path",
    `got "${u5.href}"`,
  );

  // URL with full URL as second arg
  const u6 = new URL("https://other.com/foo", "https://base.com");
  check(
    "absolute URL with base ignored",
    u6.href === "https://other.com/foo",
    `got "${u6.href}"`,
  );

  // Invalid URL without base
  try {
    new URL("not-a-url");
    check("invalid URL without base", false, "should have thrown");
  } catch (_) {
    check("invalid URL without base", true);
  }

  // Invalid base URL
  try {
    new URL("/path", "not-a-base");
    check("invalid base URL", false, "should have thrown");
  } catch (_) {
    check("invalid base URL", true);
  }
}

// ===================================================================
// 2. URL properties
// ===================================================================

function testURLProperties() {
  console.log("\n=== URL Properties ===");

  const u = new URL(
    "https://user:pass@example.com:8080/foo/bar?query=1&key=val#section",
  );

  check(
    "href",
    u.href ===
      "https://user:pass@example.com:8080/foo/bar?query=1&key=val#section",
    `got "${u.href}"`,
  );
  check("origin", u.origin === "https://example.com:8080", `got "${u.origin}"`);
  check("protocol", u.protocol === "https:", `got "${u.protocol}"`);
  check("username", u.username === "user", `got "${u.username}"`);
  check("password", u.password === "pass", `got "${u.password}"`);
  check("host", u.host === "example.com:8080", `got "${u.host}"`);
  check("hostname", u.hostname === "example.com", `got "${u.hostname}"`);
  check("port", u.port === "8080", `got "${u.port}"`);
  check("pathname", u.pathname === "/foo/bar", `got "${u.pathname}"`);
  check("search", u.search === "?query=1&key=val", `got "${u.search}"`);
  check("hash", u.hash === "#section", `got "${u.hash}"`);
}

// ===================================================================
// 3. URL property setters
// ===================================================================

function testURLSetters() {
  console.log("\n=== URL Setters ===");

  const u = new URL("https://example.com/foo");

  // protocol
  u.protocol = "http:";
  check("set protocol", u.protocol === "http:", `got "${u.protocol}"`);
  check(
    "href after protocol change",
    u.href.startsWith("http://"),
    `got "${u.href}"`,
  );

  // hostname
  u.hostname = "other.com";
  check("set hostname", u.hostname === "other.com", `got "${u.hostname}"`);

  // port
  u.port = "3000";
  check("set port", u.port === "3000", `got "${u.port}"`);

  // pathname
  u.pathname = "/new/path";
  check("set pathname", u.pathname === "/new/path", `got "${u.pathname}"`);

  // search
  u.search = "?a=1";
  check("set search", u.search === "?a=1", `got "${u.search}"`);
  check(
    "href after search set",
    u.href === "http://other.com:3000/new/path?a=1",
    `got "${u.href}"`,
  );

  // search — remove
  u.search = "";
  check("set search empty", u.search === "", `got "${u.search}"`);

  // hash
  u.hash = "anchor";
  check("set hash", u.hash === "#anchor", `got "${u.hash}"`);

  // hash — remove
  u.hash = "";
  check("set hash empty", u.hash === "", `got "${u.hash}"`);

  // username / password
  u.username = "admin";
  u.password = "secret";
  check("set username", u.username === "admin", `got "${u.username}"`);
  check("set password", u.password === "secret", `got "${u.password}"`);

  // href setter
  u.href = "https://new.example.com/wow";
  check(
    "set href",
    u.href === "https://new.example.com/wow",
    `got "${u.href}"`,
  );
}

// ===================================================================
// 4. URL toString / toJSON
// ===================================================================

function testURLStringMethods() {
  console.log("\n=== URL toString / toJSON ===");

  const u = new URL("https://example.com/page");
  check(
    "toString",
    u.toString() === "https://example.com/page",
    `got "${u.toString()}"`,
  );
  check(
    "toJSON",
    u.toJSON() === "https://example.com/page",
    `got "${u.toJSON()}"`,
  );
}

// ===================================================================
// 5. URL searchParams (integration with URL)
// ===================================================================

function testURLSearchParamsIntegration() {
  console.log("\n=== URL searchParams ===");

  const u = new URL("https://example.com/?foo=1&bar=2");
  const sp = u.searchParams;

  check(
    "searchParams is URLSearchParams",
    sp instanceof URLSearchParams,
    `got ${typeof sp}`,
  );

  // Read params
  check("sp.get foo", sp.get("foo") === "1", `got "${sp.get("foo")}"`);
  check("sp.get bar", sp.get("bar") === "2", `got "${sp.get("bar")}"`);
  check(
    "sp.get nonexistent",
    sp.get("nonexistent") === null,
    `got "${sp.get("nonexistent")}"`,
  );

  // Modify through searchParams reflects on URL
  sp.append("baz", "3");
  check(
    "url.search after append",
    u.search === "?foo=1&bar=2&baz=3",
    `got "${u.search}"`,
  );

  // Delete through searchParams
  sp.delete("bar");
  check(
    "url.search after delete",
    u.search === "?foo=1&baz=3",
    `got "${u.search}"`,
  );

  // Set through searchParams
  sp.set("foo", "100");
  check(
    "url.search after set",
    u.search === "?foo=100&baz=3",
    `got "${u.search}"`,
  );

  // Search setter invalidates cached searchParams
  const u2 = new URL("https://example.com/?a=1");
  const sp2 = u2.searchParams;
  check("sp2 initial", sp2.get("a") === "1", `got "${sp2.get("a")}"`);

  u2.search = "?b=2";
  check(
    "sp2 after url.search change",
    sp2.get("b") === "2",
    `got "${sp2.get("b")}"`,
  );
  check("sp2 old key gone", sp2.get("a") === null, `got "${sp2.get("a")}"`);
}

// ===================================================================
// 6. URLSearchParams constructor
// ===================================================================

function testSPConstructor() {
  console.log("\n=== URLSearchParams Constructor ===");

  // Empty
  const sp0 = new URLSearchParams();
  check("empty constructor", sp0.toString() === "", `got "${sp0.toString()}"`);

  // From string
  const sp1 = new URLSearchParams("foo=1&bar=2");
  check("from string", sp1.get("foo") === "1", `got "${sp1.get("foo")}"`);
  check("from string bar=2", sp1.get("bar") === "2", `got "${sp1.get("bar")}"`);

  // From string with leading ?
  const sp2 = new URLSearchParams("?a=3&b=4");
  check("from string with ?", sp2.get("a") === "3", `got "${sp2.get("a")}"`);
  check("from string ? b=4", sp2.get("b") === "4", `got "${sp2.get("b")}"`);
}

// ===================================================================
// 7. URLSearchParams methods
// ===================================================================

function testSPMethods() {
  console.log("\n=== URLSearchParams Methods ===");

  const sp = new URLSearchParams("color=red&size=m&color=blue");

  // get
  check(
    "get first value",
    sp.get("color") === "red",
    `got "${sp.get("color")}"`,
  );
  check("get single value", sp.get("size") === "m", `got "${sp.get("size")}"`);
  check(
    "get nonexistent",
    sp.get("weight") === null,
    `got "${sp.get("weight")}"`,
  );

  // getAll
  const all = sp.getAll("color");
  check("getAll returns array", Array.isArray(all), `got ${typeof all}`);
  check("getAll length", all.length === 2, `got ${all.length}`);
  check("getAll[0]", all[0] === "red", `got "${all[0]}"`);
  check("getAll[1]", all[1] === "blue", `got "${all[1]}"`);
  check(
    "getAll nonexistent",
    sp.getAll("x").length === 0,
    `got ${sp.getAll("x").length}`,
  );

  // has
  check("has existing", sp.has("color") === true, `got ${sp.has("color")}`);
  check("has missing", sp.has("weight") === false, `got ${sp.has("weight")}`);

  // append
  sp.append("size", "l");
  check("append", sp.get("size") === "m", `got "${sp.get("size")}"`);
  check(
    "append getAll",
    sp.getAll("size").length === 2,
    `got ${sp.getAll("size").length}`,
  );

  // set (replaces first, removes rest)
  const sp2 = new URLSearchParams("a=1&a=2&a=3");
  sp2.set("a", "99");
  check(
    "set replaces all",
    sp2.getAll("a").length === 1,
    `got ${sp2.getAll("a").length}`,
  );
  check("set value", sp2.get("a") === "99", `got "${sp2.get("a")}"`);

  // set (new key)
  sp2.set("new", "val");
  check("set new key", sp2.get("new") === "val", `got "${sp2.get("new")}"`);

  // delete
  const sp3 = new URLSearchParams("x=1&y=2&x=3");
  sp3.delete("x");
  check("delete", sp3.has("x") === false, `still has x`);
  check("delete others remain", sp3.get("y") === "2", `got "${sp3.get("y")}"`);

  // sort
  const sp4 = new URLSearchParams("z=1&a=2&m=3");
  sp4.sort();
  check(
    "sort order",
    sp4.toString() === "a=2&m=3&z=1",
    `got "${sp4.toString()}"`,
  );

  // toString
  const sp5 = new URLSearchParams("key=value&foo=bar");
  check(
    "toString",
    sp5.toString() === "key=value&foo=bar",
    `got "${sp5.toString()}"`,
  );

  // forEach
  const sp6 = new URLSearchParams("a=1&b=2&c=3");
  const entries = [];
  sp6.forEach((value, key) => {
    entries.push(`${key}=${value}`);
  });
  check("forEach count", entries.length === 3, `got ${entries.length}`);
  check("forEach order", entries[0] === "a=1", `got "${entries[0]}"`);
  check("forEach order", entries[1] === "b=2", `got "${entries[1]}"`);
  check("forEach order", entries[2] === "c=3", `got "${entries[2]}"`);
}

// ===================================================================
// 8. URL encoding edge cases
// ===================================================================

function testEncoding() {
  console.log("\n=== Encoding Edge Cases ===");

  // Special characters in query
  const u1 = new URL("https://example.com/?q=hello world&lang=en");
  check(
    "encoded space",
    u1.href === "https://example.com/?q=hello%20world&lang=en",
    `got "${u1.href}"`,
  );

  // Chinese characters
  const u2 = new URL("https://example.com/?name=张三");
  check(
    "Chinese chars",
    u2.href.includes("%E5%BC%A0%E4%B8%89"),
    `got "${u2.href}"`,
  );

  // URL with special chars in path
  const u3 = new URL("https://example.com/a b c");
  check(
    "path with spaces",
    u3.pathname === "/a%20b%20c",
    `got "${u3.pathname}"`,
  );
}

// ===================================================================
// 9. Edge cases
// ===================================================================

function testEdgeCases() {
  console.log("\n=== Edge Cases ===");

  // Port 0
  const u1 = new URL("http://example.com:0/");
  check("port 0", u1.port === "0", `got "${u1.port}"`);

  // No port
  const u2 = new URL("http://example.com/");
  check("no port", u2.port === "", `got "${u2.port}"`);

  // Default port for scheme
  const u3 = new URL("https://example.com/");
  check("default https port", u3.port === "", `got "${u3.port}"`);

  // IPv4 host
  const u4 = new URL("http://127.0.0.1:8080/path");
  check("IPv4 host", u4.hostname === "127.0.0.1", `got "${u4.hostname}"`);
  check("IPv4 port", u4.port === "8080", `got "${u4.port}"`);

  // Opaque origin (file: scheme)
  const u5 = new URL("file:///etc/hosts");
  check("file origin", u5.origin === "null", `got "${u5.origin}"`);

  // searchParams for URL with no query
  const u6 = new URL("https://example.com/");
  const sp6 = u6.searchParams;
  check("no query toString", sp6.toString() === "", `got "${sp6.toString()}"`);
  check("no query get", sp6.get("x") === null, `got "${sp6.get("x")}"`);

  // Multiple values for same key
  const sp7 = new URLSearchParams("key=1&key=2&key=3");
  check("multi-value has", sp7.has("key") === true, "");
  check(
    "multi-value get returns first",
    sp7.get("key") === "1",
    `got "${sp7.get("key")}"`,
  );
  check(
    "multi-value getAll count",
    sp7.getAll("key").length === 3,
    `got ${sp7.getAll("key").length}`,
  );

  // URL with blank section
  const u7 = new URL("http://example.com/#");
  check("empty hash", u7.hash === "", `got "${u7.hash}"`);

  // URL with empty search
  const u8 = new URL("http://example.com/?");
  check("empty search", u8.search === "", `got "${u8.search}"`);
}

// ===================================================================
// Runner
// ===================================================================

function runAll() {
  testURLConstructor();
  testURLProperties();
  testURLSetters();
  testURLStringMethods();
  testURLSearchParamsIntegration();
  testSPConstructor();
  testSPMethods();
  testEncoding();
  testEdgeCases();

  const total = pass + fail;
  console.log(`\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`  🎯 总计: ${total}  |  ✅ 通过: ${pass}  |  ❌ 失败: ${fail}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
}

runAll();
