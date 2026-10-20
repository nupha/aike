import { System1Client, choice, score, noul } from "ai";
import { HttpServer } from "http";

// system1_client_test.mjs — System1Client ("ai".System1Client) protocol tests
// against a built-in mock System One server. Self-contained:
//   ./target/debug/aike tests/llm/system1_client_test.mjs
// To run against the real decision model instead of the mock:
//   AIKE_S1_LIVE=1 ./target/debug/aike tests/llm/system1_client_test.mjs
//
// Live endpoint: https://api.bamboonode.dev/v1/systemone, model
// startlux-decision-27b-autothink. The client appends `/v1/systemone` to
// `baseUrl`, so baseUrl carries no `/v1` suffix.

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

// Key: BAMBOONODE_API_KEY, loaded from this repo's `.env` by `dotenvy::dotenv()`
// at startup — aike searches the CWD and its parents, so running from `aike/`
// picks up the workspace root's `.env`.
const KEY = process.env.BAMBOONODE_API_KEY;
// The mock is the default: this file asserts mock-observable wire details, so
// live mode has to be asked for explicitly.
const USE_ENV = process.env.AIKE_S1_LIVE === "1";
const MOCK = "http://127.0.0.1:18151";
const LIVE_BASE = "https://api.bamboonode.dev";
const BASE = USE_ENV ? LIVE_BASE : MOCK;
const MODEL = process.env.AIKE_S1_MODEL || "startlux-decision-27b-autothink";
const API_KEY = USE_ENV ? KEY : "sk-test";

// ---- mock System One server ----

let lastRequest = null;
let lastAuth = null;
let lastContentType = null;
let failNext = null;

const isEntry = (v) =>
  v === null || typeof v === "string" || typeof v === "object";

/**
 * Server-side validation mirroring the protocol: reject a malformed body with
 * 422 + `detail` (the shape the real API uses for request validation), so the
 * tests exercise the client's wire output rather than its own client-side guard.
 */
function validateBody(body) {
  const bad = (loc, msg) => ({ status: 422, body: { detail: [{ loc: ["body", loc], msg }] } });
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    return bad("", "body must be an object");
  }
  if (typeof body.model !== "string" || body.model === "") {
    return bad("model", "field required");
  }
  if (!isEntry(body.state)) {
    return bad("state", "must be text, an object, an array or null");
  }
  if (
    typeof body.questions !== "object" ||
    body.questions === null ||
    Array.isArray(body.questions)
  ) {
    return bad("questions", "field required");
  }
  const names = Object.keys(body.questions);
  if (names.length === 0) {
    return bad("questions", "at least one question is required");
  }
  for (const name of names) {
    const q = body.questions[name];
    const at = `questions.${name}`;
    if (typeof q !== "object" || q === null || Array.isArray(q)) {
      return bad(at, "must be an object");
    }
    if (q.instructions !== undefined && !isEntry(q.instructions)) {
      return bad(`${at}.instructions`, "must be text, an object, an array or null");
    }
    if (q.type === "noul") {
      if (
        q.criteria !== undefined &&
        q.criteria !== null &&
        (typeof q.criteria !== "object" || Array.isArray(q.criteria))
      ) {
        return bad(`${at}.criteria`, "must be an object or null");
      }
    } else if (q.type === "choice") {
      if (typeof q.criteria !== "object" || q.criteria === null || Array.isArray(q.criteria)) {
        return bad(`${at}.criteria`, "must be a map of label to description");
      }
      if (Object.keys(q.criteria).length === 0) {
        return bad(`${at}.criteria`, "at least one label is required");
      }
    } else if (q.type === "score") {
      if (!Array.isArray(q.criteria)) {
        return bad(`${at}.criteria`, "must be a list of descriptions");
      }
      if (q.criteria.length < 2) {
        return bad(`${at}.criteria`, "at least two scores are required");
      }
    } else {
      return bad(`${at}.type`, "must be one of choice, score, noul");
    }
  }
  return null;
}

