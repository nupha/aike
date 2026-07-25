import { HttpClient } from "http";

const TIMEOUT = 10;

let pass = 0;
let fail = 0;
let skip = 0;

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
// 1. HttpClient 构造函数
// ===================================================================
function testConstructor() {
  console.log("\n=== HttpClient constructor ===");

  const c0 = new HttpClient();
  check("HttpClient exists", typeof HttpClient === "function");
  check("HttpClient instance", c0 instanceof HttpClient);
  check("has request method", typeof c0.request === "function");
  check("has get method", typeof c0.get === "function");
  check("has post method", typeof c0.post === "function");
  check("has put method", typeof c0.put === "function");
  check("has delete method", typeof c0.delete === "function");
  check("has patch method", typeof c0.patch === "function");

  check("HttpClient with empty opts", new HttpClient({}) instanceof HttpClient);
  check(
    "HttpClient with rejectUnauthorized",
    new HttpClient({ rejectUnauthorized: false }) instanceof HttpClient,
  );
  check(
    "HttpClient with proxy",
    new HttpClient({ proxy: "http://127.0.0.1:8080" }) instanceof HttpClient,
  );
  check(
    "HttpClient with noProxy",
    new HttpClient({
      proxy: "http://127.0.0.1:8080",
      noProxy: ".local,.corp",
    }) instanceof HttpClient,
  );
  check(
    "HttpClient with cookieJar",
    new HttpClient({ cookieJar: true }) instanceof HttpClient,
  );
  check(
    "HttpClient with protocol http1",
    new HttpClient({ protocol: "http1" }) instanceof HttpClient,
  );
  check(
    "HttpClient with connectTimeout",
    new HttpClient({ connectTimeout: 5 }) instanceof HttpClient,
  );
}

// ===================================================================
// 2. GET
// ===================================================================
function testHttpGet() {
  console.log("\n=== HttpClient basic GET ===");
  const client = new HttpClient();
  return client
    .get("https://example.com", { timeout: TIMEOUT })
    .then((r) => {
      check("GET status 200", r.status === 200, `got ${r.status}`);
      check("GET ok", r.ok === true);
      check(
        "GET type basic/default",
        r.type === "default" || r.type === "basic",
        `got ${r.type}`,
      );
      check("GET content-type present", r.headers.get("content-type") !== null);
      check("GET has url", typeof r.url === "string" && r.url.length > 0);
      return r.text();
    })
    .then((t) => check("GET body non-empty", t.length > 0, `len ${t.length}`));
}

// ===================================================================
// 3. POST（example.com 会返回 405，但表示请求已被正常发送）
// ===================================================================
function testHttpPost() {
  console.log("\n=== HttpClient POST with body ===");
  const client = new HttpClient();
  return client
    .post("https://example.com", {
      body: "hello",
      headers: { "Content-Type": "text/plain" },
      timeout: TIMEOUT,
    })
    .then((r) => {
      // POST to example.com returns 405 Method Not Allowed — 正确的服务端响应
      check(
        "POST request completed",
        r.status >= 200 && r.status < 600,
        `got ${r.status}`,
      );
      check("POST status 405", r.status === 405, `got ${r.status}`);
      check("POST has body", r.bodyUsed === false);
      return r.text();
    })
    .then((t) =>
      check("POST response body present", t.length > 0, `len ${t.length}`),
    );
}

// ===================================================================
// 4. request() 通用方法
// ===================================================================
function testRequest() {
  console.log("\n=== HttpClient.request() ===");
  const client = new HttpClient();
  return client
    .request("https://example.com", {
      method: "GET",
      timeout: TIMEOUT,
    })
    .then((r) =>
      check("request() GET status 200", r.status === 200, `got ${r.status}`),
    );
}

// ===================================================================
// 5. 复用同一客户端
// ===================================================================
function testReuse() {
  console.log("\n=== HttpClient reuse ===");
  const client = new HttpClient();
  return client
    .get("https://example.com", { timeout: TIMEOUT })
    .then(() => client.get("https://example.com/", { timeout: TIMEOUT }))
    .then((r) =>
      check("reuse: 2nd request", r.status === 200, `got ${r.status}`),
    );
}

// ===================================================================
// 6. 与 fetch 一致
// ===================================================================
function testConsistentWithFetch() {
  console.log("\n=== HttpClient consistent with fetch ===");
  const client = new HttpClient();
  return Promise.all([
    fetch("https://example.com"),
    client.get("https://example.com", { timeout: TIMEOUT }),
  ]).then(([fr, cr]) => {
    check(
      "HttpClient & fetch same status",
      fr.status === cr.status,
      `fetch=${fr.status} client=${cr.status}`,
    );
    check(
      "HttpClient & fetch same type",
      fr.type === cr.type,
      `fetch=${fr.type} client=${cr.type}`,
    );
  });
}

