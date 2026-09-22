import { LlmClient } from "ai";
import { HttpServer } from "http";

// client_abort_test.mjs — LlmClient AbortSignal (opts.signal, second argument) tests
// against a built-in mock OpenAI-compatible server. Self-contained:
//   ./target/debug/aike tests/llm/client_abort_test.mjs

let pass = 0;
let fail = 0;
function check(label, cond, detail) {
  if (cond) {
    console.log("PASS " + label);
    pass++;
  } else {
    console.log("FAIL " + label + (detail ? " — " + detail : ""));
    fail++;
  }
}

const PORT = 18142;
const BASE = `http://127.0.0.1:${PORT}/v1`;
const ABORT_MSG = "request aborted by AbortSignal";

// Server-side observability, reset per scenario.
let hits = 0;
let bodyKeys = null; // keys of last request body (to prove control opts never leak)
let lastClosed = false;
let closeWaiters = [];

function onServerClose() {
  lastClosed = true;
  const ws = closeWaiters;
  closeWaiters = [];
  for (const w of ws) w(true);
}

function waitClosed(ms = 3000) {
  if (lastClosed) return Promise.resolve(true);
  return new Promise((r) => {
    const t = setTimeout(() => r(false), ms);
    closeWaiters.push((ok) => {
      clearTimeout(t);
      r(ok);
    });
  });
}

// sse.write() already emits SSE `data:` framing — never prefix it manually.
function mkChunk(delta, finish = null) {
  return JSON.stringify({
    id: "chatcmpl-abort",
    object: "chat.completion.chunk",
    model: "mock",
    choices: [{ index: 0, delta, finish_reason: finish }],
  });
}

const svr = new HttpServer();

svr.post("/v1/chat/completions", async (req, res) => {
  hits++;
  lastClosed = false;
  const body = await req.json();
  bodyKeys = Object.keys(body);
  req.onClose(onServerClose);

  if (!body.stream) {
    // Slow non-stream response: leave time for an in-flight abort.
    await Aike.sleep(1500);
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        id: "chatcmpl-n",
        object: "chat.completion",
        model: "mock",
        choices: [
          { index: 0, message: { role: "assistant", content: "slow reply" }, finish_reason: "stop" },
        ],
      }),
    );
    return;
  }

  // Slow SSE: a frame every 100ms, 50 frames total (~5s) — long enough to
  // abort mid-stream or break early.
  const sse = res.sse();
  sse.onClose(onServerClose);
  let i = 0;
  const timer = setInterval(() => {
    i += 1;
    sse.write(mkChunk({ content: "c" + i }));
    if (i >= 50) {
      clearInterval(timer);
      sse.write("[DONE]");
      sse.close();
    }
  }, 100);
  req.onClose(() => clearInterval(timer));
});

svr.post("/v1/responses", async (req, res) => {
  hits++;
  lastClosed = false;
  const body = await req.json();
  bodyKeys = Object.keys(body);
  req.onClose(onServerClose);

  if (!body.stream) {
    await Aike.sleep(1500);
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        id: "resp_1",
        object: "response",
        model: "mock",
        status: "completed",
        output: [],
      }),
    );
    return;
  }

  const sse = res.sse();
  sse.onClose(onServerClose);
  let i = 0;
  const timer = setInterval(() => {
    i += 1;
    sse.write(
      JSON.stringify({
        // ResponseTextDeltaEvent requires sequence_number — frames without it
        // are silently dropped by the client's `if let Ok(event)` parse.
        sequence_number: i,
        type: "response.output_text.delta",
        delta: "c" + i,
        item_id: "msg_1",
        output_index: 0,
        content_index: 0,
      }),
    );
    if (i >= 50) {
      clearInterval(timer);
      sse.write("[DONE]");
      sse.close();
    }
  }, 100);
  req.onClose(() => clearInterval(timer));
});

globalThis.mockHandle = await svr.listen({ host: "127.0.0.1", port: PORT });
await Aike.sleep(200);

const cli = new LlmClient({ baseUrl: BASE, apiKey: "k", model: "mock" });

function reset() {
  hits = 0;
  bodyKeys = null;
  lastClosed = false;
  closeWaiters = [];
}

// ---- 1. invalid signal type → TypeError ----
{
  let e1, e2;
  try {
    cli.chatOnce({ messages: [] }, { signal: {} });
  } catch (e) {
    e1 = e;
  }
  try {
    cli.chat({ messages: [] }, { signal: "nope" });
  } catch (e) {
    e2 = e;
  }
  check("chatOnce non-AbortSignal throws TypeError", e1 instanceof TypeError, String(e1));
  check("chat non-AbortSignal throws TypeError", e2 instanceof TypeError, String(e2));
}

// ---- 2. pre-aborted chatOnce: rejects with reason, request never sent ----
{
  reset();
  const reason = new Error("user cancelled");
  const c = new AbortController();
  c.abort(reason);
  let got;
  try {
    await cli.chatOnce({ messages: [] }, { signal: c.signal });
  } catch (e) {
    got = e;
  }
  check("pre-aborted chatOnce rejects", got !== undefined, String(got));
  check("pre-aborted chatOnce rejects with signal reason", got === reason, String(got));
  check("pre-aborted chatOnce sends no request", hits === 0, `hits=${hits}`);
}

