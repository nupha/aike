# aike

> 自带 AI 零部件的 JS/TS 运行时。零部件是原生的，用 JavaScript / TypeScript来组装它们。

aike 取名自 “Ai + ke（壳）”：把 AI 应用要用的零部件装进一个壳里带走。所以也叫"脑壳"。

AI 应用的零部件 aike 自带，全部原生实现。零部件随发行包一起装好，不必 `npm install`，也不必编译构建。组装零部件的工作交给 JavasSript/TypeScript。文件写好，直接用 aike 运行。

- **零部件持续收集**：收集写 AI 应用需要的零部件，编译成标准模块，随 aike 发行包发布。截止当前，已收集 19 个：`ai` `mcp` `sqlite` `llama` `redb` `browser` `tree_sitter` 等，覆盖模型调用、本地推理、存储、浏览器自动化、语法解析。后续发展，新零部件将加入，过期的将下线。
- **同线程多子运行时**：一个进程开多个隔离子运行时。每个运行时拥有独立堆。不建进程，不建 worker。
- **沙箱强度可配**：宿主与沙箱共用一份运行时。差异只有一组能力开关。

实测数据（macOS arm64，0.1.0）：二进制 26 MB。`aike -e '1+1'` 热启动约 10 ms。空载常驻内存 6.4 MB。

## 例子

每段都能直接运行，命令形如 `aike -m hello.ts`。装好发行包即可，不需要 `npm install`，不需要构建。

**一、调云端模型，流式输出。** 一次对话边收边打印，不引入 SDK。

```ts
// hello.ts
import { LlmClient } from "ai";

const llm = new LlmClient({
  apiKey: "sk-xxxx",
  baseUrl: "https://api.openai.com/v1",
  model: "gpt-4o-mini",
});

for await (const chunk of llm.chat({
  messages: [{ role: "user", content: "用一句话说明 aike 是什么" }],
})) {
  process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
}
```

**二、本机推理，请求不出网。** 模型从磁盘加载，推理在本机完成。

```ts
// local.ts
import * as llama from "llama";

const model = new llama.Model("./models/Qwen3.5-4B-Q4_K_M.gguf");
const engine = llama.Engine.createEngine(model, {
  contextSize: 4096,
  maxCompletionTokens: 256,
});

const reply = engine.chat(
  [{ role: "user", content: "用一句话说明本地推理的意义" }],
  { temperature: 0.7, thinking: false },
);
console.log(reply.text, reply.usage);

engine.dispose();
```

**三、操控浏览器，取回渲染后的页面。** 启动 Chrome，打开页面，读 DOM 文本。

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

**四、写入本地数据库，读回记录。** 建表、写入、迭代查询，全程在本机。

```ts
// db.ts
import { Database } from "sqlite";

const db = new Database(":memory:");
await db.execute("create table t (id integer primary key, name text)");
await db.execute("insert into t (name) values (?)", "aike");
for await (const row of db.query("select * from t")) console.log(row); // { 'id': 1, 'name': 'aike' }
db.close();
```

另有两个存储模块：`postgres`（服务端数据库）与 `redb`（嵌入式键值存储）。此处不再演示。

**五、运行不可信代码，权限按清单授予。** 子运行时只拿到语言本身，没有文件，没有网络。

```ts
// sandbox.ts
const rt = Aike.createRuntime({ timeout: 1000 }); // 省略 global = 只给语言本身
console.log(await rt.run("typeof fetch")); // "undefined"
console.log(await rt.run("1 + 1")); // 2
rt.close();
```

以上涉及的 `llama`、`browser`、`sqlite` 即是标准模块，随 aike 发行包分发，见下节。

## 运行时（Runtime）与发行包（Distro）

**`bin/aike` 是运行时，`lib/aike/modules/` 是模块目录。** 用户拿到的目录结构如下：

