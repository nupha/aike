// MCP client tests — native StdioMcpClient / HttpMcpClient round trips.
//
//   ../aike/target/debug/aike -m tests/mcp-client.mjs
//
// stdio: spawn an aike StdioMcp server child, handshake, serverInfo,
//        listTools, callTool (incl. tool-level isError and tool-arg
//        JSONSchema rejection), close.
// http:  mount an HttpMcp on an HttpServer and do the same over
//        streamable HTTP with custom headers.

import * as os from "os";

let pass = 0;
let fail = 0;
function check(label, condition, detail) {
  if (condition) {
    console.log(`  ✅ ${label}`);
    pass++;
  } else {
    console.log(`  ❌ ${label}${detail ? " — " + detail : ""}`);
    fail++;
  }
}

const { StdioMcp, HttpMcp, StdioMcpClient, HttpMcpClient } =
  await import("mcp");
const { HttpServer } = await import("http");
const fs = await import("fs");
const path = await import("path");

// ── stdio round trip ──

// Server script runs in a child aike process; tools defined via StdioMcp.
const serverScript = `
import { StdioMcp } from "mcp";
const server = StdioMcp.define({
  name: "test-tools",
  version: "1.2.3",
  instructions: "test server",
  tools: {
    add: {
      desc: "Add two numbers",
      args: {
        a: { desc: "first", type: "integer", required: true },
        b: { desc: "second", type: "integer", required: true },
      },
      handler: (args) => ({ content: [{ type: "text", text: String(args.a + args.b) }] }),
    },
    fail: {
      desc: "Always errors",
      args: {},
      handler: () => { throw new Error("boom"); },
    },
    choose: {
      desc: "Choose a mode",
      args: {
        mode: { desc: "mode", type: "string", enum: ["overwrite", "append"], required: true },
        depth: { desc: "depth", type: "integer", default: 2 },
      },
      handler: (args) => ({ content: [{ type: "text", text: args.mode + ":" + args.depth }] }),
    },
  },
});
await server.run();
export default server;
`;

const scriptPath = os.tmpdir() + "/mcp-client-test-server.mjs";
await fs.writeText(scriptPath, serverScript);
// pathToFileURL, not the raw path: embedding a Windows path in module source
// would eat "\U" etc. as escape sequences.
const scriptUrl = path.pathToFileURL(scriptPath).href;

const client = new StdioMcpClient({
  command: process.execPath,
  args: ["-m", "-e", `import md from '${scriptUrl}'; await md.run();`],
});
check("serverInfo null before connect", client.serverInfo === null);

await client.connect();
check("serverInfo populated after connect", client.serverInfo !== null);
check(
  "handshake reports test-tools",
  client.serverInfo?.serverInfo?.name === "test-tools",
  JSON.stringify(client.serverInfo?.serverInfo),
);

const listed = await client.listTools();
check(
  "listTools returns 3 tools",
  Array.isArray(listed.tools) && listed.tools.length === 3,
);
check(
  "add tool present",
  listed.tools.some((t) => t.name === "add"),
);
check(
  "choose tool present",
  listed.tools.some((t) => t.name === "choose"),
);

const res = await client.callTool("add", { a: 20, b: 22 });
check(
  "callTool add -> 42",
  res.content[0].text === "42",
  JSON.stringify(res.content),
);

const errRes = await client.callTool("fail", {});
check("tool handler error surfaces as isError result", errRes.isError === true);

// ── tool-arg JSONSchema validation (MCP args contract) ──
// An argument that violates the tool's declared schema is rejected BEFORE the
// handler runs, surfaced as a JSON-RPC -32602 error (the client throws).
async function expectReject(label, tool, args, needle) {
  try {
    await client.callTool(tool, args);
    check(label, false, "expected a rejection but the call succeeded");
  } catch (e) {
    check(label, String(e.message).includes(needle), String(e.message));
  }
}

