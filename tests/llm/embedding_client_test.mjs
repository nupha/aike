import { EmbeddingClient } from "ai";
import { HttpServer } from "http";

// embedding_client_test.mjs — EmbeddingClient ("ai".EmbeddingClient) protocol
// tests against a built-in mock OpenAI-compatible embeddings server.
// Self-contained:
//   ./target/debug/aike tests/llm/embedding_client_test.mjs
//
// Wire protocol: POST {baseUrl}/embeddings — `baseUrl` carries the `/v1`
// prefix, the same convention `LlmClient` uses for
// `{baseUrl}/chat/completions`.

let pass = 0;
let fail = 0;
function check(label, cond, detail) {
  if (cond) {
    console.log("PASS " + label);
    pass++;
  } else {
    console.log("FAIL " + label + (detail !== undefined ? "  -> " + detail : ""));
    fail++;
  }
}

function throwsType(fn, label) {
  try {
    fn();
    check(label, false);
  } catch (e) {
    check(label, e instanceof TypeError, e && e.message);
  }
}

function throwsRange(fn, label) {
  try {
    fn();
    check(label, false);
  } catch (e) {
    check(label, e instanceof RangeError, e && e.message);
  }
}

const MOCK = "http://127.0.0.1:18152/v1";
const MODEL = "text-embedding-3-small";
const API_KEY = "sk-test";

// ---- mock embeddings server ----

let lastRequest = null;
let lastAuth = null;
let lastContentType = null;
let failNext = null;

const server = new HttpServer();
server.post("/v1/embeddings", async (req, res) => {
  const body = await req.json();
  lastRequest = { path: req.path, method: req.method, body };
  lastAuth = req.headers["authorization"];
  lastContentType = req.headers["content-type"];

  const send = (status, payload) => {
    res.status(status);
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify(payload));
  };

  if (failNext) {
    const f = failNext;
    failNext = null;
    send(f.status, f.body);
    return;
  }

  const inputs = Array.isArray(body.input) ? body.input : [body.input];
  const data = inputs.map((input, index) => ({
    object: "embedding",
    index,
    embedding: [0.1, 0.2, 0.3],
  }));
  send(200, {
    object: "list",
    data,
    model: body.model,
    usage: { prompt_tokens: 8, total_tokens: 8 },
  });
});
globalThis.mockHandle = await server.listen({ host: "127.0.0.1", port: 18152 });
await new Promise((r) => setTimeout(r, 200));

// ---- client properties ----

const client = new EmbeddingClient({
  apiKey: API_KEY,
  baseUrl: MOCK,
  model: MODEL,
});

check("model getter", client.model === MODEL);
check("baseUrl getter", client.baseUrl === MOCK);
check("timeout getter", client.timeout === 60);
check("headers getter is object", typeof client.headers === "object");
client.setHeader("x-trace", "abc");
check("setHeader", client.getHeader("x-trace") === "abc");
client.setHeader("x-trace", null);
check("setHeader(null) removes", client.getHeader("x-trace") === undefined);

// ---- request shape ----

const res = await client.embed({ input: "hello" });

check("request path", lastRequest.path === "/v1/embeddings", lastRequest.path);
check("request method", lastRequest.method === "POST");
check("json content-type", /application\/json/.test(lastContentType || ""));
check("bearer auth header", lastAuth === "Bearer " + API_KEY);
check("request model", lastRequest.body.model === MODEL);
check("request input", lastRequest.body.input === "hello");

// ---- response parsing ----

check("response object", res.object === "list", res.object);
check("response model", res.model === MODEL, res.model);
check("response data length", Array.isArray(res.data) && res.data.length === 1);
check("response data object", res.data[0].object === "embedding");
check("response data index", res.data[0].index === 0);
check(
  "response embedding is an array of numbers",
  Array.isArray(res.data[0].embedding) &&
    res.data[0].embedding.every((n) => typeof n === "number"),
  JSON.stringify(res.data[0].embedding),
);
check("response usage.prompt_tokens", res.usage.prompt_tokens === 8);
check("response usage.total_tokens", res.usage.total_tokens === 8);

// ---- an array input produces one embedding per element ----

const many = await client.embed({ input: ["a", "b", "c"] });
check("array input is forwarded", Array.isArray(lastRequest.body.input));
check("array input length", lastRequest.body.input.length === 3);
check("one embedding per input", many.data.length === 3, String(many.data.length));
check("embedding indexes are sequential", many.data[2].index === 2);

// ---- token-id inputs ----

