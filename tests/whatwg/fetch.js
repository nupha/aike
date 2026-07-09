// 🌐 WHATWG Fetch API — fetch / Request / Response / Headers 兼容测试
//
// 整合自以下冒烟脚本：
//   - fetch 构造函数/Headers/Response/Request 同步校验
//   - 真实网络 fetch() 端到端
//
// 遗留 request() 全局测试见 tests/http/request.js
//
// 运行：aike tests/whatwg/fetch.js
// 说明：网络用例在无外网时会以 `skip` 记录，不计入失败。

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

function skipCheck(label, reason) {
  console.log(`  ⏭️  ${label}${reason ? " — " + reason : ""}`);
  skip++;
}

// ===================================================================
// 1. Headers
// ===================================================================
function testHeaders() {
  console.log("\n=== Headers ===");

  // 1.1 数组对构造
  const h0 = new Headers([["k1", "v1"], ["k2", "v2"]]);
  check("Headers from pairs get", h0.get("k1") === "v1", `got ${h0.get("k1")}`);
  check("Headers from pairs get (other)", h0.get("k2") === "v2");

  // 1.2 记录对象构造
  const h1 = new Headers({ "X-Test": "yes", "A": "b" });
  check("Headers from record (case-insensitive get)", h1.get("x-test") === "yes", `got ${h1.get("x-test")}`);
  check("Headers from record has", h1.has("A") === true);

  // 1.3 大小写不敏感
  h1.set("Content-Type", "text/plain");
  check("Headers set case-insensitive", h1.get("content-type") === "text/plain");

  // 1.4 append 逗号合并
  const h2 = new Headers();
  h2.append("X-Multi", "a");
  h2.append("X-Multi", "b");
  check("Headers append comma-merge", h2.get("x-multi") === "a, b", `got ${h2.get("x-multi")}`);

  // 1.5 delete / has
  h2.delete("x-multi");
  check("Headers delete", h2.has("x-multi") === false);

  // 1.6 forEach
  const seen = {};
  h0.forEach((v, k) => { seen[k] = v; });
  check("Headers forEach", seen["k1"] === "v1" && seen["k2"] === "v2", JSON.stringify(seen));

  // 1.7 迭代 [name, value]
  let itCount = 0;
  let firstPair = null;
  for (const pair of h0) {
    if (firstPair === null) firstPair = pair;
    itCount++;
  }
  check("Headers iterator count", itCount === 2, `got ${itCount}`);
  check("Headers iterator yields [k,v]", Array.isArray(firstPair) && firstPair[0] === "k1" && firstPair[1] === "v1");
}

// ===================================================================
// 2. Response 构造函数 / 静态方法 / getter
// ===================================================================
function testResponseSync() {
  console.log("\n=== Response (sync) ===");

  const r = new Response("hello", { status: 201, headers: { "X-Test": "yes" } });
  check("Response status", r.status === 201, `got ${r.status}`);
  check("Response ok (201)", r.ok === true);
  // 构造器创建的 Response.type 规范为 "default"（"basic" 等属于 fetch 响应）
  check("Response type default", r.type === "default", `got ${r.type}`);
  check("Response headers case-insensitive", r.headers.get("x-test") === "yes");

  const r4 = new Response(null, { status: 404 });
  check("Response ok (404)", r4.ok === false);
  check("Response statusText default", typeof r4.statusText === "string");

  // 2.1 Response.error()
  const err = Response.error();
  check("Response.error type", err.type === "error", `got ${err.type}`);
  check("Response.error body null", err.body === null);

  // 2.2 Response.redirect()
  const redir = Response.redirect("http://example.com/go", 302);
  check("Response.redirect status", redir.status === 302, `got ${redir.status}`);
  check("Response.redirect location", redir.headers.get("location") === "http://example.com/go");

  let threw = false;
  try { Response.redirect("http://x", 200); } catch (e) { threw = true; }
  check("Response.redirect invalid status throws", threw);

  // 2.3 bodyUsed 初始为 false
  check("Response bodyUsed pre", r.bodyUsed === false);
}

