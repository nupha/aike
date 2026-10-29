[English](./README.md)

# aike - 为写 AI 应用而生的 JS/TS 运行时

**aike** 取名自 "Ai + ke"（壳）：AI 零部件装进一个壳里带走，俗称"脑壳"。它是专为编写 AI 应用而生的 JavaScript / TypeScript 运行时。运行在 [zuqe](https://github.com/nupha/zuqe) 引擎上，内存占用小。

AI 零部件——模型调用、本机推理、数据库、浏览器自动化、向量嵌入、语法解析、沙箱——逐一做成原生模块，随 aike 发行包发布。

## 使用场景

aike 的用法包括：

### 1. 构建 Agent

流式输出、tool call、MCP 连接、参数 schema 校验、上下文拼接，全在 aike 内。

代码只写调什么、何时停。SSE 流解析已内置。

### 2. 编写 MCP 服务器

**aike** 能写两种传输的 MCP 服务器：`StdioMcp` 走 stdin/stdout，`HttpMcp` 走 Streamable HTTP，挂在 `HttpServer` 上。

### 3. 在本地做推理与嵌入

模型从磁盘加载，生成和嵌入都在本机完成。不调 API，不按 token 计费——断网也能用。

### 4. 把文档和日志变成结构化结果

合同、工单、日志、整个目录——aike 批量读入，输出字段表、分类标签、日报、可检索索引。批量、可重复、可验收——每个结果都由 `json-schema` 模块校验。

### 5. 在本地优先的环境里引入 AI 能力

内网、涉密、医疗、教育——数据不能外发的地方。能留在本地的全部留在本地：不必为用一次 AI 交出整份材料。

请求从调用方进程发出，发什么、发往哪、发多少，都由调用方决定。

### 6. 运行不可信代码

模型刚写的代码、任何你信不过的代码，都放进沙箱里跑。不要容器，不要第二个引擎：一个运行时，两级沙箱。细节与演示见 [沙箱](#沙箱) 章。

## aike 发行包

[TBD: 预编译发行包下载]

安装后的目录结构：

```
<INSTALL_ROOT>/
├── bin/aike             # 运行时二进制
└── lib/aike/modules/
    ├── *.d.ts  …        # 所有模块的类型声明
    ├── llama.aike       # 扩展模块：原生共享库
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

二进制 `aike` 是运行时程序，`lib/aike/modules/` 放模块。

注意

1. 每个标准模块的 `.aike` 旁边都带一份 `.d.ts`。类型检查、编辑器补全、语法提示都读这些声明，写代码的 agent 也读——不必猜 API。声明跟着模块版本走：改 API 就改声明。
1. 本仓库只构建运行时。扩展模块的源码在本仓库之外。

## 沙箱

内置的不可信代码安全机制。两种，按关的是什么选：一段 JavaScript，还是一个完整程序。

### 轻量沙箱——关一段 JavaScript

`Aike.createRuntime` 在**同一线程**上开子运行时：便宜到可以开很多个，堆彼此隔离。`global` 授予全局与 API，`importPolicy` 授予模块。两样都不给，就只剩语言本身——没有文件，没有网络。用于执行 JS tool call、模型生成的表达式和不受信任的片段。

```ts
const rt = Aike.createRuntime({ timeout: 1000 }); // 省略 global = 只给语言本身
console.log(await rt.run("typeof fetch")); // "undefined"
console.log(await rt.run("1 + 1")); // 2
rt.close();
```

模块不在 `global` 清单里；子运行时能否 `import`，由 `importPolicy` 单独裁决：

```js
const rt = Aike.createRuntime({
  global: ["console"],
  importPolicy: (name) => name === "ai", // 模块给模块名，脚本文件给绝对路径
});
```

不传 `importPolicy`，子运行时**禁止一切 import**。传了，每次 `import` 都问一遍策略，返回 `true` 才放行。

策略在创建者的上下文里**同步**执行：必须同步返回布尔值，且不得创建或关闭运行时——重入会破坏运行时树，抛 `TypeError`。策略抛错则 import 失败，错误原样带出。

### 进程沙箱——关一个程序

`Aike.spawn` 在操作系统强制的沙箱里启动子进程：macOS 用 Seatbelt，Windows 用 AppContainer，Linux 用 bubblewrap。工作目录是唯一可写处；`grants` 开放更多路径，`denied` 彻底封死路径，网络默认关闭。用于跑外部命令和第三方工具。

```ts
const child = Aike.spawn("cat", ["data/report.txt"], {
  cwd: "/path/to/app",
  sandbox: {
    grants: [
      { path: "/path/to/app/data", access: "r" }, // 可读这个目录
    ],
    denied: ["/path/to/app/.env"], // 这个文件永远读不到
    network: "none", // 不放行网络
  },
});
console.log(child.sandboxed); // true
console.log(await child.stdout?.text()); // report.txt 的内容
```

_注意：沙箱后端不可用时，spawn 直接抛错——绝不脱离沙箱运行。先用 `Aike.sandboxSupport()` 检查。_

## 模块

### 基础模块

日常内置模块：

- `fs` — 读、写、glob、grep
- `path` — join、resolve、parse
- `os` — 平台、架构、常用目录
- `http` — 带路由的 HTTP 服务器
- `yaml`、`jsonl` — 解析与写入数据格式
- `json-schema` — 按 JSON-Schema 校验模型输出；可纠正明显类型偏差
- `jinja2` — 模板
- `html` — 解析页面，HTML 转 markdown

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
console.log(os.platform(), os.arch()); // 例如 darwin x64

// fs：写、读、glob
await fs.writeText("notes.txt", "hello aike");
console.log(await fs.readText("notes.txt")); // hello aike
console.log(await fs.glob("*.txt")); // [ 'notes.txt', ... ]

// path
console.log(path.join("/data", "app", "db.sqlite")); // /data/app/db.sqlite

// yaml / jsonl
console.log(YAML.parse("name: aike")); // { name: 'aike' }
console.log(jsonl.stringify([{ id: 1 }])); // {"id":1}

// json-schema：从模型文本抽 JSON，再按 schema 校验
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
// { ok: true, value: { id: 42 }, errors: [] } —— coerce 纠正明显类型偏差

// jinja2
console.log(new Jinja2().render("Hi {{ name }}", { name: "aike" })); // Hi aike

// html
console.log(html2markdown("<h1>Hello</h1>")); // # Hello

// http：服务一次请求后关闭
const server = new HttpServer();
server.get("/hi", (req, res) => res.end("hello"));
const handle = server.listen({ port: 8787 });
console.log(await (await fetch("http://127.0.0.1:8787/hi")).text()); // hello
handle.shutdown();
await handle.exit;
````

### 调用 LLM

```ts
import { LlmClient } from "ai";

const llm = new LlmClient({
  apiKey: "sk-xxxx",
  baseUrl: "https://api.openai.com/v1",
  model: "gpt-4o-mini",
});

for await (const chunk of llm.chat({
  messages: [{ role: "user", content: "用一句话说明什么是 JavaScript 运行时" }],
})) {
  process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
}
```

### 本机推理

```ts
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
      content: "用一句话说明本地推理的意义",
    },
  ],
  { temperature: 0.7, thinking: false },
);
console.log(reply.text, reply.usage);

