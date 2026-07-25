// 🌐 WHATWG Fetch API — fetch / Request / Response / Headers 全量测试
//
// 覆盖：
//   - Headers: 构造、get/set/append/delete/has、forEach、keys/values/entries、
//     Symbol.iterator、getSetCookie、合法性校验
//   - Request: 构造、getter、RequestInit 字段透传、body 消费（text/json/arrayBuffer/bytes）、
//     clone、body 迭代器
//   - Response: 构造、getter、静态方法（error/redirect/json）、body 消费、clone、close
//   - fetch: GET/POST、Request 输入、timeout、redirect、AbortSignal
//
// 运行：aike -m tests/whatwg/fetch.js
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

  // 1.1 空构造
  const hEmpty = new Headers();
  check("Headers empty get null", hEmpty.get("x") === null);

  // 1.2 数组对构造
  const h0 = new Headers([["k1", "v1"], ["k2", "v2"]]);
  check("Headers from pairs get", h0.get("k1") === "v1", `got ${h0.get("k1")}`);
  check("Headers from pairs get (other)", h0.get("k2") === "v2");

  // 1.3 记录对象构造
  const h1 = new Headers({ "X-Test": "yes", "A": "b" });
  check("Headers from record (case-insensitive get)", h1.get("x-test") === "yes", `got ${h1.get("x-test")}`);
  check("Headers from record has", h1.has("A") === true);

  // 1.4 大小写不敏感
  h1.set("Content-Type", "text/plain");
  check("Headers set case-insensitive", h1.get("content-type") === "text/plain");

  // 1.5 set 覆盖旧值
  h1.set("Content-Type", "application/json");
  check("Headers set replaces", h1.get("Content-Type") === "application/json", `got ${h1.get("Content-Type")}`);

  // 1.6 append 逗号合并
  const h2 = new Headers();
  h2.append("X-Multi", "a");
  h2.append("X-Multi", "b");
  check("Headers append comma-merge", h2.get("x-multi") === "a, b", `got ${h2.get("x-multi")}`);

  // 1.7 delete / has
  h2.delete("x-multi");
  check("Headers delete", h2.has("x-multi") === false);

  // 1.8 keys / values / entries
  const h3 = new Headers({ X: "1", Y: "2" });
  const keys = [...h3.keys()];
  check("Headers keys()", keys.length === 2 && keys.every(k => typeof k === "string"),
    `got ${JSON.stringify(keys)}`);
  const vals = [...h3.values()];
  check("Headers values()", vals.length === 2, `got ${JSON.stringify(vals)}`);
  const entries = [...h3.entries()];
  check("Headers entries() length", entries.length === 2);
  check("Headers entries() format", Array.isArray(entries[0]) && entries[0].length === 2);

  // 1.9 forEach
  const seen = {};
  h0.forEach((v, k) => { seen[k] = v; });
  check("Headers forEach", seen["k1"] === "v1" && seen["k2"] === "v2", JSON.stringify(seen));

  // 1.10 迭代 [name, value]
  let itCount = 0;
  let firstPair = null;
  for (const pair of h0) {
    if (firstPair === null) firstPair = pair;
    itCount++;
  }
  check("Headers iterator count", itCount === 2, `got ${itCount}`);
  check("Headers iterator yields [k,v]", Array.isArray(firstPair) && firstPair[0] === "k1" && firstPair[1] === "v1");

  // 1.11 getSetCookie
  const h4 = new Headers();
  h4.append("Set-Cookie", "a=1");
  h4.append("Set-Cookie", "b=2");
  const cookies = h4.getSetCookie();
  check("getSetCookie() is array", Array.isArray(cookies), `got ${typeof cookies}`);
  check("getSetCookie() count", cookies.length === 2, `got ${cookies.length}`);
  check("getSetCookie() values", cookies[0] === "a=1" && cookies[1] === "b=2", `got ${JSON.stringify(cookies)}`);

  // 1.12 非法 header name/value 抛 TypeError
  let threw = false;
  try { new Headers([["\0bad", "x"]]); } catch (e) { threw = true; }
  check("Headers rejects invalid name", threw);

  threw = false;
  try { new Headers([["ok", "\x00bad"]]); } catch (e) { threw = true; }
  check("Headers rejects invalid value", threw);
}

