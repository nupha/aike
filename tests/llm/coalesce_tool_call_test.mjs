import { LlmClient } from "ai";
import { HttpServer } from "http";

// coalesce_tool_call_test.mjs — regression: a tool_call frame must never be
// swallowed by the coalesce window.
//
// Root cause reproduced from opencode zen gateway (mimo-v2.6-flash submit):
// tool frames arrive as `{role, content: null, reasoning_content: null,
// tool_calls: [...]}`. serde_json's Value::get() returns Some(Null), so the
// old `nd.get("content").is_some()` check in the coalesce inner window
// misclassified them as pure-text frames, merged them into CoalescedChunk
// (which only reads string content/reasoning) and silently dropped the
// tool_call — consumer saw finish_reason=tool_calls with ZERO tool frames.
// ravenclaw runs with coalesce:20, which is why submit_missing nudges fired
// and tripped the submit_missing circuit breaker.
//
//   ./target/debug/aike tests/llm/coalesce_tool_call_test.mjs

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

const mk = (delta, finish = null) =>
  `data: ${JSON.stringify({
    id: "chatcmpl-t",
    object: "chat.completion.chunk",
    model: "fake",
    choices: [{ index: 0, delta, finish_reason: finish }],
  })}\n\n`;

// Gateway-faithful shape: null-valued content/reasoning keys ride along on
// every tool frame, and reasoning chunks stream right before them (so the
// coalesce window is open when the tool frames land).
const GATEWAY_SSE =
  mk({ role: "assistant" }) +
  mk({ role: "assistant", content: null, reasoning_content: "Think", tool_calls: null }) +
  mk({ content: "ing " }) +
  mk({ content: "hard." }) +
  // tool frame #1: id + name, empty arguments (as zen sends it)
  mk({
    role: "assistant",
    content: null,
    reasoning_content: null,
    tool_calls: [{ index: 0, id: "call_x", type: "function", function: { name: "submit", arguments: "" } }],
  }) +
  // tool frame #2: args only, id/name null
  mk({
    role: "assistant",
    content: null,
    reasoning_content: null,
    tool_calls: [{ index: 0, id: null, type: "function", function: { name: null, arguments: "{}" } }],
  }) +
  // finish frame: no tool_calls array, finish_reason only
  mk({ role: "assistant", content: null, reasoning_content: null, tool_calls: null }, "tool_calls") +
  "data: [DONE]\n\n";

// Variant: a plain content frame interleaved between the two tool frames.
const INTERLEAVED_SSE =
  mk({ role: "assistant" }) +
  mk({ content: "working" }) +
  mk({
    content: null,
    reasoning_content: null,
    tool_calls: [{ index: 0, id: "call_y", type: "function", function: { name: "sh", arguments: '{"cmd"' } }],
  }) +
  mk({ content: "…" }) +
  mk({
    content: null,
    reasoning_content: null,
    tool_calls: [{ index: 0, id: null, type: "function", function: { name: null, arguments: ': "ls"}' } }],
  }) +
  mk({}, "tool_calls") +
  "data: [DONE]\n\n";

// Variant: only reasoning streams before the tool call (no content at all).
const REASONING_ONLY_SSE =
  mk({ role: "assistant" }) +
  mk({ reasoning_content: "decide" }) +
  mk({ reasoning_content: " to submit" }) +
  mk({
    content: null,
    reasoning_content: null,
    tool_calls: [{ index: 0, id: "call_z", type: "function", function: { name: "submit", arguments: "{}" } }],
  }) +
  mk({ content: null, reasoning_content: null, tool_calls: null }, "tool_calls") +
  "data: [DONE]\n\n";

const svr = new HttpServer();
const routes = { "/gateway": GATEWAY_SSE, "/interleaved": INTERLEAVED_SSE, "/reasoning-only": REASONING_ONLY_SSE };
svr.post("/v1/chat/completions", async (req, res) => {
  const body = await req.json();
  const name = body.messages?.[0]?.content;
  const sse = routes[name];
  if (!sse) {
    res.setStatus(404).end("no such scenario");
    return;
  }
  res.setHeader("Content-Type", "text/event-stream");
  res.end(sse);
});
const handle = await svr.listen({ host: "127.0.0.1", port: 18147 });
await new Promise((r) => setTimeout(r, 200));

async function run(path, opts) {
  const cli = new LlmClient({ baseUrl: "http://127.0.0.1:18147/v1", apiKey: "t", model: "fake" });
  const acc = {};
  let finish = null;
  let text = "";
  let reasoning = "";
  for await (const c of cli.chat({ messages: [{ role: "user", content: path }] }, opts)) {
    const ch = c.choices && c.choices[0];
    if (!ch) continue;
    if (ch.finish_reason) finish = ch.finish_reason;
    const d = ch.delta || {};
    if (typeof d.content === "string") text += d.content;
    if (typeof d.reasoning_content === "string") reasoning += d.reasoning_content;
    if (d.tool_calls) {
      for (const tc of d.tool_calls) {
        const k = tc.index;
        acc[k] ??= { id: null, name: null, args: "" };
        if (tc.id) acc[k].id = tc.id;
        if (tc.function?.name) acc[k].name = tc.function.name;
        if (tc.function?.arguments) acc[k].args += tc.function.arguments;
      }
    }
  }
  return { finish, text, reasoning, tools: acc };
}

// ---- coalesce window open when tool frames arrive (the exact zen shape) ----
for (const coalesce of [20, 0, undefined]) {
  const label = `coalesce=${coalesce ?? "default"}`;
  const r = await run("/gateway", coalesce === undefined ? {} : { coalesce });
  const t = r.tools[0];
  check(`${label} gateway-shape: tool_call survives`, !!t);
  check(`${label} gateway-shape: id preserved`, t?.id === "call_x");
  check(`${label} gateway-shape: name preserved`, t?.name === "submit");
  check(`${label} gateway-shape: arguments merged`, t?.args === "{}");
  check(`${label} gateway-shape: finish_reason=tool_calls`, r.finish === "tool_calls");
  check(`${label} gateway-shape: surrounding text not lost`, r.text === "Thinking hard." || r.reasoning.includes("Think"));
}

// ---- content frame interleaved between tool fragments ----
for (const coalesce of [20, undefined]) {
  const label = `coalesce=${coalesce ?? "default"}`;
  const r = await run("/interleaved", coalesce === undefined ? {} : { coalesce });
  const t = r.tools[0];
  check(`${label} interleaved: tool_call survives`, !!t);
  check(`${label} interleaved: arguments merged across gap`, t?.args === '{"cmd": "ls"}');
  check(`${label} interleaved: interleaved text kept`, r.text.includes("…"));
}

// ---- reasoning-only stream before the tool call ----
for (const coalesce of [20, undefined]) {
  const label = `coalesce=${coalesce ?? "default"}`;
  const r = await run("/reasoning-only", coalesce === undefined ? {} : { coalesce });
  const t = r.tools[0];
  check(`${label} reasoning-only: tool_call survives`, !!t);
  check(`${label} reasoning-only: name preserved`, t?.name === "submit");
  check(`${label} reasoning-only: reasoning text kept`, r.reasoning === "decide to submit");
}

console.log(pass + " passed, " + fail + " failed");
if (fail > 0) throw new Error(fail + " test(s) failed");
handle.shutdown();
await handle.exit;