```
dist/
├── bin/aike                    # 运行时本体，26 MB
└── lib/aike/modules/
    ├── ai.d.ts  mcp.d.ts  fs.d.ts  http/  process.d.ts  …   # 所有模块的类型声明
    ├── llama.aike       12.8 MB   # 扩展模块：原生共享库
    ├── speech.aike      31.8 MB
    ├── tree_sitter.aike 11.3 MB
    ├── browser.aike      8.2 MB
    ├── redb.aike         2.6 MB
    ├── sqlite.aike       2.5 MB
    ├── postgres.aike     2.4 MB
    └── audio.aike        1.4 MB
```

发行包里的模块统称标准模块。清单逐版变动：新零部件加进来，过期的清出去。

模块分两类。区别只有一处：编译进二进制，还是放进 `modules/` 按需加载。

|        | 内置模块                                                                        | 扩展模块                                                                    |
| ------ | ------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| 形态   | 编译进 `bin/aike`                                                               | 独立的 `.aike`（zapi 扩展，原生共享库）                                     |
| 清单   | `ai` `mcp` `http` `fs` `path` `os` `html` `yaml` `jsonl` `json-schema` `jinja2` | `llama` `sqlite` `postgres` `redb` `browser` `tree_sitter` `speech` `audio` |
| 体积   | 含在 26 MB 内                                                                   | 1.4 MB ~ 31.8 MB。可只带需要的几个                                          |
| 来源   | 有二进制即有                                                                    | 发行包提供。也可单独拷贝或自行构建                                          |
| 缺失时 | —                                                                               | `import "sqlite"` 抛 `could not load module '…/sqlite'`                     |

全局 API 不在模块之列，直接使用，不必 `import`：`fetch`、`URL`、`Blob`、流、`crypto.subtle`、`console`、`process`，以及 CommonJS 的 `require`。

**本仓库只构建运行时。** 扩展模块的源码在本仓库之外。

结论：仓库内 `cargo build` 产出的 `aike` 没有 `llama`、`sqlite`。要使用扩展模块，把发行包的 `lib/aike/modules/` 放到二进制旁边，或指向任意目录：

```sh
AIKE_LIB_PATH=/path/to/modules aike app.ts
```

确认当前安装带了哪些模块：

```sh
ls "$(dirname "$(command -v aike)")/../lib/aike/modules"
```

**标准模块都带 `.d.ts` 声明，与 `.aike` 同目录。** 类型检查、编辑器补全、语法提示，都从这些声明里读模块的用法。写代码的 agent 同样读它们，不必猜 API。声明跟着模块版本走，改 API 即改声明。

## 模块搜索路径

只有裸名字（`sqlite`、`vendor/lib`）走搜索路径。相对路径（`./x`、`../x`）、绝对路径、URL 一律不参与搜索。

搜索目录按优先级排列，前一级命中即止：

1. **发行包模块目录** `<二进制真实路径>/../../lib/aike/modules`。运行时先算出自己的真实路径（跟随符号链接），再向上两级拼出这个目录。它代表当前二进制自带的模块集合，项目里的同名文件覆盖不了它。目录不存在则跳过。
2. **引用者所在目录**：写 `import` 的那个文件所在的目录。
3. **当前工作目录**（CWD）。
4. **`$AIKE_LIB_PATH`**：只有一个目录；未设置或为空则跳过。

每个目录内部按下面的顺序找文件：

- 先看 `目录/名字` 是不是目录。是目录，就只在里面找 `index.js`、`index.ts`、`index.aike`，三者皆无则跳到下一级搜索目录。
- 不是目录时，名字自带扩展名（如 `notes.aike`）只认这个文件；不带扩展名则依次试 `名字.js`、`名字.ts`、`名字.aike`。

命中 `.aike` 走 zapi 扩展加载，其余按源码加载。`.js` 读不到时回退 `.ts`，`.mjs` 回退 `.mts`，`.cjs` 回退 `.cts`。`require()` 与 `module.paths` 用同一份目录列表。

推荐的摆放方式：**标准模块留在标准路径，自己写的扩展统一放 `AIKE_LIB_PATH` 指定的目录。**