engine.dispose();
```

### 操控浏览器

```ts
import { Browser } from "browser";

const browser = await Browser.launch({ headless: true });
const page = await browser.newPage();
await page.navigate("https://example.com");
await page.waitForSelector("h1", 5000);

console.log(await page.evaluate("document.title"));
console.log(await page.evaluate("document.body.innerText"));

await browser.close();
```

### 使用数据库

模块覆盖 SQL 数据库、键值库和向量库。

```ts
import { Database } from "sqlite";

const db = new Database(":memory:");
await db.execute("create table t (id integer primary key, name text)");
await db.execute("insert into t (name) values (?)", "aike");
for await (const row of db.query("select * from t")) console.log(row); // { 'id': 1, 'name': 'aike' }
db.close();
```

### 编写 MCP 服务器

用 aike 写 MCP 服务器：工具只描述一次——名字、说明、参数表、handler——`args` DSL 自动编译成 JSON Schema。

#### stdio 传输

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

用 `aike -m mcp-server.ts` 运行，即以 stdio 说 MCP：客户端列出工具，inputSchema 来自你的 `args`，调用进你的 handler。

#### HTTP 传输

`HttpMcp` 不自运行，挂在 `HttpServer` 上：

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

之后客户端访问 `http://localhost:8787/mcp`（Streamable HTTP）。

### 更多模块

[TBD: 模块索引]

### 标准模块不够用时

