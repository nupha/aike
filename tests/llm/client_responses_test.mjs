import { LlmClient } from "ai";
import { HttpServer } from "http";

// client_responses_test.mjs — JSLlmClient Responses API (responsesOnce) against
// a built-in mock server. Self-contained, no backend needed:
//   ./target/debug/aike tests/llm/client_responses_test.mjs

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

const MOCK = "http://127.0.0.1:18141";
const svr = new HttpServer();

// OpenAI Responses-API-shaped payload covering the item types aike surfaces.
const RESPONSE = {
  id: "resp_1",
  created_at: 1756041539,
  object: "response",
  model: "mock",
  status: "completed",
  usage: {
    input_tokens: 5,
    input_tokens_details: { cached_tokens: 0 },
    output_tokens: 9,
    output_tokens_details: { reasoning_tokens: 4 },
    total_tokens: 14,
  },
  output: [
    {
      type: "reasoning",
      id: "rs_1",
      status: "completed",
      summary: [{ type: "summary_text", text: "thinking hard" }],
    },
    {
      type: "message",
      id: "msg_1",
      role: "assistant",
      status: "completed",
      content: [
        { type: "output_text", text: "Hello from mock", annotations: [] },
        { type: "refusal", refusal: "" },
      ],
    },
    {
      type: "function_call",
      id: "fc_1",
      name: "get_weather",
      arguments: '{"city":"shanghai"}',
      call_id: "call_9",
      status: "completed",
    },
  ],
};

svr.post("/v1/responses", async (req, res) => {
  const body = await req.json();
  // echo request sanity back via header for the test to assert round-tripping
  res.setHeader("X-Mock-Saw-Model", body.model || "");
  res.setHeader("Content-Type", "application/json");
  res.end(JSON.stringify(RESPONSE));
});
globalThis.mockHandle = await svr.listen({ host: "127.0.0.1", port: 18141 });
await new Promise((r) => setTimeout(r, 200));

const cli = new LlmClient({ baseUrl: MOCK + "/v1", apiKey: "test-key", model: "mock" });

const resp = await cli.responsesOnce({
  input: [
    { role: "system", content: "You are a helpful assistant." },
    { role: "user", content: "weather in shanghai?" },
  ],
  tools: [
    {
      type: "function",
      name: "get_weather",
      description: "get weather",
      parameters: { type: "object", properties: { city: { type: "string" } }, required: ["city"] },
    },
  ],
  toolChoice: "auto",
  temperature: 0.7,
  maxOutputTokens: 512,
});

check("id", resp.id === "resp_1");
check("object", resp.object === "response");
check("status", resp.status === "completed");
check("usage total", resp.usage.total_tokens === 14);

const items = Array.isArray(resp.output) ? resp.output : [];
check("output has 3 items", items.length === 3, String(items.length));

const msg = items.find((i) => i.type === "message");
check("message item present", !!msg);
if (msg) {
  const texts = msg.content.filter((c) => c.type === "output_text").map((c) => c.text);
  check("output_text extracted", texts.includes("Hello from mock"), JSON.stringify(texts));
}

const reasoning = items.find((i) => i.type === "reasoning");
check("reasoning item present", !!reasoning);
if (reasoning) {
  check(
    "reasoning summary text",
    (reasoning.summary || []).some((s) => s.type === "summary_text" && s.text === "thinking hard"),
  );
}

const fc = items.find((i) => i.type === "function_call");
check("function_call item present", !!fc);
if (fc) {
  check("function_call name", fc.name === "get_weather");
  check("function_call arguments", fc.arguments === '{"city":"shanghai"}');
}

console.log(pass + " passed, " + fail + " failed");
if (fail > 0) throw new Error(fail + " test(s) failed");
globalThis.mockHandle.shutdown();
await globalThis.mockHandle.exit;