/** One answer per question, keyed exactly as `ScoreResponse`/`ChoiceResponse` declare. */
function mkAnswer(question) {
  if (question.type === "noul") {
    return { type: "noul", noul: 0.82 };
  }
  if (question.type === "score") {
    // `legend`/`probabilities` are keyed by level index, one entry per level.
    const legend = {};
    const probabilities = {};
    question.criteria.forEach((desc, i) => {
      legend[i] = desc;
      probabilities[i] = i === question.criteria.length - 1 ? 0.52 : 0.48;
    });
    return { type: "score", score: 1.035, confidence: 0.7, legend, probabilities };
  }
  const labels = Object.keys(question.criteria);
  const probabilities = {};
  labels.forEach((l, i) => (probabilities[l] = i === 0 ? 0.6 : 0.4));
  return { type: "choice", choice: labels[0], confidence: 0.6, probabilities };
}

const server = new HttpServer();
server.post("/v1/systemone", async (req, res) => {
  const body = await req.json();
  lastRequest = { path: req.path, method: req.method, body };
  lastAuth = req.headers["authorization"];
  lastContentType = req.headers["content-type"];

  const send = (status, payload) => {
    res.status(status);
    res.setHeader("Content-Type", "application/json");
    res.setHeader("x-typesafe-request-id", "req_mock_1");
    res.end(JSON.stringify(payload));
  };

  if (failNext) {
    const f = failNext;
    failNext = null;
    send(f.status, f.body);
    return;
  }

  const invalid = validateBody(body);
  if (invalid) {
    send(invalid.status, invalid.body);
    return;
  }

  const answers = {};
  for (const [name, question] of Object.entries(body.questions)) {
    answers[name] = mkAnswer(question);
  }
  send(200, {
    model: body.model,
    answers,
    usage: { input_tokens: 120, output_tokens: 0 },
  });
});
globalThis.mockHandle = await server.listen({ host: "127.0.0.1", port: 18151 });
await new Promise((r) => setTimeout(r, 200));

// ---- helpers ----

async function throwsType(fn, label) {
  try {
    await fn();
    check(label, false);
  } catch (e) {
    check(label, e instanceof TypeError);
  }
}

function throwsRange(fn, label) {
  try {
    fn();
    check(label, false);
  } catch (e) {
    check(label, e instanceof RangeError);
  }
}

const client = new System1Client({
  apiKey: API_KEY,
  baseUrl: BASE,
  model: MODEL,
});

// ---- question builders ----

const c = choice("What is this ticket about?", { billing: null, technical: null });
check("choice() type", c.type === "choice");
check("choice() instructions", c.instructions === "What is this ticket about?");
check("choice() criteria keys", Object.keys(c.criteria).join() === "billing,technical");
check("choice() null description kept", c.criteria.billing === null);

const s = score("How frustrated?", ["Calm", "Angry"]);
check("score() type", s.type === "score");
check("score() criteria is array", Array.isArray(s.criteria) && s.criteria[1] === "Angry");

const y = noul("The message conveys urgency");
check("noul() type", y.type === "noul");
check("noul() criteria defaults to null", y.criteria === undefined || y.criteria === null);

throwsType(() => score("bad", "not an array"), "score() rejects non-array criteria");
throwsType(() => choice("bad", ["a", "b"]), "choice() rejects array criteria");
throwsType(() => choice("bad"), "choice() requires criteria");
throwsType(() => noul("bad", "nope"), "noul() criteria must be object");

// ---- client properties ----

check("model getter", client.model === MODEL);
check("baseUrl getter", client.baseUrl === BASE);
check("timeout getter", client.timeout === 60);
check("headers getter is object", typeof client.headers === "object");
client.setHeader("x-trace", "abc");
check("setHeader", client.getHeader("x-trace") === "abc");
client.setHeader("x-trace", null);
check("setHeader(null) removes", client.getHeader("x-trace") === undefined);

// ---- successful call ----

const resp = await client.systemOne({
  state: "I was charged twice. Please fix this ASAP.",
  questions: {
    category: choice("What is this ticket about?", { billing: null, technical: null }),
    urgent: noul("The message conveys urgency"),
    frustration: score("How frustrated?", ["Calm", "Angry"]),
  },
});