发行包模块覆盖常见需求。缺你想要的，不必等：用 **zapi** 把喜欢的库包成扩展模块，照常从 JS `import`。

#### 用 zapi 编写原生模块

**zapi** 是 aike 的原生扩展接口。扩展就是一个动态库。写普通 Rust，几个宏生成导出表。类、异步、多线程、GC 追踪都有现成写法。动态库改名为 `.aike`，放进工作目录、标准模块目录或 `AIKE_LIB_PATH` 目录，即可 `import`。

详细写法见 zapi 仓库的 `skills/SKILL.md`：https://github.com/nupha/zuqe-zapi——让你的 agent 去读。

#### 模块搜索路径

只有裸名字（`sqlite`、`vendor/lib`）走搜索路径。相对路径（`./x`、`../x`）、绝对路径、URL 一律不参与搜索。

目录按顺序查，先命中先用：

1. **发行包模块目录** `<二进制真实路径>/../../lib/aike/modules`。运行时先算出自己的真实路径（跟随符号链接），再向上两级拼出这个目录。它代表当前二进制自带的模块集合，项目里的同名文件覆盖不了它。目录不存在则跳过。
2. **引用者所在目录**：写 `import` 的那个文件所在的目录。
3. **当前工作目录**（CWD）。
4. **`$AIKE_LIB_PATH`**：只有一个目录；未设置或为空则跳过。

每个目录内部按下面顺序找文件：

- `<目录>/<名字>` 是目录：只在里面找 `index.js`、`index.ts`、`index.aike`，三者皆无则跳到下一个搜索目录。
- 不是目录：名字自带扩展名（如 `notes.aike`）只认这个文件；不带扩展名则依次试 `名字.js`、`名字.ts`、`名字.aike`。

命中 `.aike` 按 zapi 扩展加载，其余按源码加载。`.js` 读不到时回退 `.ts`，`.mjs` 回退 `.mts`，`.cjs` 回退 `.cts`。`require()` 与 `module.paths` 用同一份目录列表。

推荐布局：**标准模块留在标准路径，自己的扩展统一放进 `AIKE_LIB_PATH` 指向的目录。**

```sh
AIKE_LIB_PATH=$HOME/.aike/mods aike app.ts   # app.ts 里 import { x } from "myext"
```

两个来源互不干扰：升级发行包不碰你的扩展，你的扩展也不用动二进制旁边的目录。

两点注意。`AIKE_LIB_PATH` 只接受一个目录，不解析冒号分隔的列表。它的优先级最低，项目目录里的同名文件会先命中——想让自建扩展成为默认实现，就别在脚本旁边留同名文件。

## 与 Node、Bun、Deno 的区别

**Node / Bun / Deno 是通用运行时。aike 是面向 AI 应用的专用运行时。**

|                    | Node / Bun / Deno                                              | aike                                                                |
| ------------------ | -------------------------------------------------------------- | ------------------------------------------------------------------- |
| 定位               | 通用 JS 运行时，任何代码均可运行                               | 只面向"编写 AI 应用"一个目标                                        |
| AI 基础设施        | 自行安装依赖                                                   | 发行包内置                                                          |
| 规范取向           | 承担历史兼容负担                                               | 按 ECMA / WHATWG 设计 API，不做历史兼容                             |
| 引擎               | 通用大引擎，面向大型服务                                       | 嵌入式轻量引擎 `zuqe`（一个 crate），面向嵌入与轻负载               |
| 外部模块           | npm / JSR 生态                                                 | 发行包模块加外部模块引用。多数 npm 包不能直接用，依赖 native 的更难 |
| 一进程多运行时     | 依赖 worker / 子进程 / isolate，各有额外开销                   | 同一线程开多个隔离子运行时，宿主事件循环驱动它们                    |
| 运行模型生成的代码 | 依赖容器，或另选嵌入式引擎。Deno 以权限模型 + isolate 亦可实现 | 两级沙箱：子运行时按能力清单 + import 策略，子进程按 OS 策略        |

**aike** 能在同一线程上开多个隔离子运行时。每个堆独立，由宿主事件循环驱动。不起进程，低成本隔离几段任务。跨边界值必须可序列化；函数、promise、symbol 抛 `TypeError`。

宿主与沙箱共用一个二进制、一套语言能力。安全运行生成的 JS 不需要第二个嵌入式运行时。

## 状态

开发中，API 仍在变动。

## 许可

Apache-2.0。
