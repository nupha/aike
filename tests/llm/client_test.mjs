import { Client } from "llama";
import { HttpServer } from "http";

// client_test.mjs — JSLlmClient ("llama".Client) protocol tests against a
// built-in mock OpenAI-compatible server. Self-contained, no backend needed:
//   ./target/debug/aike tests/llm/client_test.mjs
// To run against a real server instead of the mock:
//   LLM_CLIENT_TEST=env AIKE_LLM_BASE_URL=http://host/v1 ./target/debug/aike ...
// (.mjs keeps this file in ESM mode regardless of leading comments; in .js
// files a comment before the first `import` breaks aike's module detection.)

let pass = 0;
let fail = 0;
function check(label, cond) {
  if (cond) {
    console.log("PASS " + label);
    pass++;
  } else {
    console.log("FAIL " + label);
    fail++;
  }
}

// Mock by default. Set LLM_CLIENT_TEST=env (+ AIKE_LLM_BASE_URL) to run the
// same assertions against a real server via the env-config constructor.
const USE_ENV = process.env.LLM_CLIENT_TEST === "env" && !!process.env.AIKE_LLM_BASE_URL;
const MOCK = "http://127.0.0.1:18140";
const BASE = USE_ENV ? process.env.AIKE_LLM_BASE_URL : MOCK + "/v1";

// ---- mock OpenAI server ----

function mkChunk(delta, finish) {
  const msg = {
    id: "chatcmpl-1",
    object: "chat.completion.chunk",
    model: "mock",
    choices: [{ index: 0, delta: delta, finish_reason: finish === undefined ? null : finish }],
  };
  return "data: " + JSON.stringify(msg) + "\n\n";
}

function sseFor(messages) {
  const last = messages[messages.length - 1].content || "";
  if (last.indexOf("TOOLCALL") >= 0) {
    // tool_call deltas split across chunks; one chunk carries content too
    return (
      mkChunk({ role: "assistant" }) +
      mkChunk({ tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "get_weather", arguments: "" } }] }) +
      mkChunk({ tool_calls: [{ index: 0, function: { arguments: '{"city":"Sh' } }] }) +
      mkChunk({ content: "checking...", tool_calls: [{ index: 0, function: { arguments: 'anghai"}' } }] }) +
      mkChunk({}, "tool_calls") +
      "data: [DONE]\n\n"
    );
  }
  return (
    mkChunk({ role: "assistant", content: "stream " }) +
    mkChunk({ content: "reply" }) +
    mkChunk({}, "stop") +
    "data: [DONE]\n\n"
  );
}

const svr = new HttpServer();
svr.post("/v1/chat/completions", async (req, res) => {
  const body = await req.json();
  if (!body.stream) {
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        id: "chatcmpl-n",
        object: "chat.completion",
        model: "mock",
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "non-stream reply" },
            finish_reason: "stop",
          },
        ],
        usage: { prompt_tokens: 3, completion_tokens: 4, total_tokens: 7 },
      }),
    );
    return;
  }
  res.setHeader("Content-Type", "text/event-stream");
  res.end(sseFor(body.messages));
});
globalThis.mockHandle = await svr.listen({ host: "127.0.0.1", port: 18140 });
await new Promise((r) => setTimeout(r, 200));

// ---- constructor & getters ----

const cli = new Client({
  baseUrl: BASE,
  apiKey: "test-key",
  model: "mock-model",
  timeout: 30,
});
check("getter model", cli.model === "mock-model");
check("getter baseUrl", cli.baseUrl === BASE);
check("getter maxTokens is number", typeof cli.maxTokens === "number");
check("getter timeout", cli.timeout === 30);
check("bufferedToolCall getter removed", cli.bufferedToolCall === undefined);

const envCli = new Client({});
check("new Client({}) with env/defaults", envCli instanceof Client);

// ---- chatOnce ----

{
  const resp = await cli.chatOnce({ messages: [{ role: "user", content: "hi" }] });
  check("chatOnce content", resp.choices[0].message.content === "non-stream reply");
  check("chatOnce usage", resp.usage.total_tokens === 7);
}

// ---- streaming text ----

{
  let text = "";
  for await (const chunk of cli.chat({ messages: [{ role: "user", content: "hello" }] })) {
    const d = chunk.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
  }
  check("stream text accumulation", text === "stream reply");
}

// ---- streaming tool_call buffering ----

{
  const toolCalls = [];
  let sawText = "";
  for await (const chunk of cli.chat({
    messages: [{ role: "user", content: "TOOLCALL please" }],
  })) {
    const d = chunk.choices[0].delta;
    if (typeof d.content === "string" && d.content) sawText = d.content;
    if (d.tool_calls) {
      for (const tc of d.tool_calls) {
        let cur = null;
        for (const t of toolCalls) if (t.index === tc.index) cur = t;
        if (!cur) {
          toolCalls.push(JSON.parse(JSON.stringify(tc)));
        } else if (tc.function && tc.function.arguments) {
          cur.function.arguments += tc.function.arguments;
        }
      }
    }
  }
  check("toolcall content split from same chunk", sawText === "checking...");
  check(
    "toolcall deltas accumulate",
    toolCalls.length === 1 &&
      toolCalls[0].id === "call_1" &&
      toolCalls[0].function.name === "get_weather" &&
      toolCalls[0].function.arguments === '{"city":"Shanghai"}',
  );
}

// ---- responseNormalizer ----

{
  const cli2 = new Client({ baseUrl: BASE, apiKey: "test-key" });
  cli2.responseNormalizer = function (data) {
    // stream chunks carry delta, non-stream carries message
    const t0 = data.choices && data.choices[0];
    const t = t0 ? t0.delta || t0.message : undefined;
    if (t && typeof t.content === "string") t.content = "[n] " + t.content;
    return data;
  };
  const resp = await cli2.chatOnce({ messages: [{ role: "user", content: "hi" }] });
  check("normalizer on chatOnce", resp.choices[0].message.content === "[n] non-stream reply");

  let text = "";
  for await (const chunk of cli2.chat({ messages: [{ role: "user", content: "hello" }] })) {
    const d = chunk.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
  }
  check("normalizer on stream", text === "[n] stream [n] reply");
}

console.log(pass + " passed, " + fail + " failed");
if (fail > 0) throw new Error(fail + " test(s) failed");