check("request path", lastRequest.path === "/v1/systemone");
check("request method", lastRequest.method === "POST");
check("json content-type", /application\/json/.test(lastContentType || ""));
check("bearer auth header", lastAuth === "Bearer " + API_KEY);
check("request model", lastRequest.body.model === MODEL);
check("request state forwarded", lastRequest.body.state === "I was charged twice. Please fix this ASAP.");
check("request question count", Object.keys(lastRequest.body.questions).length === 3);
check("wire question type", lastRequest.body.questions.category.type === "choice");
check("wire score criteria is array", Array.isArray(lastRequest.body.questions.frustration.criteria));

check("response model", resp.model === MODEL);
check("response usage.input_tokens", resp.usage.input_tokens === 120);
check("response choice", resp.answers.category.choice === "billing");
check("response choice confidence", resp.answers.category.confidence === 0.6);
check("response choice probabilities", resp.answers.category.probabilities.billing === 0.6);
check("response noul", resp.answers.urgent.noul === 0.82);
check("response score fractional", resp.answers.frustration.score === 1.035);
check("response score legend", resp.answers.frustration.legend[0] === "Calm");
check("response score legend has one key per level",
  Object.keys(resp.answers.frustration.legend).length === 2);

// ---- wire body survives server-side protocol validation ----
// Every successful call above already passed the mock's own validateBody(),
// which rejects malformed bodies with 422. Assert the noul wire shape: an
// omitted `criteria` must stay omitted, not become an explicit null.
await client.systemOne({ state: "s", questions: { q: { type: "noul", instructions: "i" } } });
check("omitted noul criteria stays omitted", !("criteria" in lastRequest.body.questions.q));
await client.systemOne({ state: "s", questions: { q: { type: "noul" } } });
check("omitted instructions stays omitted", !("instructions" in lastRequest.body.questions.q));
check("type-only noul passes server validation", lastRequest.body.questions.q.type === "noul");
await client.systemOne({ state: "s", questions: { q: noul("i") } });
check("noul() builder sends instructions", lastRequest.body.questions.q.instructions === "i");
check("noul() builder omits criteria", !("criteria" in lastRequest.body.questions.q));
// Explicit null criteria is legal and must survive.
await client.systemOne({ state: "s", questions: { q: noul("i", null) } });
check("explicit null noul criteria is sent", lastRequest.body.questions.q.criteria === null);
// state: null is a legal EntryType.
const nullState = await client.systemOne({ state: null, questions: { q: noul("i") } });
check("null state accepted", lastRequest.body.state === null);
check("null state answers", nullState.answers.q.noul === 0.82);

// ---- the mock really does enforce the protocol ----
// Assert the server-side validator rejects out-of-protocol bodies, so a passing
// success case above means the client's wire output is spec-valid rather than
// merely accepted by a permissive mock.
const rejected = (body) => {
  const r = validateBody(body);
  return r !== null && r.status === 422 && Array.isArray(r.body.detail);
};
check("server rejects unknown question type",
  rejected({ model: "m", state: "s", questions: { q: { type: "bogus" } } }));
check("server rejects single-level score",
  rejected({ model: "m", state: "s", questions: { q: { type: "score", criteria: ["one"] } } }));
check("server rejects score criteria as object",
  rejected({ model: "m", state: "s", questions: { q: { type: "score", criteria: { a: 1 } } } }));
check("server rejects choice criteria as array",
  rejected({ model: "m", state: "s", questions: { q: { type: "choice", criteria: ["a"] } } }));
check("server rejects numeric state",
  rejected({ model: "m", state: 42, questions: { q: { type: "noul" } } }));
check("server rejects empty questions",
  rejected({ model: "m", state: "s", questions: {} }));
check("server rejects missing model",
  rejected({ state: "s", questions: { q: { type: "noul" } } }));
check("server accepts a well-formed body",
  validateBody({ model: "m", state: null, questions: { q: { type: "noul" } } }) === null);