// ---- 3. pre-aborted chat stream: iterator rejects with ABORT_MSG, no request ----
{
  reset();
  const c = new AbortController();
  c.abort();
  let got;
  let threw = false;
  try {
    for await (const _ of cli.chat({ messages: [] }, { signal: c.signal })) {
      // should not yield
    }
  } catch (e) {
    threw = true;
    got = e;
  }
  check("pre-aborted chat stream rejects", threw, "iterator completed without error");
  check(
    "pre-aborted chat stream abort message",
    got && String(got.message || got).includes(ABORT_MSG),
    `got ${got && (got.message || got)}`,
  );
  check("pre-aborted chat stream sends no request", hits === 0, `hits=${hits}`);
}

// ---- 4. in-flight abort chatOnce: prompt reject + upstream dropped ----
{
  reset();
  const c = new AbortController();
  const p = cli.chatOnce({ messages: [] }, { signal: c.signal });
  await Aike.sleep(300); // request in flight, server sleeping
  check("chatOnce request reached server", hits === 1, `hits=${hits}`);
  const t0 = Date.now();
  c.abort();
  let got;
  try {
    await p;
  } catch (e) {
    got = e;
  }
  const elapsed = Date.now() - t0;
  check(
    "in-flight chatOnce rejects with abort message",
    got && String(got.message || got).includes(ABORT_MSG),
    `got ${got && (got.message || got)}`,
  );
  check("in-flight chatOnce rejects promptly", elapsed < 1000, `took ${elapsed}ms`);
  // No server-side close check here: aike arms the disconnect watcher only on
  // the SSE path, so req.onClose never fires for a non-stream res.end().
  // (The TCP drop itself is real — verified with lsof during development.)
}

// ---- 5. in-flight abort chat stream: iterator rejects + upstream dropped ----
{
  reset();
  const c = new AbortController();
  const chunks = [];
  let got;
  let threw = false;
  try {
    for await (const chunk of cli.chat({ messages: [] }, { signal: c.signal })) {
      chunks.push(chunk);
      if (chunks.length === 2) c.abort();
    }
  } catch (e) {
    threw = true;
    got = e;
  }
  check("stream yielded before abort", chunks.length >= 2, `got ${chunks.length}`);
  check("in-flight stream rejects after abort", threw, "iterator completed without error");
  check(
    "in-flight stream abort message",
    got && String(got.message || got).includes(ABORT_MSG),
    `got ${got && (got.message || got)}`,
  );
  const closed = await waitClosed();
  check("in-flight stream drops upstream connection", closed);
}

// ---- 6. early break (return()) cancels upstream without error ----
{
  reset();
  let broke = false;
  let threw = false;
  try {
    for await (const _ of cli.chat({ messages: [] })) {
      broke = true;
      break;
    }
  } catch {
    threw = true;
  }
  check("early break yields at least one chunk", broke);
  check("early break does not throw", !threw);
  const closed = await waitClosed();
  check("early break drops upstream connection", closed);
}

// ---- 7. control opts never sent to the API ----
{
  reset();
  const c = new AbortController();
  await cli.chatOnce({ messages: [] }, { signal: c.signal, timeout: 30, idle_timeout: 5 });
  check("request reached server", hits === 1, `hits=${hits}`);
  check(
    "signal/timeout/idle_timeout absent from body",
    bodyKeys &&
      !bodyKeys.includes("signal") &&
      !bodyKeys.includes("timeout") &&
      !bodyKeys.includes("idle_timeout"),
    JSON.stringify(bodyKeys),
  );
}

// ---- 8. responsesOnce pre-aborted ----
{
  reset();
  const reason = new Error("cancel responses");
  const c = new AbortController();
  c.abort(reason);
  let got;
  try {
    await cli.responsesOnce({ input: "hi" }, { signal: c.signal });
  } catch (e) {
    got = e;
  }
  check("pre-aborted responsesOnce rejects with reason", got === reason, String(got));
  check("pre-aborted responsesOnce sends no request", hits === 0, `hits=${hits}`);
}

// ---- 9. responsesOnce in-flight abort ----
{
  reset();
  const c = new AbortController();
  const p = cli.responsesOnce({ input: "hi" }, { signal: c.signal });
  await Aike.sleep(300);
  check("responsesOnce request reached server", hits === 1, `hits=${hits}`);
  c.abort();
  let got;
  try {
    await p;
  } catch (e) {
    got = e;
  }
  check(
    "in-flight responsesOnce rejects with abort message",
    got && String(got.message || got).includes(ABORT_MSG),
    `got ${got && (got.message || got)}`,
  );
  // Non-stream: req.onClose does not fire (see comment in scenario 4).
}

// ---- 10. responses stream in-flight abort ----
{
  reset();
  const c = new AbortController();
  const chunks = [];
  let got;
  let threw = false;
  try {
    for await (const ev of cli.responses({ input: "hi" }, { signal: c.signal })) {
      chunks.push(ev);
      if (chunks.length === 2) c.abort();
    }
  } catch (e) {
    threw = true;
    got = e;
  }
  check("responses stream yielded before abort", chunks.length >= 2, `got ${chunks.length}`);
  check("responses stream rejects after abort", threw, "iterator completed without error");
  check(
    "responses stream abort message",
    got && String(got.message || got).includes(ABORT_MSG),
    `got ${got && (got.message || got)}`,
  );
  const closed = await waitClosed();
  check("responses stream drops upstream connection", closed);
}

console.log(pass + " passed, " + fail + " failed");
if (fail > 0) throw new Error(fail + " test(s) failed");
globalThis.mockHandle.shutdown();
await globalThis.mockHandle.exit;
