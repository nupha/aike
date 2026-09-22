// Smoke test for LlmClient abort + break-cancellation via the `signal` call
// option (second argument of chat/chatOnce/responses/responsesOnce).
//
// Verifies mid-flight cancellation (stream + once), pre-aborted fail-fast,
// reason propagation, signal type validation, that abort actually closes the
// upstream connection (server-side onClose), that `signal` is NOT sent in the
// API body, and that `for await ... break` drops the upstream request so
// server-side generation stops.
//
// Uses aike's in-process HttpServer as the mock: `res.sse().write(data)`
// already emits SSE `data:` framing (never prefix it manually), and a slow
// handler must defer its returned promise until after `res.end()/res.json()`
// (otherwise the framework auto-responds 204). Run with the debug aike build:
//   ./target/debug/aike tests/llm/llmclient-abort.mjs
import { LlmClient } from "ai";
import { HttpServer } from "http";

function trace(msg) {
  console.error(`[trace] ${msg}`);
}

let failures = 0;
function assert(cond, msg) {
  if (!cond) {
    failures++;
    console.error(`FAIL: ${msg}`);
  } else {
    console.log(`ok: ${msg}`);
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// ── mock LLM server (in-process aike HttpServer) ──────────────────────────
let totalRequests = 0;
let closedChatStreams = 0;
let closedResponsesStreams = 0;
const receivedBodyKeys = [];

function chatStream(req, res) {
  const sse = res.sse();
  let i = 0;
  const timer = setInterval(() => {
    sse.write(JSON.stringify({
      id: "chatcmpl-abort",
      object: "chat.completion.chunk",
      created: 123,
      model: "test-model",
      choices: [{ index: 0, delta: { content: `tok${i++} ` }, finish_reason: null }],
    }));
  }, 100);
  req.onClose(() => clearInterval(timer));
}

function responsesStream(req, res) {
  const sse = res.sse();
  let i = 0;
  const timer = setInterval(() => {
    sse.write(JSON.stringify({
      type: "response.output_item.done",
      response_id: "r_1",
      item_id: "it_1",
      output_index: 0,
      item: {
        id: "it_1",
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text: `${i++} ` }],
      },
    }));
  }, 100);
  req.onClose(() => clearInterval(timer));
}

const server = new HttpServer();
server
  .post("/v1/chat/completions", (req, res) =>
    req.json().then((body) => {
      totalRequests++;
      receivedBodyKeys.push(Object.keys(body));
      if (body.stream) {
        chatStream(req, res);
        req.onClose(() => closedChatStreams++);
        return;
      }
      // Defer until the response is sent: resolving earlier makes the
      // framework auto-respond 204 no-content.
      return new Promise((resolve) => {
        req.onClose(() => closedChatStreams++);
        setTimeout(() => {
          res.json({
            id: "chatcmpl-1",
            object: "chat.completion",
            created: 123,
            model: "test-model",
            choices: [
              { index: 0, message: { role: "assistant", content: "done" }, finish_reason: "stop" },
            ],
            usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
          });
          resolve();
        }, 3000);
      });
    }),
  )
  .post("/v1/responses", (req, res) =>
    req.json().then((body) => {
      totalRequests++;
      receivedBodyKeys.push(Object.keys(body));
      responsesStream(req, res);
      req.onClose(() => closedResponsesStreams++);
    }),
  );
server.listen({ port: 8123, host: "127.0.0.1" });

const client = new LlmClient({
  baseUrl: "http://127.0.0.1:8123/v1",
  apiKey: "sk-test",
  model: "test-model",
});

