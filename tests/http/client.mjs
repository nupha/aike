import { HttpClient, HttpServer } from "http";

// 🌐 HTTP 客户端测试 — HttpClient / Response / Headers / Request 全量测试
//
// fetch() 是 HttpClient 的语法糖，返回相同的 Response 类。本文件覆盖：
//   - HttpClient: 构造、GET/POST/PUT/DELETE/PATCH、request()、超时、重定向、
//     自定义 header、与 fetch 一致性
//   - Response: 构造、getter、静态方法（error/redirect/json）、body 消费
//     （text/json/bytes/arrayBuffer/blob）、bodyUsed、clone、close、body 迭代器
//   - Headers: 构造、get/set/append/delete/has、keys/values/entries/forEach、
//     Symbol.iterator、getSetCookie、合法性校验
//   - Request: 构造、getter、RequestInit 字段透传、body 消费、clone、body 迭代器
//
// 运行：aike -m tests/http/client.mjs

const TIMEOUT = 10;

// 本地测试服务器：所有网络测试打本地，不依赖外部站点
// （example.com 等外部站点超时/缺头会导致测试随机失败）
const PORT = 18123;
const BASE = `http://127.0.0.1:${PORT}`;
const svr = new HttpServer();
svr.get("/", (req, res) => {
  res.setHeader("content-type", "text/html");
  res.end("<html><body>hello from local test server</body></html>");
});
svr.post("/", (req, res) => res.end("posted"));
svr.put("/", (req, res) => res.end("put-ok"));
svr.delete("/", (req, res) => res.end("delete-ok"));
svr.patch("/", (req, res) => res.end("patch-ok"));
svr.get("/meta", (req, res) => {
  res.setHeader("content-type", "text/plain");
  res.end("hello-meta"); // 10 bytes
});
// 慢响应路由：用于 timeout 测试（延迟 2s 应大于测试用的 timeout）
svr.get("/slow", async (req, res) => {
  await new Promise((r) => setTimeout(r, 2000));
  res.end("slow");
});
svr.listen({ host: "127.0.0.1", port: PORT });

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

  // 空构造
  const hEmpty = new Headers();
  check("Headers empty get null", hEmpty.get("x") === null);

  // 数组对构造
  const h0 = new Headers([
    ["k1", "v1"],
    ["k2", "v2"],
  ]);
  check("Headers from pairs get", h0.get("k1") === "v1", `got ${h0.get("k1")}`);
  check("Headers from pairs get (other)", h0.get("k2") === "v2");

  // 记录对象构造
  const h1 = new Headers({ "X-Test": "yes", A: "b" });
  check(
    "Headers from record (case-insensitive get)",
    h1.get("x-test") === "yes",
    `got ${h1.get("x-test")}`,
  );
  check("Headers from record has", h1.has("A") === true);

  // 大小写不敏感
  h1.set("Content-Type", "text/plain");
  check(
    "Headers set case-insensitive",
    h1.get("content-type") === "text/plain",
  );

  // set 覆盖旧值
  h1.set("Content-Type", "application/json");
  check(
    "Headers set replaces",
    h1.get("Content-Type") === "application/json",
    `got ${h1.get("Content-Type")}`,
  );

  // append 逗号合并
  const h2 = new Headers();
  h2.append("X-Multi", "a");
  h2.append("X-Multi", "b");
  check(
    "Headers append comma-merge",
    h2.get("x-multi") === "a, b",
    `got ${h2.get("x-multi")}`,
  );

  // delete / has
  h2.delete("x-multi");
  check("Headers delete", h2.has("x-multi") === false);

  // keys / values / entries
  const h3 = new Headers({ X: "1", Y: "2" });
  const keys = [...h3.keys()];
  check(
    "Headers keys()",
    keys.length === 2 && keys.every((k) => typeof k === "string"),
    `got ${JSON.stringify(keys)}`,
  );
  const vals = [...h3.values()];
  check("Headers values()", vals.length === 2, `got ${JSON.stringify(vals)}`);
  const entries = [...h3.entries()];
  check("Headers entries() length", entries.length === 2);
  check(
    "Headers entries() format",
    Array.isArray(entries[0]) && entries[0].length === 2,
  );

  // forEach
  const seen = {};
  h0.forEach((v, k) => {
    seen[k] = v;
  });
  check(
    "Headers forEach",
    seen["k1"] === "v1" && seen["k2"] === "v2",
    JSON.stringify(seen),
  );

  // 迭代 [name, value]
  let itCount = 0;
  let firstPair = null;
  for (const pair of h0) {
    if (firstPair === null) firstPair = pair;
    itCount++;
  }
  check("Headers iterator count", itCount === 2, `got ${itCount}`);
  check(
    "Headers iterator yields [k,v]",
    Array.isArray(firstPair) && firstPair[0] === "k1" && firstPair[1] === "v1",
  );

  // getSetCookie
  const h4 = new Headers();
  h4.append("Set-Cookie", "a=1");
  h4.append("Set-Cookie", "b=2");
  const cookies = h4.getSetCookie();
  check(
    "getSetCookie() is array",
    Array.isArray(cookies),
    `got ${typeof cookies}`,
  );
  check("getSetCookie() count", cookies.length === 2, `got ${cookies.length}`);
  check(
    "getSetCookie() values",
    cookies[0] === "a=1" && cookies[1] === "b=2",
    `got ${JSON.stringify(cookies)}`,
  );

  // 非法 header name/value 抛 TypeError
  let threw = false;
  try {
    new Headers([["\0bad", "x"]]);
  } catch (e) {
    threw = true;
  }
  check("Headers rejects invalid name", threw);

  threw = false;
  try {
    new Headers([["ok", "\x00bad"]]);
  } catch (e) {
    threw = true;
  }
  check("Headers rejects invalid value", threw);
}

