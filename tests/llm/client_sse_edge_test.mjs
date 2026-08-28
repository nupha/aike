import { Client } from "ai";
import { HttpServer } from "http";

// SSE chunk builders
const mk = (delta, finish = null, id = "chatcmpl-e") =>
  `data: ${JSON.stringify({
    id,
    object: "chat.completion.chunk",
    model: "fake",
    choices: [{ index: 0, delta, finish_reason: finish }],
  })}\n\n`;

const svr = new HttpServer();
const routes = {
  // A: two parallel tool calls, index switch forces flush of #0
  "/parallel": () =>
    mk({ role: "assistant" }) +
    mk({ tool_calls: [{ index: 0, id: "c0", type: "function", function: { name: "f0", arguments: '{"a"' } }] }) +
    mk({ tool_calls: [{ index: 0, function: { arguments: ":1}" } }] }) +
    mk({ tool_calls: [{ index: 1, id: "c1", type: "function", function: { name: "f1", arguments: "" } }] }) +
    mk({ tool_calls: [{ index: 1, function: { arguments: '{"b":2}' } }] }) +
    mk({}, "tool_calls") +
    "data: [DONE]\n\n",

  // B: finish_reason arrives in the SAME chunk as a tool_call delta
  "/same-chunk-finish": () =>
    mk({ role: "assistant" }) +
    mk({ tool_calls: [{ index: 0, id: "c0", type: "function", function: { name: "f", arguments: "{}" } }] }) +
    mk({ tool_calls: [{ index: 0, function: { arguments: ' extra' } }], }, "tool_calls") +
    "data: [DONE]\n\n",

  // C: server never sends finish_reason nor [DONE]-flushable state... actually:
  //    buffered tool_call then abrupt end (no [DONE], no finish_reason)
  "/truncated": () =>
    mk({ role: "assistant" }) +
    mk({ tool_calls: [{ index: 0, id: "cx", type: "function", function: { name: "fx", arguments: '{"x":1}' } }] }),

  // D: CRLF line endings everywhere
  "/crlf": () =>
    mk({ role: "assistant" }).split("\n").join("\r\n") +
    mk({ tool_calls: [{ index: 0, id: "cd", type: "function", function: { name: "fd", arguments: "{}" } }] }).split("\n").join("\r\n") +
    mk({}, "tool_calls").split("\n").join("\r\n") +
    "data: [DONE]\r\n\r\n",

  // E: garbage data lines interleaved
  "/garbage": () =>
    "data: not-json-at-all\n\n" +
    mk({ role: "assistant" }) +
    "data: {broken\n\n" +
    mk({ content: "ok" }) +
    "data: [DONE]\n\n",

  // F: mid-stream OpenAI-style error AFTER a partial tool_call buffer
  "/mid-error": () =>
    mk({ role: "assistant" }) +
    mk({ tool_calls: [{ index: 0, id: "ce", type: "function", function: { name: "fe", arguments: '{"' } }] }) +
    `data: ${JSON.stringify({ error: { message: "boom mid-stream" } })}\n\n`,

  // G: tool call that never receives arguments
  "/empty-args": () =>
    mk({ role: "assistant" }) +
    mk({ tool_calls: [{ index: 0, id: "cg", type: "function", function: { name: "noargs" } }] }) +
    mk({}, "tool_calls") +
    "data: [DONE]\n\n",

  // H: plain content streaming before the tool call starts
  "/text-then-tool": () =>
    mk({ role: "assistant", content: "Let me " }) +
    mk({ content: "check." }) +
    mk({ tool_calls: [{ index: 0, id: "ch", type: "function", function: { name: "lookup", arguments: '{"q":"x"}' } }] }) +
    mk({}, "tool_calls") +
    "data: [DONE]\n\n",
};

svr.post("/v1/chat/completions", async (req, res) => {
  const body = await req.json();
  const name = body.messages?.[0]?.content;
  const gen = routes[name];
  if (!gen) {
    res.setStatus(404).end("no such scenario");
    return;
  }
  res.setHeader("Content-Type", "text/event-stream");
  res.end(gen());
});
globalThis.h = await svr.listen({ host: "127.0.0.1", port: 18133 });
await new Promise(r => setTimeout(r, 200));

async function run(path) {
  const client = new Client({ baseUrl: "http://127.0.0.1:18133/v1", apiKey: "t", model: "fake" });
  void 0;
  const chunks = [];
  try {
    for await (const c of client.chat({ messages: [{ role: "user", content: path }] })) {
      chunks.push(c);
    }
  } catch (e) {
    return { error: e.message };
  }
  return { chunks };
}

function summarize(result) {
  const chunks = result.chunks ?? [];
  const out = [];
  if (result.error) return ["error:" + result.error];
  let acc = {}; // accumulate tool_calls by index like a consumer would
  for (const c of chunks) {
    const d = c.choices?.[0]?.delta;
    const fr = c.choices?.[0]?.finish_reason;
    if (!d) continue;
    if (typeof d.content === "string" && d.content) out.push(`text:${d.content}`);
    if (d.tool_calls) {
      for (const tc of d.tool_calls) {
        const k = tc.index;
        acc[k] ??= { id: tc.id, name: tc.function?.name, args: "" };
        if (tc.id) acc[k].id = tc.id;
        if (tc.function?.name) acc[k].name = tc.function.name;
        if (tc.function?.arguments) acc[k].args += tc.function.arguments;
      }
    }
    if (fr) out.push(`finish:${fr}`);
  }
  for (const k of Object.keys(acc).sort()) {
    const t = acc[k];
    out.push(`tool[${k}]:${t.name}:${t.args}`);
  }
  return out;
}

console.log("A parallel      :", JSON.stringify(summarize(await run("/parallel"))));
console.log("B same-chunk-fin:", JSON.stringify(summarize(await run("/same-chunk-finish"))));
console.log("C truncated     :", JSON.stringify(summarize(await run("/truncated"))));
console.log("D crlf          :", JSON.stringify(summarize(await run("/crlf"))));
console.log("E garbage       :", JSON.stringify(summarize(await run("/garbage"))));
console.log("F mid-error     :", JSON.stringify(await run("/mid-error")));
console.log("G empty-args    :", JSON.stringify(summarize(await run("/empty-args"))));
console.log("H text-then-tool:", JSON.stringify(summarize(await run("/text-then-tool"))));
console.log("done: all scenarios executed");
globalThis.h.shutdown();
await globalThis.h.exit;