async function run() {
  await sleep(200);

  // ── 1. streaming chat: abort mid-flight ──
  {
    const ctrl = new AbortController();
    const it = client.chat(
      { messages: [{ role: "user", content: "hi" }] },
      { signal: ctrl.signal },
    );
    let chunks = 0;
    let abortErr = "";
    try {
      for await (const _chunk of it) {
        chunks++;
        if (chunks >= 1) ctrl.abort();
      }
    } catch (e) {
      abortErr = e.message;
    }
    assert(chunks >= 1, `streaming chat delivered at least one chunk (n=${chunks})`);
    assert(abortErr.includes("aborted by AbortSignal"), `streaming chat rejects on abort: "${abortErr}"`);
    await sleep(300);
    assert(closedChatStreams >= 1, `abort closes the server-side stream (closed=${closedChatStreams})`);
  }

  // ── 2. chatOnce: abort mid-flight ──
  {
    const ctrl = new AbortController();
    const p = client.chatOnce(
      { messages: [{ role: "user", content: "hi" }] },
      { signal: ctrl.signal },
    );
    setTimeout(() => ctrl.abort(), 150);
    let abortErr = "";
    try {
      await p;
    } catch (e) {
      abortErr = e.message;
    }
    assert(abortErr.includes("aborted by AbortSignal"), `chatOnce rejects on abort: "${abortErr}"`);
  }

  // ── 3. chatOnce pre-aborted → immediate DOMException AbortError, no request ──
  {
    const ctrl = new AbortController();
    ctrl.abort();
    const before = totalRequests;
    let err = null;
    try {
      await client.chatOnce(
        { messages: [{ role: "user", content: "hi" }] },
        { signal: ctrl.signal },
      );
    } catch (e) {
      err = e;
    }
    assert(err !== null, `pre-aborted chatOnce rejects`);
    assert(err?.name === "AbortError", `pre-aborted chatOnce rejects with AbortError, got "${err?.name}"`);
    await sleep(100);
    assert(totalRequests === before, `pre-aborted chatOnce never sends a request`);
  }

  // ── 4. pre-aborted with custom reason ──
  {
    const ctrl = new AbortController();
    ctrl.abort("user-logged-out");
    const before = totalRequests;
    let reason = undefined;
    try {
      await client.chatOnce(
        { messages: [{ role: "user", content: "hi" }] },
        { signal: ctrl.signal },
      );
    } catch (e) {
      reason = e;
    }
    assert(reason === "user-logged-out", `pre-aborted chatOnce rejects with the custom reason, got ${reason}`);
    assert(totalRequests === before, `custom-reason pre-abort never sends a request`);
  }

  // ── 5. streaming chat pre-aborted → iterator rejects without a request ──
  {
    const ctrl = new AbortController();
    ctrl.abort();
    const before = totalRequests;
    let abortErr = "";
    try {
      for await (const _chunk of client.chat(
        { messages: [{ role: "user", content: "hi" }] },
        { signal: ctrl.signal },
      )) {
        // unreachable
      }
    } catch (e) {
      abortErr = e.message;
    }
    assert(abortErr.includes("aborted by AbortSignal"), `pre-aborted streaming chat rejects: "${abortErr}"`);
    assert(totalRequests === before, `pre-aborted streaming chat never sends a request`);
  }

  // ── 6. invalid signal type → TypeError ──
  {
    let threw = false;
    try {
      client.chat({ messages: [] }, { signal: {} });
    } catch (e) {
      threw = true;
      assert(
        e.message.includes("must be an AbortSignal"),
        `non-AbortSignal signal is TypeError: ${e.message}`,
      );
    }
    assert(threw, `non-AbortSignal signal throws`);
  }

  // ── 7. responsesOnce + responses (stream) abort mid-flight ──
  {
    const ctrl = new AbortController();
    const p = client.responsesOnce({ input: "hi" }, { signal: ctrl.signal });
    setTimeout(() => ctrl.abort(), 150);
    let abortErr = "";
    try {
      await p;
    } catch (e) {
      abortErr = e.message;
    }
    assert(abortErr.includes("aborted by AbortSignal"), `responsesOnce rejects on abort: "${abortErr}"`);
  }
  {
    const ctrl = new AbortController();
    const it = client.responses({ input: "hi" }, { signal: ctrl.signal });
    setTimeout(() => ctrl.abort(), 300);
    let abortErr = "";
    try {
      for await (const _ev of it) {
        // The mock's frames may not map to aike's ResponseStreamEvent schema;
        // abort lands on a timer, not on an event.
      }
    } catch (e) {
      abortErr = e.message;
    }
    assert(abortErr.includes("aborted by AbortSignal"), `streaming responses rejects on abort: "${abortErr}"`);
    await sleep(200);
    assert(closedResponsesStreams >= 1, `responses abort closes the server-side stream (closed=${closedResponsesStreams})`);
  }

  // ── 8. control opts are NOT forwarded to the API body ──
  {
    const ctrl = new AbortController();
    const it = client.chat(
      { messages: [{ role: "user", content: "hi" }], stream_options: { include_usage: true } },
      { signal: ctrl.signal },
    );
    let saw = false;
    try {
      for await (const _c of it) {
        saw = true;
        ctrl.abort();
      }
    } catch {
      /* expected */
    }
    assert(saw, `signal-not-forwarded request streams (saw a chunk)`);
    await sleep(100);
    let allOk = true;
    for (const keys of receivedBodyKeys) {
      if (keys.includes("signal")) allOk = false;
    }
    assert(allOk, `no request body contained a "signal" key`);
  }

  // ── 9. regression: normal non-abort chatOnce still works ──
  {
    const res = await client.chatOnce({ messages: [{ role: "user", content: "hi" }] });
    assert(res.choices?.[0]?.message?.content === "done", `non-abort chatOnce returns normally`);
  }

  // ── 10. regression: streaming chat delivers chunks until abort ──
  {
    const ctrl = new AbortController();
    let n = 0;
    try {
      for await (const _chunk of client.chat(
        { messages: [{ role: "user", content: "hi" }] },
        { signal: ctrl.signal },
      )) {
        n++;
        if (n >= 3) ctrl.abort();
      }
    } catch {
      // abort after 3 chunks is expected
    }
    assert(n >= 3, `streaming chat delivers chunks until abort (n=${n})`);
  }

  // ── 11. break/return() cancels the upstream request ──
  {
    const before = closedChatStreams;
    const it = client.chat({ messages: [{ role: "user", content: "hi" }] });
    let n = 0;
    for await (const _chunk of it) {
      n++;
      break; // abandon the stream early
    }
    assert(n >= 1, `break happened after at least one chunk (n=${n})`);
    await sleep(400);
    assert(closedChatStreams > before, `break closes the upstream stream (closed=${closedChatStreams}, before=${before})`);
  }

  const hadFailures = failures > 0;
  console.log(hadFailures ? `\n${failures} FAILURE(S)` : "\nALL PASS");
  process.exit(hadFailures ? 1 : 0);
}

run().catch((e) => {
  console.error("test crashed:", e);
  process.exit(2);
});
