// Consolidated HttpServer test server — fully curl-verifiable, no model needed.
//
// Covers: JS routes (GET/POST/PUT/DELETE + params/query/headers/body/redirect/
// crash/204), static file serving from the tests dir, SSE streaming, and a
// WebSocket echo. The LLM/OpenAI part lives in the aike-llama zapi module
// (aike-llama/examples/openai_server.mjs).
//
// Run:  aike tests/http/server.mjs
// Then verify with the printed curl commands.
//
// This is a manual fixture, not an automated test: it serves forever by design,
// so exclude it from test sweeps (and note it binds a fixed port 8080).

import { HttpServer } from "http";

const PORT = 8080;
const svr = new HttpServer();

// --- Static files: serve the whole tests dir so this file is self-contained
//     for testing (e.g. `curl :PORT/server.mjs` fetches this very file). ---
svr.staticFiles("/", "tests");

// ======================== JS Routes ========================

// Friendly landing that lists the available endpoints (curl-testable root).
svr.get("/", (req, res) => {
  res.json({
    name: "aike HttpServer test server",
    staticDir: "tests",
    routes: [
      "GET /api/hello",
      "GET /api/users/:id",
      "POST /api/users",
      "PUT /api/users/:id",
      "DELETE /api/users/:id",
      "GET /api/search?q=&page=",
      "GET /api/default-204",
      "GET /api/headers",
      "POST /api/echo-headers",
      "GET /api/redirect",
      "GET /api/crash",
      "GET /info",
      "POST /echo",
      "GET /status?code=200",
      "GET /text",
      "GET /json?x=1",
      "GET /chat (SSE)",
      "WS  /ws",
    ],
  });
});

svr.get("/api/hello", (req, res) => {
  res.json({ message: "Hello, world!" });
});

// GET /api/users/:id — path params
svr.get("/api/users/:id", (req, res) => {
  res.json({ id: req.params.id, name: "Alice" });
});

// POST /api/users — JSON body, 201 status
svr.post("/api/users", (req, res) => {
  const body = req.json();
  res.status(201).json({ id: 999, ...body });
});

// PUT /api/users/:id — text body
svr.put("/api/users/:id", (req, res) => {
  const body = req.text();
  res.json({ id: req.params.id, body });
});

// DELETE /api/users/:id — 204 No Content
svr.delete("/api/users/:id", (req, res) => {
  res.sendStatus(204);
});

// GET /api/search — query params
svr.get("/api/search", (req, res) => {
  res.json({ q: req.query.q, page: req.query.page || "1" });
});

// GET /api/default-204 — no response method called → expects 204
svr.get("/api/default-204", (req, res) => {});

// GET /api/headers — setHeader / setHeaders chaining
svr.get("/api/headers", (req, res) => {
  res
    .status(200)
    .setHeader("x-custom", "hello")
    .setHeaders({ "x-another": "world", "x-number": "42" })
    .end("check headers");
});

// POST /api/echo-headers — echo request headers
svr.post("/api/echo-headers", (req, res) => {
  res.json({ headers: req.headers });
});

// GET /api/redirect — 302 redirect
svr.get("/api/redirect", (req, res) => {
  res.redirect(302, "/api/hello");
});

// GET /api/crash — error handling test (handler throws)
svr.get("/api/crash", (req, res) => {
  throw new Error("something went wrong");
});

// GET /info — request introspection
svr.get("/info", (req, res) => {
  res.json({
    method: req.method,
    path: req.path,
    headers: req.headers,
    query: req.query,
  });
});

// POST /echo — echo JSON body
svr.post("/echo", (req, res) => {
  const body = req.json();
  res.json({ echoed: body });
});

// GET /status?code=xxx — arbitrary status code
svr.get("/status", (req, res) => {
  const code = parseInt(req.query.code) || 200;
  res.sendStatus(code);
});

// --- Plain text ---
svr.get("/text", (req, res) => {
  res.setHeader("content-type", "text/plain; charset=utf-8");
  res.end("hello from aike http server");
});

// --- JSON + query echo ---
svr.get("/json", (req, res) => {
  res.json({ ok: true, path: req.path, method: req.method, query: req.query });
});

// --- SSE stream: GET /chat (5 frames, ~200ms apart) ---
svr.get("/chat", (req, res) => {
  const sse = res.sse();
  let i = 0;
  const timer = setInterval(() => {
    i += 1;
    sse.write("event " + i + " @ " + new Date().toISOString());
    if (i >= 5) {
      clearInterval(timer);
      sse.close();
    }
  }, 200);
});

// --- WebSocket echo: ws://127.0.0.1:PORT/ws (use websocat / node client) ---
svr.ws("/ws", async (stream) => {
  console.log("[ws] opened id=", stream.id, "readyState=", stream.readyState);
  try {
    for await (const msg of stream) {
      console.log("[ws] recv:", msg);
      await stream.write("echo: " + msg);
    }
  } catch (e) {
    console.log("[ws] loop error:", e);
  }
  console.log("[ws] closed");
});

svr.listen({ host: "0.0.0.0", port: PORT });

console.log(`[server] listening on http://127.0.0.1:${PORT}`);
console.log("");
console.log("curl test commands:");
console.log(`  landing      : curl -s http://127.0.0.1:${PORT}/`);
console.log(`  static self  : curl -s http://127.0.0.1:${PORT}/http/server.mjs | head -3`);
console.log(`  hello        : curl -s http://127.0.0.1:${PORT}/api/hello`);
console.log(`  path param   : curl -s http://127.0.0.1:${PORT}/api/users/42`);
console.log(`  post json    : curl -s -X POST -H 'content-type: application/json' -d '{"name":"test"}' http://127.0.0.1:${PORT}/api/users`);
console.log(`  put text     : curl -s -X PUT -d 'some text' http://127.0.0.1:${PORT}/api/users/42`);
console.log(`  delete       : curl -s -o /dev/null -w '%{http_code}\\n' -X DELETE http://127.0.0.1:${PORT}/api/users/1`);
console.log(`  query        : curl -s 'http://127.0.0.1:${PORT}/api/search?q=rust&page=2'`);
console.log(`  default-204  : curl -s -o /dev/null -w '%{http_code}\\n' http://127.0.0.1:${PORT}/api/default-204`);
console.log(`  custom hdr   : curl -s -D - http://127.0.0.1:${PORT}/api/headers | grep -i x-`);
console.log(`  echo hdr     : curl -s -X POST http://127.0.0.1:${PORT}/api/echo-headers`);
console.log(`  redirect     : curl -s -o /dev/null -w '%{http_code} -> %{redirect_url}\\n' http://127.0.0.1:${PORT}/api/redirect`);
console.log(`  crash        : curl -s -o /dev/null -w '%{http_code}\\n' http://127.0.0.1:${PORT}/api/crash`);
console.log(`  info         : curl -s http://127.0.0.1:${PORT}/info`);
console.log(`  echo body    : curl -s -X POST -H 'content-type: application/json' -d '{"foo":"bar"}' http://127.0.0.1:${PORT}/echo`);
console.log(`  status code  : curl -s -o /dev/null -w '%{http_code}\\n' 'http://127.0.0.1:${PORT}/status?code=418'`);
console.log(`  text         : curl -s http://127.0.0.1:${PORT}/text`);
console.log(`  json+query   : curl -s "http://127.0.0.1:${PORT}/json?x=1"`);
console.log(`  sse stream   : curl -sN http://127.0.0.1:${PORT}/chat`);
console.log(`  websocket    : websocat ws://127.0.0.1:${PORT}/ws`);

await new Promise(() => {});