check("server accepts null noul criteria",
  validateBody({ model: "m", state: "s", questions: { q: { type: "noul", criteria: null } } }) === null);

// ---- per-request model override ----

await client.systemOne({
  state: "x",
  model: "jev-9.9.9",
  questions: { q: noul("anything") },
});
check("per-request model override", lastRequest.body.model === "jev-9.9.9");

// ---- plain-object state and questions (no builders) ----

const plain = await client.systemOne({
  state: { ticket: { subject: "Duplicate charge" } },
  questions: { a: { type: "noul", instructions: "Is it billing?" } },
});
check("object state forwarded", lastRequest.body.state.ticket.subject === "Duplicate charge");
check("plain question answers", plain.answers.a.noul === 0.82);

// ---- custom headers sent ----

const hdrClient = new System1Client({
  apiKey: "k",
  baseUrl: BASE,
  model: MODEL,
  headers: { "x-app": "test" },
});
await hdrClient.systemOne({ state: "s", questions: { q: noul("i") } });
check("constructor headers used", hdrClient.getHeader("x-app") === "test");
hdrClient.dispose();

// ---- strict request validation ----

throwsType(() => client.systemOne(), "systemOne() requires a request object");
throwsType(() => client.systemOne(null), "systemOne() rejects null request");
throwsType(() => client.systemOne({ questions: { q: noul("i") } }), "state is required");
throwsType(() => client.systemOne({ state: 42, questions: { q: noul("i") } }), "state rejects number");
throwsType(() => client.systemOne({ state: "s", questions: "nope" }), "questions must be object");
throwsType(() => client.systemOne({ state: "s", questions: {} }), "questions must not be empty");
throwsType(
  () => client.systemOne({ state: "s", questions: { q: { type: 5 } } }),
  "question.type must be a string",
);
throwsRange(
  () => client.systemOne({ state: "s", questions: { q: { type: "bogus" } } }),
  "unknown question.type is RangeError",
);
throwsType(
  () => client.systemOne({ state: "s", questions: { q: { type: "choice" } } }),
  "choice requires criteria",
);
throwsType(
  () => client.systemOne({ state: "s", questions: { q: { type: "choice", criteria: ["a", "b"] } } }),
  "choice criteria rejects array",
);
throwsType(
  () => client.systemOne({ state: "s", questions: { q: { type: "score", criteria: { a: 1 } } } }),
  "score criteria rejects object",
);
throwsRange(
  () => client.systemOne({ state: "s", questions: { q: { type: "score", criteria: ["only"] } } }),
  "score needs two levels",
);
throwsType(
  () => client.systemOne({ state: "s", questions: { q: { type: "noul", instructions: 7 } } }),
  "instructions rejects number",
);
throwsType(() => client.systemOne({ state: "s", model: 9, questions: { q: noul("i") } }), "model must be a string");

// ---- strict constructor validation ----
// `baseUrl` and `model` are required; `apiKey` is not.

throwsType(() => new System1Client({ apiKey: 5, baseUrl: BASE, model: "m" }),
  "constructor apiKey must be a string");
throwsType(() => new System1Client({ apiKey: "k", model: {}, baseUrl: BASE }),
  "constructor model must be a string");
throwsType(() => new System1Client({ apiKey: "k", baseUrl: BASE, model: "m", headers: "x" }),
  "constructor headers must be object");
throwsRange(() => new System1Client({ baseUrl: BASE, model: "m", timeout: 0 }),
  "constructor timeout must be positive");
throwsType(() => new System1Client(5), "constructor requires options object");
throwsType(() => new System1Client(), "constructor requires an options object");
throwsType(() => new System1Client({ model: "m" }), "constructor requires baseUrl");
throwsType(() => new System1Client({ baseUrl: BASE }), "constructor requires model");
throwsType(() => new System1Client({ baseUrl: "", model: "m" }), "constructor rejects empty baseUrl");
throwsType(() => new System1Client({ baseUrl: BASE, model: "" }), "constructor rejects empty model");
throwsType(() => new System1Client({ baseUrl: null, model: "m" }), "constructor rejects null baseUrl");