```sh
AIKE_LIB_PATH=$HOME/.aike/mods aike app.ts   # app.ts 里 import { x } from "myext"
```

这样两个来源互不混淆：升级发行包不影响自建扩展，自建扩展也不用改二进制旁边的目录。

两点要留意。`AIKE_LIB_PATH` 只接受一个目录，不解析冒号分隔的列表。它的优先级最低，项目目录里的同名文件会先命中——想让自建扩展成为默认实现，就别在脚本旁边留同名文件。

## 标准模块不够用时

标准模块覆盖常见需求。如果还没采集到你需要的，不必等：用 zapi 把中意的库接成扩展模块，JS 侧照常 `import`。

zapi 是 aike 的原生扩展接口。扩展产物是一个动态库。写普通 Rust，几个宏生成导出表。类、异步、多线程、GC 追踪都有现成写法。动态库改名成 `.aike`，放进工作目录，标准模块目录，或 `AIKE_LIB_PATH` 指向的目录，即可 `import`。

详细写法，参考zapi仓库 https://github.com/nupha/zuqe-zapi 之 `skills/SKILL.md` 技能 —— 你让Agent去读也行。

## 使用场景

### 嵌入已有产品

桌面软件、IDE 插件、工业设备、离线终端。宿主程序调用 aike 执行脚本，脚本执行完毕即退出。aike 直接放进安装包，无需部署，也无需运维。

### 把一批材料转成一个确定结果

合同 → 字段表；工单 → 分类标签；数十 GB 日志 → 一份日报；一整个目录 → 一个可检索索引。

共同点：批量、可重复执行、结果可验收。

`json-schema` 把模型输出接入自有数据结构。`fromSimpleSchema` 写 schema，`extractJSON` 从模型输出取出 JSON，`validateSchema` / `matchesType` 校验。抽取与校验都是模块函数，不必逐项目重写。

### 本地数据的 AI 应用

内网、涉密、医疗、教育。`llama` 做本机推理，`sqlite` / `redb` 做本地存储，向量检索同样在本机完成。能留在本机的部分全部留在本机。不必为"用一下 AI"交出整份材料。

请求从调用方进程发出。调用方决定发出什么、发往何处、发出多少。

### 安全运行模型生成的代码

别处通常要一个容器，或者再嵌一个引擎。aike 只有一份运行时，差别在配置。

`Aike.createRuntime({ global: ["console", "crypto"] })` 按清单授予**全局**能力。省略 `global` 只给语言本身：没有文件，没有网络，没有外部联系。同一进程可同时开多个，堆彼此独立。

模块不在这份清单里。子运行时能否 `import` 一个模块，由 `importPolicy` 单独裁决：

```js
const rt = Aike.createRuntime({
  global: ["console"],
  importPolicy: (name) => name === "ai", // 模块给模块名，脚本文件给绝对路径
});
```

不传 `importPolicy`，子运行时**禁止一切 import**。传了，就每次 import 问一次，返回 `true` 才放行。

策略在创建者的上下文里**同步**执行：必须同步返回布尔值，不得创建或关闭运行时（重入会破坏运行时树，报 `TypeError`）；策略抛错则 import 失败，错误信息原样带出。

### 构建 Agent

流式输出、tool call、MCP 连接、参数 schema 校验、上下文拼接，都在运行时内。`StdioMcp` / `HttpMcp` 提供 MCP 服务端，`StdioMcpClient` / `HttpMcpClient` 提供客户端。

代码里只写调什么、何时停。SSE 流解析，运行时已经做好。

## 两级沙箱

需要限制不可信代码时，aike 提供两级，按用途挑。

**线程级**（`Aike.createRuntime`）：在同一进程里执行一段代码，决定这段代码能用哪些全局能力、用到哪些模块。清单为空，就只剩语言本身；模块由 `importPolicy` 裁决，缺省禁止一切 import。用来跑模型生成的表达式、处理不受信任的片段。