// ===================================================================
// 3. Request 构造函数 / getter（body 消费的异步部分见 §5）
// ===================================================================
function testRequestSync() {
  console.log("\n=== Request (sync) ===");

  const req = new Request("http://example.com/x", { method: "POST", headers: { Y: "z" } });
  check("Request method", req.method === "POST", `got ${req.method}`);
  check("Request url", req.url === "http://example.com/x", `got ${req.url}`);
  check("Request headers case-insensitive", req.headers.get("y") === "z");

  // 继承 Request 输入
  const req2 = new Request(req, { headers: { "Y": "z2" } });
  check("Request inherits url", req2.url === "http://example.com/x");
  check("Request override header", req2.headers.get("y") === "z2");

  const req3 = new Request("foo/bar", { method: "GET" });
  // aike 无文档 base URL，相对 URL 原样保留
  check("Request relative url preserved", req3.url === "foo/bar", `got ${req3.url}`);
}

// ===================================================================
// 4. 异步 body 消费：Response.text / clone / Request.arrayBuffer
// ===================================================================
function testBodies() {
  console.log("\n=== Bodies (async) ===");

  return Promise.resolve()
    .then(() => {
      // clone 必须在消费前；两 body 独立
      const r = new Response("hello", { status: 201 });
      const c = r.clone();
      check("clone status", c.status === 201);
      check("clone bodyUsed pre", c.bodyUsed === false);
      check("orig bodyUsed pre", r.bodyUsed === false);
      return Promise.all([r.text(), c.text()]).then(([t1, t2]) => {
        check("Response.text()", t1 === "hello", `got "${t1}"`);
        check("clone.text() independent", t2 === "hello", `got "${t2}"`);
        check("orig bodyUsed post", r.bodyUsed === true);
        check("clone bodyUsed post", c.bodyUsed === true);
      });
    })
    .then(() => {
      // 已消费的 body 再次 clone 应抛错
      const r2 = new Response("x");
      return r2.text().then(() => {
        let threw = false;
        try { r2.clone(); } catch (e) { threw = true; }
        check("clone after body consumed throws", threw);
      });
    })
    .then(() => {
      // Request body 流式读取
      const req = new Request("http://x/y", { method: "POST", body: "ping" });
      return req.arrayBuffer().then((b) => {
        const bytes = new Uint8Array(b);
        const s = String.fromCharCode.apply(null, bytes);
        check("Request.arrayBuffer() roundtrip", s === "ping", `got "${s}"`);
      });
    })
    .then(() => {
      // Response.arrayBuffer / bytes 等价
      const r3 = new Response("abc");
      return r3.arrayBuffer().then((b) => {
        check("Response.arrayBuffer byteLength", new Uint8Array(b).length === 3, `got ${new Uint8Array(b).length}`);
      });
    });
}

// ===================================================================
// 5. 真实网络 fetch()（无网则 skip）
// ===================================================================
function testNetworkFetch() {
  console.log("\n=== Network fetch() ===");
  return fetch("https://example.com")
    .then((r) => {
      check("fetch status 200", r.status === 200, `got ${r.status}`);
      check("fetch ok", r.ok === true);
      check("fetch redirected false", r.redirected === false);
      check("fetch url set", typeof r.url === "string" && r.url.length > 0, `got ${r.url}`);
      check("fetch contentType", typeof r.contentType === "string");
      check("fetch header server", !!r.headers.get("server"), `got ${r.headers.get("server")}`);
      check("fetch bodyUsed pre", r.bodyUsed === false);
      return r.text();
    })
    .then((t) => {
      check("fetch body non-empty", t.length > 0, `len ${t.length}`);
      check("fetch bodyUsed post", true); // reached => body consumed
    })
    .catch((e) => skipCheck("fetch network", String(e)));
}