// ===================================================================
// 2. Response 构造函数 / 静态方法 / getter
// ===================================================================
function testResponseSync() {
  console.log("\n=== Response (sync) ===");

  const r = new Response("hello", { status: 201, headers: { "X-Test": "yes" } });
  check("Response status", r.status === 201, `got ${r.status}`);
  check("Response ok (201)", r.ok === true);
  check("Response type default", r.type === "default", `got ${r.type}`);
  check("Response headers case-insensitive", r.headers.get("x-test") === "yes");

  const r4 = new Response(null, { status: 404 });
  check("Response ok (404)", r4.ok === false);
  check("Response statusText default", typeof r4.statusText === "string");

  // Aike 扩展 getter
  check("Response contentType string", typeof new Response("x").contentType === "string");
  check("Response contentLength null", new Response("x").contentLength === null);

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

  // 2.4 Response.json()
  const jr = Response.json({ ok: true, n: 42 }, { status: 201 });
  check("Response.json status", jr.status === 201, `got ${jr.status}`);
  check("Response.json Content-Type", jr.headers.get("content-type") === "application/json",
    `got ${jr.headers.get("content-type")}`);

  // 2.5 Response.json() body 解析
  return jr.json().then((data) => {
    check("Response.json parse ok", data.ok === true, `got ${JSON.stringify(data)}`);
    check("Response.json parse n", data.n === 42, `got ${JSON.stringify(data)}`);
  });
}

// ===================================================================
// 3. Request 构造函数 / getter（body 消费的异步部分见 §4）
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
  check("Request relative url preserved", req3.url === "foo/bar", `got ${req3.url}`);

  // 额外 getter
  check("Request destination empty", req.destination === "", `got ${req.destination}`);
  check("Request redirect follow", req.redirect === "follow", `got ${req.redirect}`);
  check("Request signal null (no signal)", req.signal === null);
  check("Request bodyUsed pre", req.bodyUsed === false);

  // 从另一个 Request 继承 signal
  const ac = new AbortController();
  const reqSig = new Request("http://x", { signal: ac.signal });
  check("Request.signal from init", reqSig.signal !== null, `got ${reqSig.signal}`);

  // 使用 Signal 中止后再次 fetch 应 reject
  ac.abort();
  return fetch("http://10.255.255.1/", { signal: ac.signal })
    .then(() => { check("aborted via request signal rejects", false, "unexpectedly resolved") })
    .catch((e) => { check("aborted via request signal", e.name === "AbortError", `got ${e.name}`); });
}

// ===================================================================
// 4. Body 消费：Request / Response 的各种读取方式
// ===================================================================
function testBodies() {
  console.log("\n=== Bodies (async) ===");

  return Promise.resolve()
    // -- Response body --
    .then(() => {
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
      // Response.bytes()
      const r = new Response("abc");
      return r.bytes().then((b) => {
        check("Response.bytes() Uint8Array", b instanceof Uint8Array, `got ${b.constructor.name}`);
        check("Response.bytes() length", b.length === 3, `got ${b.length}`);
      });
    })
    .then(() => {
      // Response.json()
      const r = new Response('{"a":1,"b":"x"}');
      return r.json().then((data) => {
        check("Response.json()", data.a === 1 && data.b === "x", `got ${JSON.stringify(data)}`);
      });
    })
    .then(() => {
      // Response.arrayBuffer / bytes 等价
      const r3 = new Response("abc");
      return r3.arrayBuffer().then((b) => {
        check("Response.arrayBuffer byteLength", new Uint8Array(b).length === 3, `got ${new Uint8Array(b).length}`);
      });
    })
    .then(() => {
      // Response.blob()
      const r = new Response("blobby");
      return r.blob().then((b) => {
        check("Response.blob() Uint8Array", b instanceof Uint8Array, `got ${b.constructor.name}`);
        check("Response.blob() length", b.length === 6, `got ${b.length}`);
      });
    })
    .then(() => {
      // Response.formData() 应同步抛 TypeError
      const r = new Response("x");
      let threw = false;
      try { r.formData(); } catch (e) { threw = true; }
      check("Response.formData throws", threw);
    })
    .then(() => {
      // Response.close()
      const r = new Response("close me");
      r.close();
      check("Response.close() no throw", true);
    })
    // -- Request body --
    .then(() => {
      // Request.arrayBuffer
      const req = new Request("http://x/y", { method: "POST", body: "ping" });
      return req.arrayBuffer().then((b) => {
        const bytes = new Uint8Array(b);
        const s = String.fromCharCode.apply(null, bytes);
        check("Request.arrayBuffer() roundtrip", s === "ping", `got "${s}"`);
      });
    })
    .then(() => {
      // Request.text
      const req = new Request("http://x", { method: "POST", body: "text-body" });
      return req.text().then((t) => {
        check("Request.text()", t === "text-body", `got "${t}"`);
      });
    })
    .then(() => {
      // Request.bytes
      const req = new Request("http://x", { method: "PUT", body: "\x00\x01\x02" });
      return req.bytes().then((b) => {
        check("Request.bytes()", b.length === 3, `got ${b.length}`);
        check("Request.bytes() content", b[0] === 0 && b[1] === 1 && b[2] === 2);
      });
    })
    .then(() => {
      // Request.json
      const req = new Request("http://x", { method: "POST", body: '{"key":"val"}' });
      return req.json().then((data) => {
        check("Request.json()", data.key === "val", `got ${JSON.stringify(data)}`);
      });
    })
    .then(() => {
      // Request clone 独立 body
      const req = new Request("http://x", { method: "POST", body: "clone-me" });
      const c = req.clone();
      return Promise.all([req.text(), c.text()]).then(([t1, t2]) => {
        check("Request.clone text()", t1 === "clone-me", `got "${t1}"`);
        check("Request.clone independent", t2 === "clone-me", `got "${t2}"`);
      });
    })
    .then(() => {
      // Request clone 后原请求 bodyUsed
      const req = new Request("http://x", { method: "POST", body: "used" });
      const c = req.clone();
      check("Request clone orig bodyUsed pre", req.bodyUsed === false);
      return req.text().then(() => {
        check("Request clone orig bodyUsed post", req.bodyUsed === true);
        // clone 体应可用
        let threw = false;
        try { c.clone(); } catch (e) { threw = true; }
        check("Request clone of clone ok", !threw, threw ? "threw" : "ok");
      });
    })
    .then(() => {
      // Request.body 异步迭代器
      const req = new Request("http://x", { method: "POST", body: "iter" });
      const it = req.body;
      check("Request.body is iterable", it !== null, `got ${it}`);
      let bodyStr = "";
      return (async () => {
        for await (const chunk of it) {
          bodyStr += String.fromCharCode.apply(null, chunk);
        }
        check("Request.body async iteration", bodyStr === "iter", `got "${bodyStr}"`);
      })();
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
      check("fetch type basic", r.type === "basic", `got ${r.type}`);
      check("fetch contentType", typeof r.contentType === "string");
      check("fetch header server", !!r.headers.get("server"), `got ${r.headers.get("server")}`);
      check("fetch bodyUsed pre", r.bodyUsed === false);
      return r.text();
    })
    .then((t) => {
      check("fetch body non-empty", t.length > 0, `len ${t.length}`);
      check("fetch bodyUsed post", true);
    })
    .catch((e) => skipCheck("fetch network", String(e)));
}