await client.embed({ input: [1, 2, 3] });
check("token ids forwarded", JSON.stringify(lastRequest.body.input) === "[1,2,3]");

await client.embed({ input: [[1, 2], [3]] });
check(
  "token id arrays forwarded",
  JSON.stringify(lastRequest.body.input) === "[[1,2],[3]]",
  JSON.stringify(lastRequest.body.input),
);

// ---- per-request model override ----

await client.embed({ input: "x", model: "other-embedding-model" });
check("per-request model override", lastRequest.body.model === "other-embedding-model");

// ---- strict request validation ----

throwsType(() => client.embed(), "embed() requires a request object");
throwsType(() => client.embed(null), "embed() rejects null request");
throwsType(() => client.embed({}), "input is required");
throwsType(() => client.embed({ input: 42 }), "input rejects a number");
throwsType(() => client.embed({ input: [true] }), "input array rejects a boolean");
throwsType(() => client.embed({ input: [["a"]] }), "token id arrays reject strings");

// ---- strict constructor validation ----

throwsType(() => new EmbeddingClient({ apiKey: 5, baseUrl: MOCK, model: "m" }),
  "constructor apiKey must be a string");
throwsType(() => new EmbeddingClient({ apiKey: "k", model: {}, baseUrl: MOCK }),
  "constructor model must be a string");
throwsRange(() => new EmbeddingClient({ baseUrl: MOCK, model: "m", timeout: 0 }),
  "constructor timeout must be positive");
throwsType(() => new EmbeddingClient(5), "constructor requires options object");
throwsType(() => new EmbeddingClient(), "constructor requires an options object");
throwsType(() => new EmbeddingClient({ model: "m" }), "constructor requires baseUrl");
throwsType(() => new EmbeddingClient({ baseUrl: MOCK }), "constructor requires model");
throwsType(() => new EmbeddingClient({ baseUrl: "", model: "m" }), "constructor rejects empty baseUrl");
throwsType(() => new EmbeddingClient({ baseUrl: MOCK, model: "" }), "constructor rejects empty model");

const keyless = new EmbeddingClient({ baseUrl: MOCK, model: "m" });
check("apiKey is optional", keyless instanceof EmbeddingClient);

// ---- call opts ----

throwsType(() => client.embed({ input: "x" }, null), "null opts rejected");
throwsType(() => client.embed({ input: "x" }, { signal: {} }), "signal must be AbortSignal");
throwsRange(() => client.embed({ input: "x" }, { timeout: -1 }), "timeout must be positive integer");
throwsType(() => client.embed({ input: "x" }, { timeout: "5" }), "timeout must be a number");

// ---- server error surfaces as a rejected promise ----

async function expectReject(promise, re, label) {
  try {
    await promise;
    check(label, false);
  } catch (e) {
    check(label, re.test(e.message), e.message);
  }
}

failNext = { status: 401, body: { error: { message: "invalid api key" } } };
await expectReject(
  client.embed({ input: "x" }),
  /invalid api key/,
  "error.message extracted",
);

failNext = { status: 400, body: { error: "bad request" } };
await expectReject(client.embed({ input: "x" }), /bad request/, "error string extracted");

failNext = { status: 429, body: { message: "slow down" } };
await expectReject(client.embed({ input: "x" }), /slow down/, "top-level message extracted");

failNext = { status: 500, body: { error: { message: "upstream exploded" } } };
await expectReject(client.embed({ input: "x" }), /upstream exploded/, "5xx error extracted");

failNext = { status: 418, body: { weird: true } };
await expectReject(client.embed({ input: "x" }), /weird/, "unrecognized body falls back to raw");

// ---- abort before send ----

const ac = new AbortController();
ac.abort();
try {
  await client.embed({ input: "x" }, { signal: ac.signal });
  check("pre-aborted signal rejects", false);
} catch (e) {
  check("pre-aborted signal rejects", e.name === "AbortError", e && e.message);
}

// ---- timeout: an unreachable endpoint must time out, not hang ----

const dead = new EmbeddingClient({
  baseUrl: "http://127.0.0.1:1/v1",
  model: "m",
  timeout: 1,
});
try {
  await dead.embed({ input: "x" });
  check("unreachable endpoint rejects", false);
} catch (e) {
  check("unreachable endpoint rejects", e instanceof Error && e.message.length > 0, e && e.message);
}

globalThis.mockHandle.shutdown();
await globalThis.mockHandle.exit;
console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) throw new Error(`${fail} assertion(s) failed`);