**进程级**（`Aike.spawn`）：执行一个子进程，资源边界交给操作系统——可写目录、可执行文件、出站连接。macOS、Windows、Linux 各有自己的后端。用来跑外部命令和第三方工具。

两级可叠加：在受限的子运行时里再启一个受限进程。

启动前问一次 `Aike.sandboxSupport()`。后端不可用时它会拒绝，不会静默降级。

## Aike 与 Node、Bun、Deno 的区别

**Node / Bun / Deno 是通用运行时。aike 是面向 AI 应用的专用运行时。**

|                        | Node / Bun / Deno                                              | aike                                                                           |
| ---------------------- | -------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| 定位                   | 通用 JS 运行时，任何代码均可运行                               | 只面向"编写 AI 应用"这一目标                                                   |
| AI 应用的基础设施      | 自行安装依赖                                                   | 发行包内置                                                                     |
| 规范取向               | 承担历史兼容负担                                               | 按 ECMA / WHATWG 设计 API，不做历史兼容                                        |
| 引擎                   | 通用大引擎，面向大型服务                                       | 嵌入式轻量引擎 `zuqe`（一个 crate），面向嵌入与轻负载                          |
| 外部模块               | npm / JSR 生态                                                 | 发行包模块 + 可引用外部模块。现成的 npm 包多数不能直接用，依赖 native 的更不能 |
| 一个进程里的多个运行时 | 依赖 worker / 子进程 / isolate，各有额外开销                   | 同一线程开多个隔离子运行时，宿主事件循环驱动它们                               |
| 运行模型生成的代码     | 依赖容器，或另选嵌入式引擎。Deno 以权限模型 + isolate 亦可实现 | 两级沙箱：子运行时按能力清单 + import 策略，子进程按 OS 策略                   |

三条底层差异：

**轻量多运行时。** 同一线程可开多个隔离子运行时。每个运行时拥有独立堆。宿主的事件循环驱动它们。隔离执行几段任务无需创建进程。跨边界值必须可序列化；函数、promise、symbol 抛 `TypeError`。

**沙箱强度可配。** 同一份运行时，全局能力按清单给，模块由 import 策略裁决。两样都不给，就只剩语言本身，与外界全无接触。

**同一份代码，两种身份。** 宿主与沙箱共用一个二进制、一套语言能力。安全执行生成的 JS 无需第二个嵌入式运行时。

## 开始

目前只有源码构建。预编译安装包尚未发布。

```sh
# 需要 Rust 工具链，以及轻量嵌入式引擎 zuqe 的 crate 源码
# Cargo.toml 中：zuqe = { path = "../../../devel/zhuque" }，暂未公开
cargo build --release
./target/release/aike -e 'console.log("hello aike")'
```

发行包内的 `bin/aike` 可直接使用，内含运行时与全部内置模块。

日常用法：

```sh
aike app.ts                 # 直接运行 TypeScript
aike -m app.mjs             # 强制按 ES module 运行
aike -e '1 + 1'             # 运行一段表达式
aike                        # REPL
aike app.ts --help extra    # 文件名之后的参数原样进 process.argv
aike -h                     # 运行时自身的帮助（含 --memory-limit 等）
```

## 状态

0.1.0，活跃开发中。API 仍在变动。

- 适用范围：编写 AI 应用。不提供聊天客户端，不复刻 npm 生态。
- 标准模块随版本增删，清单以发行包为准。本文件的模块数量按当前版本标注。
- API 声明位于 `types/`，模块与全局 API 各有一份 `.d.ts`，可直接当文档阅读。
- Web 设施按 WHATWG 规范实现，用 WPT 校对。仓库内抽样（`streams/writable-streams`，16 个文件 / 192 个子测试）通过 185 个，即 96.4%。明细见 `wpt-report.json`。
- 已知缺口：预编译二进制与安装脚本尚未发布；运行时与扩展模块分开分发，扩展模块源码不在本仓库；`LlmClient` 不做请求重试，重试需在调用方实现。

## 许可

Apache-2.0。