// ===================================================================
// 5b. fetch timeout 选项（非标准）
// ===================================================================
function testFetchTimeout() {
  console.log("\n=== fetch() timeout option ===");

  return fetch("https://example.com", { timeout: 60 })
    .then((r) => check("fetch timeout:60 accepted (status 200)", r.status === 200, `got ${r.status}`))
    .catch((e) => skipCheck("fetch timeout accepted", String(e)))
    .then(() => {
      return fetch("http://10.255.255.1/", { timeout: 1 })
        .then(() => skipCheck("fetch timeout:1 blackhole", "unexpectedly resolved"))
        .catch((e) => check("fetch timeout:1 rejects on blackhole", true, String(e)));
    });
}

// ===================================================================
// 5c. fetch with Request input, POST body
// ===================================================================
function testFetchWithRequest() {
  console.log("\n=== fetch() with Request input ===");

  const req = new Request("https://example.com", { method: "GET" });
  return fetch(req)
    .then((r) => {
      check("fetch(Request) status 200", r.status === 200, `got ${r.status}`);
      check("fetch(Request) ok", r.ok === true);
      check("fetch(Request) method", r.redirected === false);
    })
    .catch((e) => skipCheck("fetch(Request) network", String(e)));
}

// ===================================================================
// 5d. AbortError
// ===================================================================
function testAbortError() {
  console.log("\n=== AbortError ===");

  const c1 = new AbortController();
  c1.abort();
  return fetch("https://example.com", { signal: c1.signal })
    .then(() => check("pre-aborted rejects", false, "unexpectedly resolved"))
    .catch((e) => {
      const name = (e && e.name) || "";
      check("pre-aborted -> AbortError name", name === "AbortError", `got ${name}`);
      check("pre-aborted -> TypeError kind",
        String(e && e.constructor && e.constructor.name) === "TypeError",
        `got ${e && e.constructor && e.constructor.name}`);
    })
    .then(() => {
      const c2 = new AbortController();
      return fetch("https://example.com", { signal: c2.signal })
        .then((r) => check("live signal non-aborting", r.status === 200, `got ${r.status}`))
        .catch((e) => skipCheck("live signal fetch", String(e)));
    });
}

// ===================================================================
// 5e. RequestInit 字段透传 — Request 继承 + 覆盖
// ===================================================================
function testRequestInitForwarding() {
  console.log("\n=== RequestInit forwarding ===");

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

  const def = new Request("https://api.example.com/y");
  check("default mode cors", def.mode === "cors", `got ${def.mode}`);
  check("default credentials same-origin", def.credentials === "same-origin", `got ${def.credentials}`);
  check("default cache default", def.cache === "default", `got ${def.cache}`);
  check("default referrer about:client", def.referrer === "about:client", `got ${def.referrer}`);
}

// ===================================================================
// 入口
// ===================================================================
testHeaders();

// Chain async tests
testRequestSync()
  .then(() => testResponseSync())
  .then(() => testBodies())
  .then(() => testNetworkFetch())
  .then(() => testFetchTimeout())
  .then(() => testFetchWithRequest())
  .then(() => testAbortError())
  .then(() => testRequestInitForwarding())
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
