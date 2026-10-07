// Tests the `cors` option of HttpServer.listen().
//
// The permissive CORS layer is opt-out: with `cors: false` the server must send
// no `Access-Control-*` headers at all, so a CORS-enforcing client sees the
// failure it would see against any non-CORS origin.
//
// Assertions go through HttpClient, which does not apply CORS, so the header
// values can be inspected directly.
//
// Run: aike -m tests/http/cors_option.mjs

import { HttpClient, HttpServer } from "http";

let pass = 0;
let fail = 0;

function check(label, ok, detail) {
  if (ok) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

const client = new HttpClient();

/** Start a server on `port` returning ACAO (or null), then stop it. */
async function allowOriginFor(port, listenOpts) {
  const svr = new HttpServer();
  svr.get("/", (_req, res) => res.end("ok"));
  svr.listen({ host: "127.0.0.1", port, ...listenOpts });
  const r = await client.get(`http://127.0.0.1:${port}/`, { timeout: 10 });
  const acao = r.headers.get("access-control-allow-origin");
  await svr.shutdown();
  return acao;
}

async function main() {
  console.log("\n=== listen({ cors }) ===");

  // Default: the permissive layer is attached, so ACAO is present.
  const withCors = await allowOriginFor(18141, {});
  check("default attaches permissive CORS", withCors === "*", `got ${withCors}`);

  // Explicit true is the same as the default.
  const explicitOn = await allowOriginFor(18142, { cors: true });
  check("cors: true attaches permissive CORS", explicitOn === "*", `got ${explicitOn}`);

  // Opt-out: no Access-Control-* headers at all.
  const withCorsOff = await allowOriginFor(18143, { cors: false });
  check("cors: false sends no ACAO", withCorsOff === null, `got ${withCorsOff}`);

  // Strict validation: `cors` is a boolean or absent, nothing else.
  const svr = new HttpServer();
  svr.get("/", (_req, res) => res.end("ok"));
  let threw = null;
  try {
    svr.listen({ host: "127.0.0.1", port: 18144, cors: "no" });
  } catch (e) {
    threw = e;
  }
  check(
    "cors: non-boolean throws TypeError",
    threw instanceof TypeError,
    threw ? `${threw.name}: ${threw.message}` : "no throw",
  );

  threw = null;
  try {
    svr.listen({ host: "127.0.0.1", port: 18145, cors: null });
  } catch (e) {
    threw = e;
  }
  check(
    "cors: null throws TypeError",
    threw instanceof TypeError,
    threw ? `${threw.name}: ${threw.message}` : "no throw",
  );

  // The rejected listen() must not have left the server running.
  check("rejected listen() leaves server stopped", svr.isRunning === false);

  console.log(`\n=== SUMMARY: pass=${pass} fail=${fail} ===`);
  console.log(fail > 0 ? "RESULT: FAIL" : "RESULT: PASS");
}

await main();