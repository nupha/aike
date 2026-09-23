import { LlmClient } from "ai";
import { HttpServer } from "http";

// coalesce_test.mjs — verifies the `coalesce` call opt merges consecutive
// pure-content chunks into one, while default streaming still yields every chunk.
//   ./target/debug/aike tests/llm/coalesce_test.mjs

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

const MOCK = "http://127.0.0.1:18141";

function mkChunk(delta, finish) {
  const msg = {
    id: "chatcmpl-1",
    object: "chat.completion.chunk",
    model: "mock",
    choices: [{ index: 0, delta: delta, finish_reason: finish === undefined ? null : finish }],
  };
  return "data: " + JSON.stringify(msg) + "\n\n";
}

// Simulate char-per-chunk streaming: 5 content chars + reasoning + finish.
const SSE =
  mkChunk({ role: "assistant" }) +
  mkChunk({ content: "h" }) +
  mkChunk({ content: "e" }) +
  mkChunk({ content: "l" }) +
  mkChunk({ content: "l" }) +
  mkChunk({ content: "o" }) +
  mkChunk({ reasoning_content: "thinking" }) +
  mkChunk({ content: " world" }) +
  mkChunk({}, "stop") +
  "data: [DONE]\n\n";

const svr = new HttpServer();
svr.post("/v1/chat/completions", async (req, res) => {
  const body = await req.json();
  res.setHeader("Content-Type", "text/event-stream");
  res.end(SSE);
});
globalThis.mockHandle = await svr.listen({ host: "127.0.0.1", port: 18141 });
await new Promise((r) => setTimeout(r, 200));

const cli = new LlmClient({ baseUrl: MOCK + "/v1", apiKey: "k", model: "m" });

// ---- default: every chunk yielded individually ----
{
  let chunks = 0;
  let text = "";
  let reasoning = "";
  for await (const c of cli.chat({ messages: [{ role: "user", content: "hi" }] })) {
    chunks++;
    const d = c.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
    if (typeof d.reasoning_content === "string") reasoning += d.reasoning_content;
  }
  check("default yields every chunk (9 data chunks + role/finish)", chunks === 9);
  check("default text", text === "hello world");
  check("default reasoning", reasoning === "thinking");
}

// ---- coalesce: pure content chunks merged within window ----
{
  let chunks = 0;
  let text = "";
  let reasoning = "";
  for await (const c of cli.chat({ messages: [{ role: "user", content: "hi" }] }, { coalesce: 10 })) {
    chunks++;
    const d = c.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
    if (typeof d.reasoning_content === "string") reasoning += d.reasoning_content;
  }
  check("coalesce merges chunks (5 chars + reasoning + space → ≤4)", chunks <= 4);
  check("coalesce text preserved", text === "hello world");
  check("coalesce reasoning preserved", reasoning === "thinking");
}

// ---- coalesce: 0 / negative disables merging ----
for (const v of [0, -1]) {
  let chunks = 0;
  let text = "";
  for await (const c of cli.chat({ messages: [{ role: "user", content: "hi" }] }, { coalesce: v })) {
    chunks++;
    const d = c.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
  }
  check("coalesce=" + v + " disables merging (9 chunks)", chunks === 9 && text === "hello world");
}

// ---- coalesce with normalizer: merged chunks still normalized ----
{
  const cli2 = new LlmClient({ baseUrl: MOCK + "/v1", apiKey: "k" });
  cli2.responseNormalizer = function (data) {
    const t = data.choices && data.choices[0] && data.choices[0].delta;
    if (t && typeof t.content === "string") t.content = "[" + t.content + "]";
    return data;
  };
  let chunks = 0;
  let text = "";
  for await (const c of cli2.chat({ messages: [{ role: "user", content: "hi" }] }, { coalesce: 10 })) {
    chunks++;
    const d = c.choices[0].delta;
    if (typeof d.content === "string") text += d.content;
  }
  check("coalesce + normalizer calls fewer times", chunks <= 5);
  check("coalesce + normalizer text", text === "[hello world]");
}

// ---- coalesce option validation ----
{
  let threw = false;
  try {
    await cli.chat({ messages: [{ role: "user", content: "hi" }] }, { coalesce: true });
  } catch (e) {
    threw = /coalesce/.test(String(e));
  }
  check("coalesce non-number throws TypeError", threw);

  let threw2 = false;
  try {
    await cli.chat({ messages: [{ role: "user", content: "hi" }] }, { coalesce: 1.5 });
  } catch (e) {
    threw2 = /coalesce/.test(String(e));
  }
  check("coalesce fractional throws RangeError", threw2);
}

console.log(pass + " passed, " + fail + " failed");
if (fail > 0) throw new Error(fail + " test(s) failed");
globalThis.mockHandle.shutdown();
await globalThis.mockHandle.exit;
