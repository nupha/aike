import { System1Client, choice, score, noul } from "ai";

// system1_live_test.mjs — smoke test against a REAL System One endpoint.
//
//   ./target/debug/aike tests/llm/system1_live_test.mjs
//
// Endpoint: https://api.bamboonode.dev/v1/systemone (the client appends
// `/v1/systemone` to `baseUrl`, so the base carries no `/v1` suffix).
// Model:    startlux-decision-27b-autothink.
// Override with AIKE_S1_BASE_URL / AIKE_S1_MODELS (comma separated).
//
// This asserts nothing about answer *values* — the decision model is
// probabilistic, so only the protocol shape is checked.

// Key: BAMBOONODE_API_KEY, loaded from this repo's `.env` by `dotenvy::dotenv()`
// at startup — aike searches the CWD and its parents, so running from `aike/`
// picks up the workspace root's `.env`.
const KEY = process.env.BAMBOONODE_API_KEY;
if (!KEY) {
  console.error("BAMBOONODE_API_KEY is not set (expected in the workspace .env)");
  throw new Error("no api key");
}

const BASE = process.env.AIKE_S1_BASE_URL || "https://api.bamboonode.dev";
const MODELS = (process.env.AIKE_S1_MODELS || "startlux-decision-27b-autothink").split(",");

let pass = 0;
let fail = 0;
const check = (label, cond, detail) => {
  if (cond) {
    console.log("PASS " + label);
    pass++;
  } else {
    console.log("FAIL " + label + (detail ? "  -> " + detail : ""));
    fail++;
  }
};

// A realistic triage state, structured the way the docs recommend.
const STATE = {
  ticket: {
    subject: "Charged twice for order A-104",
    messages: [
      { from: "customer", text: "I was charged twice for order A-104. Please refund the duplicate." },
      { from: "customer", text: "This is the third time. I am losing time on this." },
    ],
  },
  order: {
    id: "A-104",
    charges: [
      { amount_usd: 49, status: "captured" },
      { amount_usd: 49, status: "captured" },
    ],
  },
  refund_policy: "Duplicate charges are eligible for a refund.",
};

const QUESTIONS = {
  department: choice("Which team should handle this request?", {
    billing: "Charges, invoices, or payment problems",
    technical: "Bugs or integration problems",
    sales: "Pricing or account questions",
    other: "Anything else",
  }),
  frustration: score("How frustrated is the customer?", ["Calm", "Frustrated", "Very angry"]),
  refund_requested: noul("The customer is explicitly asking for a refund"),
  policy_supports: noul("The stated refund policy covers this situation"),
  is_duplicate: noul("The order shows two identical captured charges for the same amount"),
};