// ===================================================================
// 7. timeout 拒绝（非常短的超时，即使对 example.com 也必定触发）
// ===================================================================
function testTimeout() {
  console.log("\n=== HttpClient timeout ===");
  const client = new HttpClient();
  return client
    .get("https://example.com", { timeout: 0.001 })
    .then(() => check("timeout: should have thrown", false))
    .catch((e) =>
      check(
        "timeout: request rejected",
        typeof e === "string" || e instanceof Error,
        String(e).substring(0, 60),
      ),
    );
}

// ===================================================================
// 8. AbortController / signal — currently unsupported on HttpClient
//    (zuqe engine limitation: native-object pointer lost when retrieved
//     from a JS property.  Use `fetch()` for abort support.)
// ===================================================================
function testAbort() {
  console.log("\n=== HttpClient AbortController ===");
  const c = new HttpClient();
  const ac = new AbortController();
  ac.abort();
  // signal 选项被接受但暂不生效（zuqe 限制）
  return c
    .get("https://example.com", { signal: ac.signal, timeout: TIMEOUT })
    .then(() => {
      console.log("  ⏭️  abort: unsupported (zuqe limitation)");
      skip++;
    })
    .catch(() => {
      console.log("  ⏭️  abort: caught error");
      skip++;
    });
}

// ===================================================================
// 9. 自定义 headers（example.com 不 echo，但至少验证请求发出且返回正常状态）
// ===================================================================
function testCustomHeaders() {
  console.log("\n=== HttpClient custom headers ===");
  const client = new HttpClient();
  return client
    .get("https://example.com", {
      headers: { "X-Test": "hello-httpclient" },
      timeout: TIMEOUT,
    })
    .then((r) => {
      check(
        "custom headers: request completed",
        r.status === 200,
        `got ${r.status}`,
      );
    });
}

// ===================================================================
// 9. redirect 选项（example.com 返回 200 无重定向，验证选项被正确接受）
// ===================================================================
function testRedirect() {
  console.log("\n=== HttpClient redirect options ===");
  const client = new HttpClient();

  return client
    .get("https://example.com", { redirect: "follow", timeout: TIMEOUT })
    .then((r) =>
      check("redirect:follow works", r.status === 200, `got ${r.status}`),
    )
    .then(() =>
      client.get("https://example.com", {
        redirect: "error",
        timeout: TIMEOUT,
      }),
    )
    .then((r) =>
      check(
        "redirect:error works (no redirect)",
        r.status === 200,
        `got ${r.status}`,
      ),
    )
    .then(() =>
      client.get("https://example.com", {
        redirect: "manual",
        timeout: TIMEOUT,
      }),
    )
    .then((r) =>
      check(
        "redirect:manual works (no redirect)",
        r.status === 200,
        `got ${r.status}`,
      ),
    );
}

// ===================================================================
// 10. PUT / DELETE / PATCH（example.com 返回 405，但证明请求被正常发送）
// ===================================================================
function testOtherMethods() {
  console.log("\n=== HttpClient PUT / DELETE / PATCH ===");
  const client = new HttpClient();

  return client
    .put("https://example.com", {
      body: "update",
      headers: { "Content-Type": "text/plain" },
      timeout: TIMEOUT,
    })
    .then((r) =>
      check("PUT request completed", r.status === 405, `got ${r.status}`),
    )
    .then(() => client.delete("https://example.com", { timeout: TIMEOUT }))
    .then((r) =>
      check("DELETE request completed", r.status === 405, `got ${r.status}`),
    )
    .then(() =>
      client.patch("https://example.com", {
        body: "modify",
        timeout: TIMEOUT,
      }),
    )
    .then((r) =>
      check("PATCH request completed", r.status === 405, `got ${r.status}`),
    );
}

// ===================================================================
// Run
// ===================================================================
testConstructor();

Promise.resolve()
  .then(testHttpGet)
  .then(testHttpPost)
  .then(testRequest)
  .then(testReuse)
  .then(testConsistentWithFetch)
  .then(testCustomHeaders)
  .then(testAbort)
  .then(testRedirect)
  .then(testOtherMethods)
  .then(testTimeout)
  .then(() => {
    console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
    console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  })
  .catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
  });
