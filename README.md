[中文](./README_zh.md)

# aike - The JS/TS runtime for scripting AI apps

**aike** is named after "Ai + ke" (壳, "shell"): AI infra parts packed into one shell, nicknamed "braincase". It is the JavaScript / TypeScript runtime for scripting AI apps. It runs on [zuqe](https://github.com/nupha/zuqe), a small-footprint engine.

AI infra parts — model calls, local inference, databases, browser automation, embeddings, syntax parsing, sandboxing — are built into native modules and shipped in the aike distro.

## Use cases

### Embed in an existing product

Desktop apps, IDE plugins, industrial devices, offline terminals. The host calls aike to run a script; the script exits when done. Ship aike inside the installer — no deployment, no ops.

### Turn a pile of material into one reliable result

Contracts → field tables. Work orders → category labels. Tens of GB of logs → one daily report. A whole directory → one searchable index.

The common traits: batch, repeatable, verifiable.

The `json-schema` module ties model output to your own data structures: `fromSimpleSchema` writes a schema, `extractJSON` pulls JSON out of free-form text, `validateSchema` / `matchesType` check it. All module functions — no per-project rewrites.

### AI apps on local data

Intranets, classified data, healthcare, education. `llama` runs local inference, `sqlite` / `redb` store locally, vector search stays local too. Everything that can stay local, stays local. No need to hand over a whole document set just to use AI once.

Requests leave from the caller's process, which decides what to send, where, and how much.

### Run model-generated code safely

Elsewhere you usually need a container, or a second embedded engine. aike has one runtime; the difference is configuration.

`Aike.createRuntime({ global: ["console", "crypto"] })` grants **global** capabilities by list. Omit `global` and the sub-runtime gets only the language: no files, no network, no outside contact. Open several in one process; heaps stay independent.

Modules are not on that list. Whether a sub-runtime may `import` a module is decided separately by `importPolicy`:

```js
const rt = Aike.createRuntime({
  global: ["console"],
  importPolicy: (name) => name === "ai", // modules get module names; script files get absolute paths
});
```

Without `importPolicy`, a sub-runtime **cannot import anything**. With one, every `import` asks the policy; only `true` passes.

The policy runs **synchronously** in the creator's context: it returns a boolean and must not create or close runtimes — re-entry breaks the runtime tree and throws `TypeError`. If it throws, the import fails and the error passes through unchanged.

### Building agents

Streaming output, tool calls, MCP connections, argument schema checks, context assembly — all inside the runtime.

Your code only says what to call and when to stop. SSE stream parsing is built in.

### Writing MCP servers

**aike** writes MCP servers in both transports: `StdioMcp` over stdin/stdout, `HttpMcp` over Streamable HTTP mounted on an `HttpServer`.

## Aike distro

[TBD: pre-build distro downloads]

After installation, you get this layout:

```
<INSTALL_ROOT>/
├── bin/aike             # the runtime binary
└── lib/aike/modules/
    ├── *.d.ts  …        # type declarations for all modules
    ├── llama.aike       # an extension module: shared library
    ├── browser.aike
    ├── tree_sitter.aike
    ├── sqlite.aike
    ├── postgres.aike
    ├── redb.aike
    ├── audio.aike
    ├── speech.aike
    ├── gui.aike
    └── ...
```

The binary `aike` is the runtime program; `lib/aike/modules/` holds the modules.

Note

1. Every standard module ships a `.d.ts` next to its `.aike` file. Type checking, editor completion, and hints all read these declarations. Coding agents read them too — no guessing APIs. Declarations follow module versions: change the API, change the declaration.
1. This repo builds only the runtime. Extension module sources live outside this repo.

## Sandbox

Built-in safety for running untrusted code. Two kinds, picked by what you confine: a piece of JavaScript, or a whole program.

### Light sandbox - for a piece of JavaScript

`Aike.createRuntime` opens a sub-runtime on the _same thread_: cheap enough to open many, heaps isolated. `global` grants the globals and APIs, `importPolicy` grants the modules. Grant neither and only the language remains — no files, no network. For evaluating JS tool calls, model-generated expressions, and untrusted snippets.

```ts
const rt = Aike.createRuntime({ timeout: 1000 }); // omit global = language only
console.log(await rt.run("typeof fetch")); // "undefined"
console.log(await rt.run("1 + 1")); // 2
rt.close();
```

### Process sandbox - for a program

`Aike.spawn` starts a child process inside an OS-enforced sandbox: Seatbelt on macOS, AppContainer on Windows, bubblewrap on Linux. The working directory is the only place it may write; `grants` opens more paths, `denied` closes paths completely, and the network stays off until you ask. For external commands and third-party tools.

```ts
const child = Aike.spawn("cat", ["data/report.txt"], {
  cwd: "/path/to/app",
  sandbox: {
    grants: [
      { path: "/path/to/app/data", access: "r" }, // may read this directory
    ],
    denied: ["/path/to/app/.env"], // never this file
    network: "none", // no outbound connections
  },
});
console.log(child.sandboxed); // true
console.log(await child.stdout?.text()); // contents of report.txt
```

_Note: if the OS sandbox backend is unavailable, spawn throws — never runs unsandboxed. Check `Aike.sandboxSupport()` first._

## Modules

### Basics

Everyday built-ins:

- `fs` — read, write, glob, grep
- `path` — join, resolve, parse
- `os` — platform, arch, well-known directories
- `http` — HTTP server with routing
- `yaml`, `jsonl` — parse and write data formats
- `json-schema` — validate model output against a JSON-Schema; coerce obvious mismatches
- `jinja2` — templates
- `html` — parse pages, convert HTML to markdown

````ts
import * as fs from "fs";
import * as path from "path";
import * as os from "os";
import { YAML } from "yaml";
import * as jsonl from "jsonl";
import JSONSchema from "json-schema";
import { Jinja2 } from "jinja2";
import { html2markdown } from "html";
import { HttpServer } from "http";

// os
console.log(os.platform(), os.arch()); // e.g. darwin x64

// fs: write, read, glob
await fs.writeText("notes.txt", "hello aike");
console.log(await fs.readText("notes.txt")); // hello aike
console.log(await fs.glob("*.txt")); // [ 'notes.txt', ... ]

// path
console.log(path.join("/data", "app", "db.sqlite")); // /data/app/db.sqlite

// yaml / jsonl
console.log(YAML.parse("name: aike")); // { name: 'aike' }
console.log(jsonl.stringify([{ id: 1 }])); // {"id":1}

// json-schema: extract JSON from model text, then check it against your schema
const schema = {
  type: "object",
  properties: { id: { type: "integer" }, name: { type: "string" } },
  required: ["id"],
};
const value = JSONSchema.extractJSON(
  'Result: ```json\n{"id":1,"name":"aike"}\n```',
);
console.log(JSONSchema.validateSchema(value, schema));
// { ok: true, value: { id: 1, name: 'aike' }, errors: [] }
console.log(JSONSchema.validateSchema({ id: "x" }, schema));
// { ok: false, value: { id: 'x' }, errors: [ 'root.id: expected integer' ] }
console.log(JSONSchema.validateSchema({ id: "42" }, schema, "coerce"));
// { ok: true, value: { id: 42 }, errors: [] } — coercion fixes obvious mismatches

// jinja2
console.log(new Jinja2().render("Hi {{ name }}", { name: "aike" })); // Hi aike

// html
console.log(html2markdown("<h1>Hello</h1>")); // # Hello

// http: serve one request, then shut down
const server = new HttpServer();
server.get("/hi", (req, res) => res.end("hello"));
const handle = server.listen({ port: 8787 });
console.log(await (await fetch("http://127.0.0.1:8787/hi")).text()); // hello
handle.shutdown();
await handle.exit;
````

### Call an LLM

```ts
import { LlmClient } from "ai";

const llm = new LlmClient({
  apiKey: "sk-xxxx",
  baseUrl: "https://api.openai.com/v1",
  model: "gpt-4o-mini",
});

for await (const chunk of llm.chat({
  messages: [
    { role: "user", content: "What is a JavaScript runtime, in one sentence?" },
  ],
})) {
  process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
}
```

### Local inference

```ts
// local.ts
import * as llama from "llama";

const model = new llama.Model("./models/Qwen3.5-4B-Q4_K_M.gguf");
const engine = llama.Engine.createEngine(model, {
  contextSize: 4096,
  maxCompletionTokens: 256,
});

const reply = engine.chat(
  [
    {
      role: "user",
      content: "Explain why local inference matters, in one sentence",
    },
  ],
  { temperature: 0.7, thinking: false },
);
console.log(reply.text, reply.usage);

engine.dispose();
```

### Automate a browser

```ts
// page.ts
import { Browser } from "browser";

const browser = await Browser.launch({ headless: true });
const page = await browser.newPage();
await page.navigate("https://example.com");
await page.waitForSelector("h1", 5000);

console.log(await page.evaluate("document.title"));
console.log(await page.evaluate("document.body.innerText"));

await browser.close();
```

### Using databases

Modules cover SQL databases, key-value stores, and vector databases.

```ts
import { Database } from "sqlite";

const db = new Database(":memory:");
await db.execute("create table t (id integer primary key, name text)");
await db.execute("insert into t (name) values (?)", "aike");
for await (const row of db.query("select * from t")) console.log(row); // { 'id': 1, 'name': 'aike' }
db.close();
```

### Writing MCP servers

Write MCP servers with aike: describe a tool once — name, description, argument list, handler — and the `args` DSL compiles to JSON Schema. The file-system, browser and web-search servers in `aike-mcp` are built this way.

#### Stdio transport

```ts
import { StdioMcp } from "mcp";

const server = StdioMcp.define({
  name: "demo",
  version: "0.1.0",
  tools: {
    word_count: {
      desc: "Count words in a text.",
      handler: async ({ text }) => ({ words: text.trim().split(/\s+/).length }),
      args: {
        text: { desc: "Text to count.", type: "string", required: true },
      },
    },
  },
});

await server.run();
```

Run it with `aike -m mcp-server.ts` and it speaks MCP over stdio: clients list the tools, the input schema comes from your `args`, and calls land in your handler.

#### HTTP transport

`HttpMcp` doesn't run by itself; it mounts onto an `HttpServer`:

```ts
import { HttpMcp } from "mcp";
import { HttpServer } from "http";

// http mcp
const mcpServer = HttpMcp.define({
  name: "demo",
  version: "0.1.0",
  tools: {
    word_count: {
      desc: "Count words in a text.",
      handler: async ({ text }) => ({ words: text.trim().split(/\s+/).length }),
      args: {
        text: { desc: "Text to count.", type: "string", required: true },
      },
    },
  },
});

const http = new HttpServer();
http.mcp("/mcp", mcpServer);
http.listen({ port: 8787 });
```

Clients then talk Streamable HTTP at `http://localhost:8787/mcp`.

### See more modules

[TBD: module index]

### When shipped modules are not enough

Distro-packaged modules cover common needs. If yours is not collected yet, do not wait: use **zapi** to wrap your favorite library into an extension module, then `import` it from JS as usual.

#### Writing a native module with zapi

**zapi** is aike's native extension interface. An extension is a dynamic library. Write plain Rust; a few macros generate the export table. Classes, async, threads, and GC tracing all have ready patterns. Rename the dynamic library to `.aike`, drop it in the working directory, the standard module directory, or the `AIKE_LIB_PATH` directory — then `import` it.

For details, see `skills/SKILL.md` in the zapi repo: https://github.com/nupha/zuqe-zapi — let your agent read it.

#### Module search path

Only bare names (`sqlite`, `vendor/lib`) go through the search path. Relative paths (`./x`, `../x`), absolute paths, and URLs never do.

Directories are checked in order; first hit wins:

1. **Distro module directory** `<real binary path>/../../lib/aike/modules`. The runtime resolves its own real path (following symlinks), then goes up two levels. This is what the current binary ships; same-named files in your project cannot override it. Skipped if the directory does not exist.
2. **The importer's directory**: where the file with the `import` lives.
3. **Current working directory (CWD)**.
4. **`$AIKE_LIB_PATH`**: a single directory; skipped when unset or empty.

Inside each directory, files are looked up in this order:

- If `<dir>/<name>` is a directory: look only for `index.js`, `index.ts`, `index.aike` inside. If none exist, move to the next directory.
- If it is not a directory: a name with an extension (like `notes.aike`) matches only that file; without one, try `name.js`, `name.ts`, `name.aike` in order.

A hit on `.aike` loads as a zapi extension; everything else loads as source. `.js` falls back to `.ts`, `.mjs` to `.mts`, `.cjs` to `.cts`. `require()` and `module.paths` use the same list.

Recommended layout: **standard modules stay on the standard path; your own extensions go in one directory pointed to by `AIKE_LIB_PATH`.**

```sh
AIKE_LIB_PATH=$HOME/.aike/mods aike app.ts   # app.ts does: import { x } from "myext"
```

The two sources stay apart: upgrading the distro never touches your extensions, and your extensions never touch the directory next to the binary.

Two things to note. `AIKE_LIB_PATH` takes one directory only — no colon-separated list. It has the lowest priority; same-named files in project directories win first. To make your extension the default, do not leave a same-named file next to your script.

## How aike differs from Node, Bun, and Deno

**Node / Bun / Deno are general-purpose runtimes. aike is a dedicated runtime for AI applications.**

|                         | Node / Bun / Deno                                                                          | aike                                                                                                                                                |
| ----------------------- | ------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Positioning             | General-purpose JS runtimes; run any code                                                  | Built for one goal: writing AI applications                                                                                                         |
| AI app infrastructure   | Install dependencies yourself                                                              | Shipped in the distro                                                                                                                               |
| Standards               | Carry a burden of backward compatibility                                                   | APIs designed to ECMA / WHATWG; no legacy compatibility                                                                                             |
| Engine                  | Large general engines, aimed at big services                                               | Small embedded engine `zuqe` (one crate), built for embedding and light loads                                                                       |
| External modules        | npm / JSR ecosystems                                                                       | Distro modules plus references to external modules. Most npm packages do not work as-is; ones with native dependencies are even less likely to work |
| Runtimes in one process | workers / sub-processes / isolates, each with extra overhead                               | Several isolated sub-runtimes on one thread, driven by the host event loop                                                                          |
| Running generated code  | Containers, or a separate embedded engine. Deno can also do it with permissions + isolates | Two sandbox levels: capability list + import policy for sub-runtimes; OS policy for child processes                                                 |

**aike** opens several isolated sub-runtimes on a single thread. Each has its own heap, driven by the host's event loop. Isolate tasks cheaply without spawning processes. Values crossing the boundary must be serializable; functions, promises, and symbols throw `TypeError`.

Host and sandbox share one binary and one set of language capabilities. Running generated JS safely needs no second embedded runtime.

## Status

Under active development. APIs still change.

## License

Apache-2.0.