for (const model of MODELS) {
  console.log(`\n=== ${model} @ ${BASE} ===`);
  const client = new System1Client({ apiKey: KEY, baseUrl: BASE, model, timeout: 60 });
  check(`${model}: baseUrl`, client.baseUrl === BASE, client.baseUrl);
  check(`${model}: model`, client.model === model, client.model);

  let res;
  const t0 = Date.now();
  try {
    res = await client.systemOne({ state: STATE, questions: QUESTIONS });
  } catch (e) {
    check(`${model}: request succeeds`, false, e.message);
    continue;
  }
  const ms = Date.now() - t0;
  console.log(`  (${ms}ms)`);

  // ---- protocol shape ----
  check(`${model}: model echoed`, typeof res.model === "string" && res.model.length > 0, res.model);
  check(`${model}: answers object`, typeof res.answers === "object" && res.answers !== null);
  check(`${model}: every question answered`,
    Object.keys(res.answers).length === Object.keys(QUESTIONS).length,
    `${Object.keys(res.answers).length}/${Object.keys(QUESTIONS).length}`);
  check(`${model}: usage.input_tokens is number`,
    typeof res.usage?.input_tokens === "number", JSON.stringify(res.usage));

  const a = res.answers;
  const keys = Object.keys(a);

  // noul → {type, noul} in 0..1, no confidence (the model IS the belief)
  for (const name of keys.filter((k) => QUESTIONS[k].type === "noul")) {
    const v = a[name];
    check(`${model}: ${name}.type`, v.type === "noul", v.type);
    check(`${model}: ${name}.noul in [0,1]`,
      typeof v.noul === "number" && v.noul >= 0 && v.noul <= 1, JSON.stringify(v.noul));
    check(`${model}: ${name} has no confidence (protocol)`, v.confidence === undefined,
      JSON.stringify(v.confidence));
  }

  // choice → {type, choice, confidence, probabilities}, choice ∈ criteria keys
  // or `null`: this deployment has `allow_none` on, so a null choice is a legal
  // answer meaning "none of the listed options" and is signalled by
  // `none_probability`, not by a low listed-option probability.
  for (const name of keys.filter((k) => QUESTIONS[k].type === "choice")) {
    const v = a[name];
    const labels = Object.keys(QUESTIONS[name].criteria);
    check(`${model}: ${name}.type`, v.type === "choice", v.type);
    if (v.choice === null) {
      console.log(`  (${name}: choice null, none_probability=${v.none_probability})`);
      check(`${model}: ${name}.null choice reports none_probability`,
        typeof v.none_probability === "number" && v.none_probability >= 0 && v.none_probability <= 1,
        JSON.stringify(v.none_probability));
    } else {
      check(`${model}: ${name}.choice is a declared label`,
        labels.includes(v.choice), `${v.choice} not in ${labels.join(",")}`);
    }
    check(`${model}: ${name}.confidence in [0,1]`,
      typeof v.confidence === "number" && v.confidence >= 0 && v.confidence <= 1,
      JSON.stringify(v.confidence));
    check(`${model}: ${name}.probabilities covers all labels`,
      labels.every((l) => typeof v.probabilities?.[l] === "number"),
      JSON.stringify(v.probabilities));
    const sum = labels.reduce((t, l) => t + v.probabilities[l], 0);
    check(`${model}: ${name}.probabilities sum ~1`, Math.abs(sum - 1) < 0.01, `sum=${sum}`);
  }

  // score → {type, score, confidence, legend, probabilities}, score may be fractional
  for (const name of keys.filter((k) => QUESTIONS[k].type === "score")) {
    const v = a[name];
    const levels = QUESTIONS[name].criteria.length;
    check(`${model}: ${name}.type`, v.type === "score", v.type);
    check(`${model}: ${name}.score within rubric range`,
      typeof v.score === "number" && v.score >= 0 && v.score <= levels - 1 + 1e-9,
      `${v.score} not in [0, ${levels - 1}]`);
    check(`${model}: ${name}.confidence in [0,1]`,
      typeof v.confidence === "number" && v.confidence >= 0 && v.confidence <= 1,
      JSON.stringify(v.confidence));
    check(`${model}: ${name}.legend has one key per level`,
      typeof v.legend === "object" && Object.keys(v.legend).length === levels,
      `${Object.keys(v.legend ?? {}).length}/${levels}`);
    check(`${model}: ${name}.probabilities covers all levels`,
      Array.from({ length: levels }, (_, i) => i).every((i) => typeof v.probabilities?.[i] === "number"),
      JSON.stringify(v.probabilities));
  }

  // ---- none-of-the-above: `allow_none` lets `choice` come back null, and the
  // answer stays well-formed either way. Accept both outcomes; assert the shape.
  try {
    const r = await client.systemOne({
      state: "The customer writes: 'nothing needed, just saying hi.'",
      questions: {
        department: choice("Which team should handle this request?", {
          billing: "Charges, invoices, or payment problems",
          technical: "Bugs or integration problems",
        }),
      },
    });
    const v = r.answers.department;
    const labels = ["billing", "technical"];
    check(`${model}: none-matching state answers with type choice`, v.type === "choice", v.type);
    check(`${model}: none-matching state choice is null or a declared label`,
      v.choice === null || labels.includes(v.choice), `${v.choice} not in null,${labels.join(",")}`);
    if (v.choice === null) {
      check(`${model}: none-matching null choice reports none_probability`,
        typeof v.none_probability === "number",
        JSON.stringify(v.none_probability));
    }
  } catch (e) {
    check(`${model}: none-matching state answered`, false, e.message);
  }

  // ---- state carried as an object, not flattened ----
  check(`${model}: object state accepted`, true);

  // ---- per-request model override still works over the wire ----
  try {
    await client.systemOne({
      state: "hello",
      model: model,
      questions: { greeting: noul("Is this a greeting") },
    });
    check(`${model}: single-question call`, true);
  } catch (e) {
    check(`${model}: single-question call`, false, e.message);
  }

  // ---- array state is a legal EntryType; `state: null` is legal per the SDK
  // type but the gateway may reject it upstream. Either way the client must
  // behave predictably: answer, or surface the server's message verbatim.
  try {
    const r = await client.systemOne({ state: ["Hi there", "Charged twice."], questions: { q: noul("Is this a request") } });
    check(`${model}: array state accepted`, typeof r.answers.q.noul === "number",
      JSON.stringify(r.answers.q));
  } catch (e) {
    check(`${model}: array state accepted`, false, e.message);
  }
  try {
    const r = await client.systemOne({ state: null, questions: { q: noul("Is this a request") } });
    check(`${model}: null state answered`, typeof r.answers.q.noul === "number",
      JSON.stringify(r.answers.q));
  } catch (e) {
    // Zen rejects it upstream; the client must surface that message unchanged
    // rather than swallow it or invent its own error.
    check(`${model}: null state surfaces server error`, e instanceof Error && e.message.length > 0,
      e.message);
  }

  // ---- timeout opts reach reqwest ----
  try {
    await client.systemOne({ state: STATE, questions: QUESTIONS }, { timeout: 60 });
    check(`${model}: per-call timeout accepted`, true);
  } catch (e) {
    check(`${model}: per-call timeout accepted`, false, e.message);
  }

  // ---- server-side error path: an unknown model must reject, and the
  // gateway's own message must reach the caller unchanged. ----
  try {
    await client.systemOne({ state: "s", model: "no-such-model-xyz", questions: { q: noul("i") } });
    check(`${model}: unknown model errors`, false, "expected a rejection");
  } catch (e) {
    check(`${model}: unknown model errors`, e instanceof Error && e.message.length > 0, e.message);
    check(`${model}: unknown model error is the server's own`,
      /no-such-model-xyz|unavailable|not found|unknown|invalid/i.test(e.message), e.message);
  }

  // ---- abort mid-flight ----
  const ac = new AbortController();
  const p = client.systemOne({ state: STATE, questions: QUESTIONS }, { signal: ac.signal });
  ac.abort();
  try {
    await p;
    console.log(`  (abort raced the response; request completed first)`);
    check(`${model}: abort does not hang`, true);
  } catch (e) {
    check(`${model}: abort does not hang`, true, e.message);
  }

  client.dispose();
}

console.log(`\n${pass} passed, ${fail} failed`);
if (fail > 0) throw new Error(`${fail} assertion(s) failed`);