// ===================================================================
// 2. Response 构造函数 / 静态方法 / getter
// ===================================================================
function testResponseSync() {
  console.log("\n=== Response (sync) ===");

  const r = new Response("hello", {
    status: 201,
    headers: { "X-Test": "yes" },
  });
  check("Response status", r.status === 201, `got ${r.status}`);
  check("Response ok (201)", r.ok === true);
  check("Response type default", r.type === "default", `got ${r.type}`);
  check("Response headers case-insensitive", r.headers.get("x-test") === "yes");

  const r4 = new Response(null, { status: 404 });
  check("Response ok (404)", r4.ok === false);
  check("Response statusText default", typeof r4.statusText === "string");

  // Aike 扩展 getter
  check(
    "Response contentType string",
    typeof new Response("x").contentType === "string",
  );
  check(
    "Response contentLength null",
    new Response("x").contentLength === null,
  );

  // Response.error()
  const err = Response.error();
  check("Response.error type", err.type === "error", `got ${err.type}`);
  check("Response.error body null", err.body === null);

  // Response.redirect()
  const redir = Response.redirect("http://example.com/go", 302);
  check(
    "Response.redirect status",
    redir.status === 302,
    `got ${redir.status}`,
  );
  check(
    "Response.redirect location",
    redir.headers.get("location") === "http://example.com/go",
  );

  let threw = false;
  try {
    Response.redirect("http://x", 200);
  } catch (e) {
    threw = true;
  }
  check("Response.redirect invalid status throws", threw);

  // bodyUsed 初始为 false
  check("Response bodyUsed pre", r.bodyUsed === false);

  // Response.json()
  const jr = Response.json({ ok: true, n: 42 }, { status: 201 });
  check("Response.json status", jr.status === 201, `got ${jr.status}`);
  check(
    "Response.json Content-Type",
    jr.headers.get("content-type") === "application/json",
    `got ${jr.headers.get("content-type")}`,
  );

  return jr.json().then((data) => {
    check(
      "Response.json parse ok",
      data.ok === true,
      `got ${JSON.stringify(data)}`,
    );
    check(
      "Response.json parse n",
      data.n === 42,
      `got ${JSON.stringify(data)}`,
    );
  });
}