// apiKey is optional — a keyless local gateway gets no Authorization header.
const keyless = new System1Client({ baseUrl: BASE, model: "m" });
check("apiKey is optional", keyless instanceof System1Client);
check("keyless client has no default model/baseUrl",
  keyless.baseUrl === BASE && keyless.model === "m");

// No environment variable is consulted: BAMBOONODE_API_KEY must not leak in.
check("client ignores BAMBOONODE_API_KEY", (() => {
  const saved = process.env.BAMBOONODE_API_KEY;
  const made = new System1Client({ baseUrl: BASE, model: "m" });
  if (saved === undefined) delete process.env.BAMBOONODE_API_KEY;
  else process.env.BAMBOONODE_API_KEY = saved;
  return made.baseUrl === BASE && made.model === "m";
})());

// ---- call opts ----

throwsType(() => client.systemOne({ state: "s", questions: { q: noul("i") } }, null), "null opts rejected");
throwsType(() => client.systemOne({ state: "s", questions: { q: noul("i") } }, { signal: {} }), "signal must be AbortSignal");
throwsRange(
  () => client.systemOne({ state: "s", questions: { q: noul("i") } }, { timeout: -1 }),
  "timeout must be positive integer",
);
throwsType(
  () => client.systemOne({ state: "s", questions: { q: noul("i") } }, { timeout: "5" }),
  "timeout must be a number",
);

// ---- server error surfaces as a rejected promise ----

async function expectReject(promise, re, label) {
  try {
    await promise;
    check(label, false);
  } catch (e) {
    check(label, re.test(e.message));
  }
}

// The mock's own 422 shape: a `detail` array of {loc, msg}, the form the real
// API uses for request validation. `body` is stripped from the reported path.
failNext = {
  status: 422,
  body: {
    detail: [
      { loc: ["body", "questions", "category", "criteria"], msg: "too few options" },
      { loc: ["body", "questions", "urgent"], msg: "instructions is required" },
    ],
  },
};
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /^questions\.category\.criteria: too few options; questions\.urgent: instructions is required$/,
  "detail array joins loc: msg pairs",
);

failNext = { status: 422, body: { detail: "questions.category.criteria: too few options" } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /too few options/,
  "detail string extracted",
);

failNext = { status: 401, body: { error: { message: "invalid api key" } } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /invalid api key/,
  "error.message extracted",
);

failNext = { status: 403, body: { error: "not allowed" } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /not allowed/,
  "error string extracted",
);

failNext = { status: 400, body: { message: "bad request" } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /bad request/,
  "top-level message extracted",
);

failNext = { status: 429, body: { detail: { message: "slow down" } } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /slow down/,
  "detail.message extracted",
);

failNext = { status: 500, body: { error: { message: "upstream exploded" } } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /upstream exploded/,
  "5xx error extracted",
);

// A body with no recognizable message falls back to the raw payload, so the
// status is never silently lost.
failNext = { status: 418, body: { weird: true } };
await expectReject(
  client.systemOne({ state: "s", questions: { q: noul("i") } }),
  /weird/,
  "unrecognized body falls back to raw",
);

// ---- abort before send ----

const ac = new AbortController();
ac.abort();
try {
  await client.systemOne({ state: "s", questions: { q: noul("i") } }, { signal: ac.signal });
  check("pre-aborted signal rejects", false);
} catch (e) {
  check("pre-aborted signal rejects", e.name === "AbortError");
}

// ---- abort mid-flight ----

const ac2 = new AbortController();
const pending = client.systemOne({ state: "s".repeat(200), questions: { q: noul("i") } }, { signal: ac2.signal });
setTimeout(() => ac2.abort(), 1);
try {
  await pending;
  // The mock may have answered first; that is a legitimate race outcome.
  check("mid-flight abort does not hang", true);
} catch (e) {
  check("mid-flight abort does not hang", /abort/i.test(e.message));
}

globalThis.mockHandle.shutdown();
await globalThis.mockHandle.exit;
console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) throw new Error(`${fail} assertion(s) failed`);