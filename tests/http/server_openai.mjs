// Optional LLM test server: loads a model and exposes an OpenAI-compatible
// endpoint. Separate from server.mjs because it requires a model file and the
// `llama_cpp` build feature — run it only when you want to exercise the LLM
// serving path.
//
// Run:  AIKE_MODEL_PATH=../models/.../model.gguf cargo run -- tests/http/server_llama.mjs
// Or pass the path inline (see MODEL_PATH below).

import { Model } from "llama";
import { HttpServer } from "http";

// Model path: override via AIKE_MODEL_PATH env var (set on the cargo command),
// or edit it here.
const MODEL_PATH =
  (typeof process !== "undefined" && process.env && process.env.AIKE_MODEL_PATH) || "";

if (!MODEL_PATH) {
  console.log("[server_llama] AIKE_MODEL_PATH not set; nothing to serve.");
  console.log(
    "  example: AIKE_MODEL_PATH=../models/qwen35/Qwen3.5-4B-Q4_K_M.gguf \\",
  );
  console.log("           cargo run -- tests/http/server_llama.mjs");
  await new Promise(() => {});
  throw new Error("no model path");
}

console.log("[server_llama] loading model:", MODEL_PATH);
const model = await Model.load(MODEL_PATH, {
  useMmap: true,
  useDirectIo: true,
  gpuLayers: 99,
  splitMode: "none",
  checkTensors: false,
});
console.log("[server_llama] model loaded");

const ctx = model.createContext({
  contextSize: 8192,
  batchSize: 512,
  ubatchSize: 512,
  kvUnified: true,
  sequences: 4,
  threads: 6,
  threadsBatch: 8,
  cache: { k: "q8_0", v: "q8_0" },
});

const engine = ctx.createEngine({
  idleSleepMs: 1,
  slotMatchThreshold: 0.5,
  maxTokens: 1024,
});

const svr = new HttpServer();
svr.openai(engine, "/openai");

svr.listen({ host: "0.0.0.0", port: 8080 });

console.log("\n[server_llama] OpenAI-compatible endpoint on http://127.0.0.1:8080");
console.log("  health   : curl http://127.0.0.1:8080/v1/health");
console.log("  models   : curl http://127.0.0.1:8080/v1/models");
console.log(
  "  chat     : curl -X POST http://127.0.0.1:8080/v1/chat/completions \\",
);
console.log('             -H "Content-Type: application/json" \\');
console.log(
  "             -d '{\"model\":\"default\",\"messages\":[{\"role\":\"user\",\"content\":\"say hi\"}],\"stream\":false}'",
);

await new Promise(() => {});