await expectReject(
  "missing required arg rejected",
  "add",
  { a: 20 },
  'missing required argument: "b"',
);
await expectReject(
  "wrong arg type rejected",
  "add",
  { a: "x", b: 2 },
  '"a" must be an integer',
);
await expectReject(
  "enum violation rejected",
  "choose",
  { mode: "patch" },
  'invalid value for "mode"',
);
await expectReject(
  "required enum missing rejected",
  "choose",
  {},
  'missing required argument: "mode"',
);

// A valid enum passes and the schema `default` fills the missing arg.
const defRes = await client.callTool("choose", { mode: "append" });
check(
  "enum accepted + default applied",
  defRes.content[0].text === "append:2",
  JSON.stringify(defRes.content),
);

// Extra keys are ignored (the schema only constrains declared properties).
const extraRes = await client.callTool("add", { a: 1, b: 2, surprise: 9 });
check(
  "extra keys ignored",
  extraRes.content[0].text === "3",
  JSON.stringify(extraRes.content),
);

await client.close();
console.log("  closed stdio client");

// ── http round trip ──

const httpServer = new HttpServer();
const mcpServer = HttpMcp.define({
  name: "http-test-tools",
  version: "0.9.0",
  tools: {
    greet: {
      desc: "Greet someone",
      args: { who: { desc: "name", required: true } },
      handler: (args) => ({
        content: [{ type: "text", text: `hi ${args.who}` }],
      }),
    },
  },
});
httpServer.mcp("/mcp", mcpServer);
httpServer.listen({ port: 18472, host: "127.0.0.1" });
await Aike.sleep(300);

const hclient = new HttpMcpClient({
  url: "http://127.0.0.1:18472/mcp",
  headers: { "x-test": "yes" },
});
await hclient.connect();
check(
  "http handshake ok",
  hclient.serverInfo?.serverInfo?.name === "http-test-tools",
);

const hres = await hclient.callTool("greet", { who: "world" });
check(
  "http callTool works",
  hres.content[0].text === "hi world",
  JSON.stringify(hres.content),
);

await hclient.close();
await httpServer.shutdown();

// ── engine exit without close() must not orphan the server child ──
// Parent script connects, then exits WITHOUT calling close(). The engine's
// exit cleanup (kill_all_mcp_children) has to take the spawned server down.
{
  const markerPath =
    os.tmpdir() + "/mcp-exit-kill-marker-" + Date.now() + ".mjs";
  await fs.writeText(
    markerPath,
    `import { StdioMcp } from "mcp";
     const s = StdioMcp.define({ name: "exit-kill-test", tools: { noop: { desc: "noop", args: {}, handler: () => "ok" } } });
     await s.run();\nexport default s;`,
  );

  const parentScript =
    os.tmpdir() + "/mcp-exit-kill-parent-" + Date.now() + ".mjs";
  await fs.writeText(
    parentScript,
    `const { StdioMcpClient } = await import("mcp");
     const c = new StdioMcpClient({
       command: process.execPath,
       args: ["-m", "-e", "import md from '${path.pathToFileURL(markerPath).href}'; await md.run();"],
     });
     await c.connect();
     // deliberately NO close(): simulate forgotten cleanup / abrupt exit
     console.log("PARENT_CONNECTED");`,
  );

  const parent = Aike.spawn(
    process.execPath,
    ["-m", "-e", `await import('${path.pathToFileURL(parentScript).href}');`],
    { stdin: "pipe", stdout: "pipe", stderr: "inherit" },
  );

  const out = [];
  const dec = new TextDecoder();
  let stdoutEof = false;
  parent.stdout.onData((chunk) => {
    if (chunk === null) {
      stdoutEof = true;
      return;
    }
    out.push(dec.decode(chunk, { stream: true }));
  });
  await parent.exitCode;
  await Aike.sleep(200);
  check(
    "parent connected before exiting",
    out.join("").includes("PARENT_CONNECTED"),
  );

  // give the engine's atexit a moment, then look for the surviving server
  await Aike.sleep(500);
  const ps = Aike.sh("ps -ax -o command=", { timeout: 10000 });
  check(
    "no orphaned server child after parent exit",
    !ps.stdout.includes(markerPath),
  );
}

console.log(`\n=== mcp-client: ${pass} passed, ${fail} failed ===`);
if (fail > 0) process.exit(1);