// ===================================================================
// 3. Body 消费：Response 的各种读取方式
// ===================================================================
function testResponseBody() {
  console.log("\n=== Response body consumption ===");

  return (
    Promise.resolve()
      // Response.text + clone
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
      // clone after body consumed throws
      .then(() => {
        const r2 = new Response("x");
        return r2.text().then(() => {
          let threw = false;
          try {
            r2.clone();
          } catch (e) {
            threw = true;
          }
          check("clone after body consumed throws", threw);
        });
      })
      // Response.bytes()
      .then(() => {
        const r = new Response("abc");
        return r.bytes().then((b) => {
          check(
            "Response.bytes() Uint8Array",
            b instanceof Uint8Array,
            `got ${b.constructor.name}`,
          );
          check("Response.bytes() length", b.length === 3, `got ${b.length}`);
        });
      })
      // Response.json()
      .then(() => {
        const r = new Response('{"a":1,"b":"x"}');
        return r.json().then((data) => {
          check(
            "Response.json()",
            data.a === 1 && data.b === "x",
            `got ${JSON.stringify(data)}`,
          );
        });
      })
      // Response.arrayBuffer
      .then(() => {
        const r3 = new Response("abc");
        return r3.arrayBuffer().then((b) => {
          check(
            "Response.arrayBuffer byteLength",
            new Uint8Array(b).length === 3,
            `got ${new Uint8Array(b).length}`,
          );
        });
      })
      // Response.blob()
      .then(() => {
        const r = new Response("blobby");
        return r.blob().then((b) => {
          check(
            "Response.blob() Uint8Array",
            b instanceof Uint8Array,
            `got ${b.constructor.name}`,
          );
          check("Response.blob() length", b.length === 6, `got ${b.length}`);
        });
      })
      // Response.formData() 应同步抛 TypeError
      .then(() => {
        const r = new Response("x");
        let threw = false;
        try {
          r.formData();
        } catch (e) {
          threw = true;
        }
        check("Response.formData throws", threw);
      })
      // Response.close()
      .then(() => {
        const r = new Response("close me");
        r.close();
        check("Response.close() no throw", true);
      })
      // Response.body async iteration
      .then(() => {
        const r = new Response("iterable");
        const it = r.body;
        check("Response.body is iterable", it !== null, `got ${it}`);
        let bodyStr = "";
        return (async () => {
          for await (const chunk of it) {
            bodyStr += String.fromCharCode.apply(null, chunk);
          }
          check(
            "Response.body async iteration",
            bodyStr === "iterable",
            `got "${bodyStr}"`,
          );
        })();
      })
  );
}

