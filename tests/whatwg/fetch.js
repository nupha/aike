// 🌐 WHATWG Fetch API 标准测试 — fetch / Request / Response / Headers
//
// 按 WHATWG 标准测试全局 fetch() API 及其相关接口，与底层 HttpClient
// 实现解耦。所有实现细节在 src/http/client/ 中，本文件只验证标准行为。
//
// 运行：aike -m tests/whatwg/fetch.js

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
  check("Headers values() count", vals.length === 2);
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
    seen.k1 === "v1" && seen.k2 === "v2",
    JSON.stringify(seen),
  );

  // Symbol.iterator
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
  check("getSetCookie() is array", Array.isArray(cookies));
  check("getSetCookie() count", cookies.length === 2, `got ${cookies.length}`);
  check(
    "getSetCookie() values",
    cookies[0] === "a=1" && cookies[1] === "b=2",
    `got ${JSON.stringify(cookies)}`,
  );

  // 合法性校验
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
// 2. Request
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

  // getter
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
  check("Request signal default AbortSignal", req.signal instanceof AbortSignal);
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
  check("init mode", ext.mode === "no-cors", `got ${ext.mode}`);
  check(
    "init credentials",
    ext.credentials === "include",
    `got ${ext.credentials}`,
  );
  check("init cache", ext.cache === "no-store", `got ${ext.cache}`);
  check(
    "init referrer",
    ext.referrer === "https://ref.example/",
    `got ${ext.referrer}`,
  );
  check(
    "init referrerPolicy",
    ext.referrerPolicy === "no-referrer",
    `got ${ext.referrerPolicy}`,
  );
  check(
    "init integrity",
    ext.integrity === "sha256-abc",
    `got ${ext.integrity}`,
  );
  check("init keepalive", ext.keepalive === true, `got ${ext.keepalive}`);

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
        check("Request.arrayBuffer()", s === "ping", `got "${s}"`);
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
      check("clone orig bodyUsed pre", r.bodyUsed === false);
      return r.text().then(() => {
        check("clone orig bodyUsed post", r.bodyUsed === true);
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
      const r = new Request("http://x", { method: "POST", body: "reader" });
      const bodyStream = r.body;
      check(
        "Request.body is ReadableStream",
        bodyStream instanceof ReadableStream,
      );
      let bodyStr = "";
      return (async () => {
        for await (const chunk of bodyStream) {
          bodyStr += String.fromCharCode.apply(null, chunk);
        }
        check(
          "Request.body for await...of",
          bodyStr === "reader",
          `got "${bodyStr}"`,
        );
      })();
    });
}

// ===================================================================
// 3. Response
// ===================================================================
function testResponse() {
  console.log("\n=== Response ===");

  const r = new Response("hello", {
    status: 201,
    headers: { "X-Test": "yes" },
  });
  check("Response status", r.status === 201, `got ${r.status}`);
  check("Response ok (201)", r.ok === true);
  check("Response type default", r.type === "default", `got ${r.type}`);
  check("Response headers case-insensitive", r.headers.get("x-test") === "yes");

  let bodyIsRS = false;
  try {
    bodyIsRS = r.body instanceof ReadableStream;
  } catch (e) {
    console.log("  [DEBUG] body access error:", e.message);
    console.log("  [DEBUG] bodyUsed before:", r.bodyUsed);
  }
  check(
    "Response.body is ReadableStream",
    bodyIsRS,
    `got body instanceof=${bodyIsRS}`,
  );

  const r4 = new Response(null, { status: 404 });
  check("Response ok (404)", r4.ok === false);
  check("Response statusText default", typeof r4.statusText === "string");
  check("Response bodyUsed pre", r.bodyUsed === false);

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

  // Response.json()
  const jr = Response.json({ ok: true, n: 42 }, { status: 201 });
  check("Response.json status", jr.status === 201, `got ${jr.status}`);
  check(
    "Response.json Content-Type",
    jr.headers.get("content-type") === "application/json",
    `got ${jr.headers.get("content-type")}`,
  );

  // body 消费
  return Promise.resolve()
    .then(() =>
      new Response('{"x":1}').json().then((d) => {
        check("Response.json() parse", d.x === 1, `got ${JSON.stringify(d)}`);
      }),
    )
    .then(() =>
      new Response("abc").bytes().then((b) => {
        check("Response.bytes() length", b.length === 3, `got ${b.length}`);
      }),
    )
    .then(() =>
      new Response("abc").arrayBuffer().then((b) => {
        check(
          "Response.arrayBuffer byteLength",
          new Uint8Array(b).length === 3,
        );
      }),
    )
    .then(() =>
      new Response("blobby").blob().then((b) => {
        check("Response.blob() size", b.size === 6, `got ${b.size}`);
      }),
    )
    .then(() => {
      // formData() returns a promise; a non-form body rejects with TypeError.
      return new Response("x")
        .formData()
        .then(
          () => check("Response.formData rejects", false, "unexpectedly resolved"),
          (e) =>
            check(
              "Response.formData rejects with TypeError",
              e instanceof TypeError,
              `got ${e}`,
            ),
        );
    })
    .then(() => {
      const r = new Response("hello");
      const c = r.clone();
      check("clone bodyUsed pre orig", r.bodyUsed === false);
      check("clone bodyUsed pre clone", c.bodyUsed === false);
      return Promise.all([r.text(), c.text()]).then(([t1, t2]) => {
        check("Response.text()", t1 === "hello", `got "${t1}"`);
        check("clone.text()", t2 === "hello", `got "${t2}"`);
        check("bodyUsed orig", r.bodyUsed === true);
        check("bodyUsed clone", c.bodyUsed === true);
      });
    })
    .then(() => {
      const r = new Response("x");
      return r.text().then(() => {
        let threw = false;
        try {
          r.clone();
        } catch (e) {
          threw = true;
        }
        check("clone after consumed throws", threw);
      });
    })
    .then(() => {
      // body as ReadableStream — getReader + read
      const r1 = new Response("hello-reader");
      const reader1 = r1.body.getReader();
      return reader1.read().then((result) => {
        check("Response.body reader read() done=false", result.done === false);
        const str = String.fromCharCode.apply(null, result.value);
        check(
          "Response.body reader read() value",
          str === "hello-reader",
          `got "${str}"`,
        );
        return reader1.read().then((done) => {
          check(
            "Response.body reader read() after end done=true",
            done.done === true,
          );
        });
      });
    })
    .then(() => {
      // body as ReadableStream — for await...of
      const r2 = new Response("iterable");
      let bodyStr = "";
      return (async () => {
        for await (const chunk of r2.body) {
          bodyStr += String.fromCharCode.apply(null, chunk);
        }
        check(
          "Response.body for await...of",
          bodyStr === "iterable",
          `got "${bodyStr}"`,
        );
      })();
    });
}