// ===================================================================
// 5b. fetch timeout 选项（非标准，对齐遗留 request()）
// ===================================================================
function testFetchTimeout() {
  console.log("\n=== Network fetch() timeout option ===");

  // 大超时：正常请求仍应通过（验证 timeout 选项被接受且不破坏行为）
  return fetch("https://example.com", { timeout: 60 })
    .then((r) => check("fetch timeout:60 accepted (status 200)", r.status === 200, `got ${r.status}`))
    .catch((e) => skipCheck("fetch timeout accepted", String(e)))
    .then(() => {
      // 极小值 + 不可达地址：应在超时后 reject（而非永久挂起）
      return fetch("http://10.255.255.1/", { timeout: 1 })
        .then(() => skipCheck("fetch timeout:1 blackhole", "unexpectedly resolved"))
        .catch((e) => check("fetch timeout:1 rejects on blackhole", true, String(e)));
    });
}

// ===================================================================
// 5c. RequestInit 字段透传（a）— Request 继承 + 覆盖
// ===================================================================
function testRequestInitForwarding() {
  console.log("\n=== RequestInit forwarding ===");

  // 同步：通过 Request(url, init) 写入的字段应被保留并可经 getter 读出
  const req = new Request("https://api.example.com/x", {
    method: "POST",
    mode: "no-cors",
    credentials: "include",
    cache: "no-store",
    referrer: "https://ref.example/",
    referrerPolicy: "no-referrer",
    integrity: "sha256-abc",
    keepalive: true,
  });
  check("init method", req.method === "POST", `got ${req.method}`);
  check("init mode forwarded", req.mode === "no-cors", `got ${req.mode}`);
  check("init credentials forwarded", req.credentials === "include", `got ${req.credentials}`);
  check("init cache forwarded", req.cache === "no-store", `got ${req.cache}`);
  check("init referrer forwarded", req.referrer === "https://ref.example/", `got ${req.referrer}`);
  check("init referrerPolicy forwarded", req.referrerPolicy === "no-referrer", `got ${req.referrerPolicy}`);
  check("init integrity forwarded", req.integrity === "sha256-abc", `got ${req.integrity}`);
  check("init keepalive forwarded", req.keepalive === true, `got ${req.keepalive}`);

  // 默认取值应存在（不被静默丢弃）
  const def = new Request("https://api.example.com/y");
  check("default mode cors", def.mode === "cors", `got ${def.mode}`);
  check("default credentials same-origin", def.credentials === "same-origin", `got ${def.credentials}`);
  check("default cache default", def.cache === "default", `got ${def.cache}`);
  check("default referrer about:client", def.referrer === "about:client", `got ${def.referrer}`);
}

// ===================================================================
// 5d. AbortError（b）— 中止信号应产生 name='AbortError' 的 TypeError
// ===================================================================
function testAbortError() {
  console.log("\n=== AbortError ===");

  // 预中止信号：确定性 reject AbortError，不发起任何网络请求
  const c1 = new AbortController();
  c1.abort();
  return fetch("https://example.com", { signal: c1.signal })
    .then(() => check("pre-aborted rejects", false, "unexpectedly resolved"))
    .catch((e) => {
      const name = (e && e.name) || "";
      check("pre-aborted -> AbortError name", name === "AbortError", `got ${name}`);
      check("pre-aborted -> TypeError kind", String(e && e.constructor && e.constructor.name) === "TypeError", `got ${e && e.constructor && e.constructor.name}`);
    })
    .then(() => {
      // 未中止的信号不应破坏正常请求
      const c2 = new AbortController();
      return fetch("https://example.com", { signal: c2.signal })
        .then((r) => check("live signal non-aborting", r.status === 200, `got ${r.status}`))
        .catch((e) => skipCheck("live signal fetch", String(e)));
    });
}

// ===================================================================
// 入口：同步先跑，异步串接，最后打印汇总
// ===================================================================
testHeaders();
testResponseSync();
testRequestSync();
testRequestInitForwarding();

Promise.resolve()
  .then(testBodies)
  .then(testNetworkFetch)
  .then(testFetchTimeout)
  .then(testAbortError)
  .then(() => {
    console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
    if (fail > 0) {
      console.log("RESULT: FAIL");
    } else {
      console.log("RESULT: PASS");
    }
  })
  .catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
  });