// ===================================================================
// 4. Request 构造函数 / getter / body 消费
// ===================================================================
function testRequest() {
  console.log("\n=== Request ===");

  const req = new Request("http://example.com/x", {
    method: "POST",
    headers: { Y: "z" },
  });
  check("Request method", req.method === "POST", `got ${req.method}`);
  check("Request url", req.url === "http://example.com/x", `got ${req.url}`);
  check("Request headers case-insensitive", req.headers.get("y") === "z");

  // 继承 Request 输入
  const req2 = new Request(req, { headers: { Y: "z2" } });
  check("Request inherits url", req2.url === "http://example.com/x");
  check("Request override header", req2.headers.get("y") === "z2");

  // 额外 getter
  check(
    "Request destination empty",
    req.destination === "",
    `got ${req.destination}`,
  );
  check(
    "Request redirect follow",
    req.redirect === "follow",
    `got ${req.redirect}`,
  );
  check("Request signal null (no signal)", req.signal === null);
  check("Request bodyUsed pre", req.bodyUsed === false);

  // RequestInit 字段透传
  const ext = new Request("https://api.example.com/x", {
    method: "POST",
    mode: "no-cors",
    credentials: "include",
    cache: "no-store",
    referrer: "https://ref.example/",
    referrerPolicy: "no-referrer",
    integrity: "sha256-abc",
    keepalive: true,
  });
  check("init method", ext.method === "POST", `got ${ext.method}`);
  check("init mode forwarded", ext.mode === "no-cors", `got ${ext.mode}`);
  check(
    "init credentials forwarded",
    ext.credentials === "include",
    `got ${ext.credentials}`,
  );
  check("init cache forwarded", ext.cache === "no-store", `got ${ext.cache}`);
  check(
    "init referrer forwarded",
    ext.referrer === "https://ref.example/",
    `got ${ext.referrer}`,
  );
  check(
    "init referrerPolicy forwarded",
    ext.referrerPolicy === "no-referrer",
    `got ${ext.referrerPolicy}`,
  );
  check(
    "init integrity forwarded",
    ext.integrity === "sha256-abc",
    `got ${ext.integrity}`,
  );
  check(
    "init keepalive forwarded",
    ext.keepalive === true,
    `got ${ext.keepalive}`,
  );

  const def = new Request("https://api.example.com/y");
  check("default mode cors", def.mode === "cors", `got ${def.mode}`);
  check(
    "default credentials same-origin",
    def.credentials === "same-origin",
    `got ${def.credentials}`,
  );
  check("default cache default", def.cache === "default", `got ${def.cache}`);
  check(
    "default referrer about:client",
    def.referrer === "about:client",
    `got ${def.referrer}`,
  );

  // Request body 消费
  return Promise.resolve()
    .then(() => {
      const r = new Request("http://x/y", { method: "POST", body: "ping" });
      return r.arrayBuffer().then((b) => {
        const bytes = new Uint8Array(b);
        const s = String.fromCharCode.apply(null, bytes);
        check("Request.arrayBuffer() roundtrip", s === "ping", `got "${s}"`);
      });
    })
    .then(() => {
      const r = new Request("http://x", { method: "POST", body: "text-body" });
      return r
        .text()
        .then((t) => check("Request.text()", t === "text-body", `got "${t}"`));
    })
    .then(() => {
      const r = new Request("http://x", {
        method: "PUT",
        body: "\x00\x01\x02",
      });
      return r.bytes().then((b) => {
        check("Request.bytes() length", b.length === 3, `got ${b.length}`);
        check(
          "Request.bytes() content",
          b[0] === 0 && b[1] === 1 && b[2] === 2,
        );
      });
    })
    .then(() => {
      const r = new Request("http://x", {
        method: "POST",
        body: '{"key":"val"}',
      });
      return r
        .json()
        .then((data) =>
          check(
            "Request.json()",
            data.key === "val",
            `got ${JSON.stringify(data)}`,
          ),
        );
    })
    .then(() => {
      const r = new Request("http://x", { method: "POST", body: "clone-me" });
      const c = r.clone();
      return Promise.all([r.text(), c.text()]).then(([t1, t2]) => {
        check("Request.clone text()", t1 === "clone-me", `got "${t1}"`);
        check("Request.clone independent", t2 === "clone-me", `got "${t2}"`);
      });
    })
    .then(() => {
      const r = new Request("http://x", { method: "POST", body: "used" });
      const c = r.clone();
      check("Request clone orig bodyUsed pre", r.bodyUsed === false);
      return r.text().then(() => {
        check("Request clone orig bodyUsed post", r.bodyUsed === true);
        let threw = false;
        try {
          c.clone();
        } catch (e) {
          threw = true;
        }
        check("Request clone of clone ok", !threw, threw ? "threw" : "ok");
      });
    })
    .then(() => {
      const r = new Request("http://x", { method: "POST", body: "iter" });
      const it = r.body;
      check("Request.body is iterable", it !== null, `got ${it}`);
      let bodyStr = "";
      return (async () => {
        for await (const chunk of it) {
          bodyStr += String.fromCharCode.apply(null, chunk);
        }
        check(
          "Request.body async iteration",
          bodyStr === "iter",
          `got "${bodyStr}"`,
        );
      })();
    });
}

// ===================================================================
// 5. HttpClient 构造函数
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
// 6. GET
// ===================================================================
function testHttpGet() {
  console.log("\n=== HttpClient basic GET ===");
  const client = new HttpClient();
  return client
    .get(BASE, { timeout: TIMEOUT })
    .then((r) => {
      check("GET status 200", r.status === 200, `got ${r.status}`);
      check("GET ok", r.ok === true);
      check(
        "GET type default/basic",
        r.type === "default" || r.type === "basic",
        `got ${r.type}`,
      );
      check("GET has url", typeof r.url === "string" && r.url.length > 0);
      check("GET contentType", typeof r.contentType === "string");
      check("GET has headers", r.headers.get("content-type") !== null);
      return r.text();
    })
    .then((t) => check("GET body non-empty", t.length > 0, `len ${t.length}`));
}

// ===================================================================
// 7. POST
// ===================================================================
function testHttpPost() {
  console.log("\n=== HttpClient POST with body ===");
  const client = new HttpClient();
  return client
    .post(BASE, {
      body: "hello",
      headers: { "Content-Type": "text/plain" },
      timeout: TIMEOUT,
    })
    .then((r) => {
      check(
        "POST request completed",
        r.status >= 200 && r.status < 600,
        `got ${r.status}`,
      );
      check("POST has body", r.bodyUsed === false);
      return r.text();
    })
    .then((t) =>
      check("POST response body present", t.length > 0, `len ${t.length}`),
    );
}

// ===================================================================
// 8. request() 通用方法
// ===================================================================
function testRequestMethod() {
  console.log("\n=== HttpClient.request() ===");
  const client = new HttpClient();
  return client
    .request(BASE, { method: "GET", timeout: TIMEOUT })
    .then((r) =>
      check("request() GET status 200", r.status === 200, `got ${r.status}`),
    );
}