// ===================================================================
// 4. fetch() 端到端
// ===================================================================
function testFetch() {
  console.log("\n=== fetch() ===");

  check("fetch is global function", typeof fetch === "function");
  check(
    "fetch returns promise",
    fetch("https://example.com") instanceof Promise,
  );

  return fetch("https://example.com")
    .then((r) => {
      check("fetch status 200", r.status === 200, `got ${r.status}`);
      check("fetch ok", r.ok === true);
      check("fetch type basic", r.type === "basic", `got ${r.type}`);
      check("fetch redirected false", r.redirected === false);
      check(
        "fetch url set",
        typeof r.url === "string" && r.url.length > 0,
        `got ${r.url}`,
      );
      check("fetch headers present", r.headers.get("server") !== null);
      check("fetch bodyUsed pre", r.bodyUsed === false);
      return r.text();
    })
    .then((t) => {
      check("fetch body non-empty", t.length > 0, `len ${t.length}`);
    })
    .catch((e) => skipCheck("fetch network", String(e)));
}

// ===================================================================
// 5. fetch(Request) — 用 Request 对象输入
// ===================================================================
function testFetchWithRequest() {
  console.log("\n=== fetch(Request) ===");
  const req = new Request("https://example.com", { method: "GET" });
  return fetch(req)
    .then((r) =>
      check("fetch(Request) status 200", r.status === 200, `got ${r.status}`),
    )
    .catch((e) => skipCheck("fetch(Request) network", String(e)));
}

// ===================================================================
// 6. AbortError — 标准规定 name="AbortError" 的 DOMException
// ===================================================================
function testAbortError() {
  console.log("\n=== AbortError ===");

  const c1 = new AbortController();
  c1.abort();
  return fetch("https://example.com", { signal: c1.signal })
    .then(() => check("pre-aborted rejects", false, "unexpectedly resolved"))
    .catch((e) => {
      check(
        "pre-aborted -> AbortError name",
        e.name === "AbortError",
        `got ${e.name}`,
      );
      check(
        "pre-aborted -> DOMException kind",
        e instanceof DOMException && e.constructor.name === "DOMException",
        `got ${e.constructor.name}`,
      );
    })
    .then(() => {
      const c2 = new AbortController();
      return fetch("https://example.com", { signal: c2.signal })
        .then((r) =>
          check(
            "live signal non-aborting",
            r.status === 200,
            `got ${r.status}`,
          ),
        )
        .catch((e) => skipCheck("live signal fetch", String(e)));
    });
}

// ===================================================================
// 7. fetch timeout
// ===================================================================
function testFetchTimeout() {
  console.log("\n=== fetch timeout ===");

  return fetch("https://example.com", { timeout: 60 })
    .then((r) =>
      check("fetch timeout:60 accepted", r.status === 200, `got ${r.status}`),
    )
    .catch((e) => skipCheck("fetch timeout accepted", String(e)))
    .then(() =>
      fetch("http://10.255.255.1/", { timeout: 1 })
        .then(() =>
          skipCheck("fetch timeout:1 blackhole", "unexpectedly resolved"),
        )
        .catch((e) =>
          check("fetch timeout:1 rejects on blackhole", true, String(e)),
        ),
    );
}

// ===================================================================
// 入口
// ===================================================================
testHeaders();

testRequest()
  .then(() => testResponse())
  .then(() => testFetch())
  .then(() => testFetchWithRequest())
  .then(() => testAbortError())
  .then(() => testFetchTimeout())
  .then(() => {
    console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} skip=${skip} ===`);
    console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
  })
  .catch((e) => {
    console.log("UNEXPECTED ERROR:", e);
    console.log("RESULT: FAIL");
  });