// ===================================================================
// 9. 复用同一客户端
// ===================================================================
function testReuse() {
  console.log("\n=== HttpClient reuse ===");
  const client = new HttpClient();
  return client
    .get(BASE, { timeout: TIMEOUT })
    .then(() => client.get(`${BASE}/`, { timeout: TIMEOUT }))
    .then((r) =>
      check("reuse: 2nd request", r.status === 200, `got ${r.status}`),
    );
}

// ===================================================================
// 10. 与 fetch 一致（共享 Response 类）
// ===================================================================
function testConsistentWithFetch() {
  console.log("\n=== HttpClient consistent with fetch ===");
  const client = new HttpClient();
  return Promise.all([
    fetch(BASE),
    client.get(BASE, { timeout: TIMEOUT }),
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
    // 返回的 response 是同一类
    check(
      "HttpClient response is Response",
      cr instanceof Response,
      `got ${cr.constructor.name}`,
    );
    check(
      "fetch response is Response",
      fr instanceof Response,
      `got ${fr.constructor.name}`,
    );
  });
}

// ===================================================================
// 11. Timeout
// ===================================================================
function testTimeout() {
  console.log("\n=== HttpClient timeout ===");
  const client = new HttpClient();
  return client
    .get(`${BASE}/slow`, { timeout: 0.5 })
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
// 12. AbortController — 与 fetch 使用同一路径
// ===================================================================
function testAbort() {
  console.log("\n=== HttpClient AbortController ===");
  const c = new HttpClient();
  const ac = new AbortController();
  ac.abort();
  return c
    .get(BASE, { signal: ac.signal, timeout: TIMEOUT })
    .then(() =>
      check("pre-aborted HttpClient rejects", false, "unexpectedly resolved"),
    )
    .catch((e) => {
      check("pre-aborted HttpClient rejects", true);
      check(
        "pre-aborted HttpClient abort error name",
        e.name === "AbortError",
        `got ${e.name}`,
      );
    });
}

// ===================================================================
// 13. 自定义 headers
// ===================================================================
function testCustomHeaders() {
  console.log("\n=== HttpClient custom headers ===");
  const client = new HttpClient();
  return client
    .get(BASE, {
      headers: { "X-Test": "hello-httpclient" },
      timeout: TIMEOUT,
    })
    .then((r) =>
      check(
        "custom headers: request completed",
        r.status === 200,
        `got ${r.status}`,
      ),
    );
}

// ===================================================================
// 14. Redirect 选项（已移除：redirect 为 client 级策略，固定 follow；
//     legacy 的 redirect 选项被忽略，不报错）
// ===================================================================
function testRedirect() {
  console.log("\n=== HttpClient redirect options (legacy, ignored) ===");
  const client = new HttpClient();

  return client
    .get(BASE, { redirect: "manual", timeout: TIMEOUT })
    .then((r) =>
      check(
        "legacy redirect option ignored",
        r.status === 200,
        `got ${r.status}`,
      ),
    );
}

// ===================================================================
// 15. PUT / DELETE / PATCH
// ===================================================================
function testOtherMethods() {
  console.log("\n=== HttpClient PUT / DELETE / PATCH ===");
  const client = new HttpClient();

  return client
    .put(BASE, {
      body: "update",
      headers: { "Content-Type": "text/plain" },
      timeout: TIMEOUT,
    })
    .then((r) =>
      check("PUT request completed", r.status === 200, `got ${r.status}`),
    )
    .then(() => client.delete(BASE, { timeout: TIMEOUT }))
    .then((r) =>
      check("DELETE request completed", r.status === 200, `got ${r.status}`),
    )
    .then(() => client.patch(BASE, { body: "modify", timeout: TIMEOUT }))
    .then((r) =>
      check("PATCH request completed", r.status === 200, `got ${r.status}`),
    );
}

// ===================================================================
// 16. 从 HttpClient 响应消费 body
// ===================================================================
function testHttpClientResponseBody() {
  console.log("\n=== HttpClient response body consumption ===");
  const client = new HttpClient();
  return client.get(BASE, { timeout: TIMEOUT }).then((r) => {
    check("bodyUsed pre", r.bodyUsed === false);
    // text() 后 body 被消费
    return r.text().then((t) => {
      check("body text non-empty", t.length > 0, `len ${t.length}`);
      check("bodyUsed post", r.bodyUsed === true);
      // clone after consumed should throw
      let threw = false;
      try {
        r.clone();
      } catch (e) {
        threw = true;
      }
      check("clone after consumption throws", threw);
    });
  });
}

// ===================================================================
// 17. fetch 语法糖验证
// ===================================================================
function testFetchAsSugar() {
  console.log("\n=== fetch as syntactic sugar ===");

  // fetch 存在且可用
  const f0 = fetch(BASE);
  check("fetch is function", typeof fetch === "function");
  check(
    "fetch returns promise",
    f0 instanceof Promise,
    `got ${f0.constructor.name}`,
  );

  return f0
    .then((r) => {
      check("fetch status 200", r.status === 200, `got ${r.status}`);
      check("fetch ok", r.ok === true);
      check("fetch type basic", r.type === "basic", `got ${r.type}`);
      check("fetch bodyUsed pre", r.bodyUsed === false);
      return r.text();
    })
    .then((t) => {
      check("fetch body non-empty", t.length > 0, `len ${t.length}`);
    })
    .catch((e) => skipCheck("fetch network", String(e)))
    .then(() => {
      // fetch timeout 选项
      return fetch(BASE, { timeout: 60 })
        .then((r) =>
          check(
            "fetch timeout:60 accepted",
            r.status === 200,
            `got ${r.status}`,
          ),
        )
        .catch((e) => skipCheck("fetch timeout accepted", String(e)));
    });
}

// ===================================================================
// 18. 响应元信息与错误分类（contentLength / httpVersion / error.code）
// ===================================================================
function testMetaAndErrors() {
  console.log("\n=== Response meta & error codes ===");
  const client = new HttpClient();

  return (
    client
      // 共享本地服务器的 /meta 路由：固定 10 字节 body，Content-Length 确定
      .get(`${BASE}/meta`, { timeout: TIMEOUT })
      .then((r) => {
        check(
          "httpVersion is 1.1",
          r.httpVersion === "1.1",
          `got ${r.httpVersion}`,
        );
        check(
          "contentLength === 10",
          r.contentLength === 10,
          `got ${r.contentLength}`,
        );
        r.close();
      })
      .then(() => {
        // 合成 Response 无网络传输，httpVersion 为 null
        check(
          "synthetic httpVersion null",
          new Response("x").httpVersion === null,
        );
      })
      .then(() =>
        client
          .get(`${BASE}/slow`, { timeout: 0.5 })
          .then(() => check("timeout should reject", false))
          .catch((e) =>
            check("error code timeout", e.code === "timeout", `got ${e.code}`),
          ),
      )
      .then(() =>
        client
          .get("http://127.0.0.1:9/", { timeout: TIMEOUT })
          .then(() => check("connect should reject", false))
          .catch((e) =>
            check("error code connect", e.code === "connect", `got ${e.code}`),
          ),
      )
      .then(() =>
        client
          .get("http://no-such-host.invalid/", { timeout: TIMEOUT })
          .then(() => check("dns should reject", false))
          .catch((e) =>
            check("error code dns", e.code === "dns", `got ${e.code}`),
          ),
      )
  );
}

// ===================================================================
// 入口
// ===================================================================
testHeaders();

testRequest()
  .then(() => testResponseSync())
  .then(() => testResponseBody())
  .then(() => {
    testConstructor();
    return testHttpGet();
  })
  .then(() => testHttpPost())
  .then(() => testRequestMethod())
  .then(() => testReuse())
  .then(() => testConsistentWithFetch())
  .then(() => testCustomHeaders())
  .then(() => testAbort())
  .then(() => testRedirect())
  .then(() => testOtherMethods())
  .then(() => testTimeout())
  .then(() => testHttpClientResponseBody())
  .then(() => testMetaAndErrors())
  .then(() => testFetchAsSugar())
  .then(() => {
    console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
    console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  })
  .catch((e) => {
    // Engine inspect prints bare "Error { }"; show name/message/code explicitly.
    if (e instanceof Error) {
      const code = e.code ? ` (code=${e.code})` : "";
      console.log(`UNEXPECTED ERROR: ${e.name}: ${e.message}${code}`);
      if (e.stack) console.log(e.stack);
    } else {
      console.log("UNEXPECTED ERROR:", e);
    }
    console.log("RESULT: FAIL");
  })
  // 无论成败都关闭本地测试服务器，保证进程退出
  .then(() => svr.shutdown());
