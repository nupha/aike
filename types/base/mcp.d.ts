declare module "mcp" {
  /**
   * A single content part inside an MCP tool result.
   * The most common variant is `{ type: "text", text: "..." }`.
   */
  export type McpContentPart =
    | { type: "text"; text: string; [key: string]: unknown }
    | { type: "image"; data: string; mimeType?: string; [key: string]: unknown }
    | {
        type: "resource";
        resource: { uri: string; text: string; mimeType?: string };
        [key: string]: unknown;
      }
    | {
        type: "audio";
        data: string;
        mimeType?: string;
        [key: string]: unknown;
      };

  /**
   * MCP tool result returned by a handler.
   * A handler may return this object directly, OR return a plain value
   * (string / object / Error) which will be automatically wrapped into
   * `{ content: [{ type: "text", text: String(value) }] }`.
   */
  export interface McpToolCallResult {
    content: McpContentPart[];
    isError?: boolean;
    [key: string]: unknown;
  }

  /**
   * Payload of a streaming chunk emitted via {@link McpChunkSender}.
   *
   * A JSON value (recursive): string, number, boolean, null, arrays, and plain
   * objects. This matches what the server actually forwards — the chunk is
   * serialized to JSON and sent as the `notifications/tool-chunk` params, so
   * non-serializable values (functions, `undefined`, symbols, `Date`, class
   * instances, ...) are not expressible.
   */
  export type McpChunkPayload =
    | string
    | number
    | boolean
    | null
    | McpChunkPayload[]
    | { [key: string]: McpChunkPayload };

  /**
   * Live chunk emitter passed to a tool handler as its second argument.
   *
   * Calling `chunkSender(payload)` pushes an intermediate
   * `notifications/tool-chunk` SSE event to the MCP client while the tool is
   * still running. The handler's return value is delivered afterwards as the
   * normal `tools/call` result, so clients can render live progress / partial
   * results before the final answer.
   *
   * Semantics:
   * - Synchronous: returns `undefined` immediately. It is NOT a Promise — do
   *   not `await` it; `await` adds a useless microtask and waits for nothing.
   * - Fire-and-forget: emitting never blocks on the client.
   * - Best-effort: chunks are queued through a bounded channel (capacity 32);
   *   when the queue is full a chunk is silently dropped.
   * - The server always emits a leading `notifications/tool-chunk`
   *   `{ status: "started", tool }` event before your handler runs.
   *
   * @example
   * handler: async (args, chunkSender) => {
   *   chunkSender({ status: "searching", engine: "bing" });
   *   for (const page of pages) {
   *     chunkSender({ results: page.items, page: page.num });
   *   }
   *   return { content: [{ type: "text", text: "done" }] };
   * }
   */
  export type McpChunkSender = (payload: McpChunkPayload) => void;

  /**
   * Tool handler invoked on `tools/call`.
   *
   * The second parameter `chunkSender` streams intermediate SSE events to the
   * client (see {@link McpChunkSender}). It is always provided at runtime even
   * though handlers that do not use streaming may simply ignore it.
   *
   * Return the final result as a {@link McpToolCallResult} envelope (keeps
   * non-text content blocks) or a plain value (auto-wrapped into a text block).
   */
  type McpToolHandler = (
    args: Record<string, any>,
    chunkSender: McpChunkSender,
  ) => McpToolCallResult | Promise<McpToolCallResult>;

  /** JSON Schema describing a tool's input parameters. */
  export interface McpToolInputSchema {
    type: "object";
    properties?: Record<string, any>;
    required?: string[];
    [key: string]: any;
  }

  /**
   * Simplified argument definition for the DSL format.
   * Rust auto-converts this to standard JSON Schema `properties` entries.
   *
   * @example
   * // Basic string field
   * { desc: "File path.", required: true }
   * // Integer field
   * { desc: "Start line", type: "integer", default: 0 }
   * // Boolean field
   * { desc: "Show hidden files", type: "boolean", default: false }
   * // Array of strings
   * { desc: "File patterns", type: "array" }
   * // Enum
   * { desc: "Write mode", type: "string", enum: ["overwrite", "append"], default: "overwrite" }
   */
  export interface ToolArgDef {
    /** Human-readable description of this argument. */
    desc: string;
    /**
     * Value type. Defaults to `"string"` when omitted.
     * Allowed JSON-Schema types (per JSON Schema Validation 2020-12 §6.1.1):
     * - `"string"` — default
     * - `"integer"` — integer number (a subtype of `number`, zero fractional part)
     * - `"number"` — floating-point number (e.g. latitude/longitude)
     * - `"boolean"` — true/false
     * - `"array"` — array; use `items` to specify element type
     * - `"object"` — nested object
     * - `"null"` — JSON null
     */
    type?:
      | "string"
      | "integer"
      | "number"
      | "boolean"
      | "array"
      | "object"
      | "null";
    /** Mark as required. */
    required?: boolean;
    /** Enum string values. */
    enum?: string[];
    /** Default value when not provided. */
    default?: any;
    /**
     * Schema for array items. Only meaningful when `type="array"`.
     * Defaults to `{ type: "string" }` when omitted.
     *
     * @example
     * // Array of integers
     * { desc: "Port numbers", type: "array", items: { type: "integer" } }
     * // Array with element description
     * { desc: "Tags", type: "array", items: { type: "string", desc: "Tag name" } }
     */
    items?: {
      /** Element type */
      type:
        | "string"
        | "integer"
        | "number"
        | "boolean"
        | "array"
        | "object"
        | "null";
      /** Description of each element. */
      desc?: string;
      /** Enum values for elements. */
      enum?: string[];
    };
  }

  /**
   * MCP tool annotations (hints for clients; see the MCP spec `ToolAnnotations`).
   * Only the fields present here are advertised.
   */
  export interface McpToolAnnotations {
    /** If true, the tool does not modify its environment. */
    readOnlyHint?: boolean;
    /**
     * If true, the tool may perform destructive updates.
     * Meaningful only when `readOnlyHint` is false.
     */
    destructiveHint?: boolean;
    /**
     * If true, repeated calls with the same arguments have no additional effect.
     * Meaningful only when `readOnlyHint` is false.
     */
    idempotentHint?: boolean;
    /**
     * If true, the tool may interact with an open world of external entities.
     */
    openWorldHint?: boolean;
  }

  /**
   * Tool definition using object-keyed DSL format.
   *
   * Tool name is the key — no `name` field needed.
   * Arguments are defined via `args` DSL, auto-converted to JSON Schema.
   *
   * @example
   * const tools = {
   *   read_file: {
   *     desc: "Read file content.",
   *     handler: handleRead,
   *     annotations: { readOnlyHint: true, openWorldHint: false },
   *     args: {
   *       path:   { desc: "File path.", required: true },
   *       offset: { desc: "Start line", type: "integer", default: 0 },
   *     },
   *   },
   * };
   */
  export interface McpToolShortDef {
    /** Human-readable description of what the tool does. */
    desc: string;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
    /**
     * Argument DSL — Rust auto-converts this to standard JSON Schema inputSchema.
     */
    args: Record<string, ToolArgDef>;
    /** Optional MCP tool annotations (readOnlyHint / destructiveHint / ...). */
    annotations?: McpToolAnnotations;
  }

  /** An icon advertised by an MCP server (`Implementation.icons`). */
  export interface McpIcon {
    /** URI pointing to the icon resource. */
    src: string;
    /** Optional MIME type override when the server's type is missing/generic. */
    mimeType?: string;
    /** Size specs in `WxH` format (e.g. `"48x48"`) or `"any"`. */
    sizes?: string[];
    /** Optional theme this icon is designed for. */
    theme?: string;
  }

  /** what "initialize" method returns — mirrors `rmcp::ServerInfo` (== `InitializeResult`). */
  export interface McpInitializeResult {
    protocolVersion:
      | "2024-11-05"
      | "2025-03-26"
      | "2025-06-18"
      | "2025-11-25"
      | "2026-07-28";
    capabilities: {
      experimental?: Record<string, any>;
      extensions?: Record<string, any>;
      logging?: Record<string, any>;
      completions?: Record<string, any>;
      prompts?: { listChanged?: boolean };
      resources?: { subscribe?: boolean; listChanged?: boolean };
      tools?: { listChanged?: boolean };
      tasks?: {
        requests?: {
          sampling?: Record<string, any>;
          elicitation?: Record<string, any>;
          tools?: Record<string, any>;
        };
        list?: Record<string, any>;
        cancel?: Record<string, any>;
      };
    };
    serverInfo: {
      name: string;
      title?: string;
      version: string;
      description?: string;
      icons?: McpIcon[];
      websiteUrl?: string;
    };
    instructions?: string;
  }

  /** Metadata of tool definition */
  export interface McpToolMeta {
    name: string;
    description: string;
    inputSchema: Record<string, unknown>;
  }

  /**
   * Abstract base class shared by {@link StdioMcp} and {@link HttpMcp} (both wrap
   * the same server payload). Holds the prototype surface common to every MCP
   * server: identity getters, tool listing, and the dynamic tool helpers.
   *
   * Never constructed directly — use `StdioMcp.define` / `HttpMcp.define`.
   */
  abstract class BaseMcp {
    /** MCP `initialize` handshake result advertised by this server. */
    readonly initializeInfo: McpInitializeResult;

    /** Tool definitions */
    readonly toolsList: McpToolMeta[];

    /** Advertised server name. */
    readonly name: string;

    /** Advertised server version. */
    readonly version: string;

    /** Server instructions. */
    readonly instructions: string;

    /**
     * Dynamically add JS tools after construction. Accepts the same object-keyed
     * `tools` DSL as `define`.
     */
    addTool(tools: Record<string, McpToolShortDef>): this;

    /**
     * Hot-reload: REPLACE all JS tools (native tools are preserved). Accepts the
     * same object-keyed `tools` DSL as `define`.
     */
    updateTools(tools: Record<string, McpToolShortDef>): this;
  }

  /**
   * Common config shared by {@link StdioMcp.define} and {@link HttpMcp.define}.
   * `name` and `tools` are required in every variant; `version` /
   * `instructions` are optional server-identity fields.
   */
  interface BaseMcpConfig {
    /** Required: advertised server name. */
    name: string;
    /** Required: object-keyed tool DSL `{ name: { desc, handler, args } }`. */
    tools: Record<string, McpToolShortDef>;
    /** Advertised server version. */
    version?: string;
    description?: string;
    /** Server instructions shown to clients. */
    instructions?: string;
  }

  /**
   * Config for {@link StdioMcp.define}. Extends {@link BaseMcpConfig} with the
   * stdio spawn-time transport options.
   */
  export interface StdioMcpConfig extends BaseMcpConfig {
    /** Process command to spawn (defaults to the current aike binary). */
    command?: string;
    /** Environment passed to the spawned process. returning undefined skips the var. */
    env?: Record<
      string,
      string | ((ns: Record<string, string>) => string | undefined)
    >;
    /** CLI args passed to the spawned process. */
    args?: string[] | (() => string[]);
    /** Async hook run once before the server starts. */
    beforeRun?: () => Promise<void>;
    /** Extra source files bundled with the server. */
    extraFiles?: string[];
  }

  /**
   * MCP server over stdio transport (stdin/stdout).
   */
  export class StdioMcp extends BaseMcp {
    private constructor();

    /**
     * Create a stdio MCP server from a single config object.
     * @param config — see {@link StdioMcpConfig}. `name` and `tools` are required.
     */
    static define(config: StdioMcpConfig): StdioMcp;

    /** Process command used to spawn the server (empty string = default aike binary). */
    readonly command: string;

    /** Extra file paths bundled with the server. */
    readonly extraFiles: string[];

    /** Async hook run before the server starts, or `null`. */
    readonly beforeRun: (() => Promise<void>) | null;

    spawnArgs(ns: Record<string, string>): string[];
    spawnEnv(ns: Record<string, string>): Record<string, string>;

    /**
     * Launch this server: starts the stdio server.
     * Registers SIGINT/SIGTERM stop handlers, and force-exits once the server stops.
     */
    run(): Promise<void>;

    /**
     * Gracefully stop the MCP stdio server.
     *
     * Cancels the underlying rmcp RunningService, closes the stdio
     * transport, and resolves the Promise returned by start().
     *
     * Safe to call multiple times; no-op if already stopped.
     */
    static stop(): void;

    /**
     * Returns true if the server is currently running.
     */
    static isRunning(): boolean;
  }

  /**
   * MCP server over HTTP transport (Streamable HTTP).
   *
   * Unlike {@link StdioMcp}, an `HttpMcp` instance is NOT self-running — it is
   * mounted onto an `HttpServer` via `HttpServer.mcp(server)` and served over
   * Streamable HTTP. Tools can be hot-reloaded at runtime with
   * `server.updateTools(tools)` (replace all JS tools) or `server.addTool(tools)`
   * (append).
   */
  /**
   * Config for {@link HttpMcp.define}. Extends {@link BaseMcpConfig}; the HTTP
   * transport has no define-time options — the URL `mountPath` is NOT part of
   * this config, it is supplied when mounted via `HttpServer.mcp(path, server)`.
   */
  export interface HttpMcpConfig extends BaseMcpConfig {}

  export class HttpMcp extends BaseMcp {
    private constructor();

    /**
     * Create an HTTP MCP server from a single config object.
     * The mount path is supplied later via `HttpServer.mcp(mountPath, server)`.
     * @param config — see {@link HttpMcpConfig}. `name` and `tools` are required.
     */
    static define(config: HttpMcpConfig): HttpMcp;

    /** The URL mount path this server is served at. */
    readonly mountPath: string;
  }

  // ---------------------------------------------------------------------------
  // MCP clients
  // ---------------------------------------------------------------------------

  /**
   * Result of {@link StdioMcpClient.listTools} / {@link HttpMcpClient.listTools}
   * — the raw `tools/list` response.
   */
  export interface McpToolsListResult {
    tools: McpToolMeta[];
    nextCursor?: string;
  }

  /** Shared client surface: connect → list/call → close. */
  interface BaseMcpClient {
    /**
     * Spawn + initialize handshake (stdio) or open the streamable-HTTP
     * session (http). Resolves once the connection is ready; rejects on
     * handshake failure or timeout (`timeoutMs`).
     */
    connect(): Promise<void>;

    /**
     * Server identity captured during `connect()` — null before connecting.
     */
    readonly serverInfo: McpInitializeResult | null;

    /** List the tools the server advertises. */
    listTools(): Promise<McpToolsListResult>;

    /**
     * Call a tool. Returns the raw tool result envelope; a tool-level error
     * is reported via `isError: true` (not a rejected promise). Rejects on
     * transport/timeout errors or when the tool does not exist.
     */
    callTool(
      name: string,
      args?: Record<string, unknown>,
    ): Promise<McpToolCallResult>;

    /** Gracefully close the connection (stdio: EOF → SIGKILL ladder). Idempotent. */
    close(): Promise<void>;
  }

  /** Config for {@link StdioMcpClient}. */
  export interface StdioMcpClientConfig {
    /** Required: executable to spawn. */
    command: string;
    /** Command-line arguments. */
    args?: string[];
    /** Environment for the child; when provided it REPLACES the entire env. */
    env?: Record<string, string>;
    /** Working directory for the child. */
    cwd?: string;
    /**
     * Per-request timeout in milliseconds (applies to connect, listTools,
     * callTool). Must be a positive integer. Default: 30000.
     */
    timeoutMs?: number;
  }

  /**
   * MCP client over stdio transport. Spawns `<command> <args...>` and speaks
   * JSON-RPC over its stdin/stdout. One-shot lifecycle: connect() once,
   * then close(); create a new instance to reconnect.
   *
   * @example
   * const c = new StdioMcpClient({ command: "aike", args: ["-m", "-e", "..."] });
   * await c.connect();
   * const { tools } = await c.listTools();
   * const r = await c.callTool("add", { a: 1, b: 2 });
   * await c.close();
   */
  export class StdioMcpClient implements BaseMcpClient {
    constructor(config: StdioMcpClientConfig);
    connect(): Promise<void>;
    readonly serverInfo: McpInitializeResult | null;
    listTools(): Promise<McpToolsListResult>;
    callTool(name: string, args?: Record<string, unknown>): Promise<McpToolCallResult>;
    close(): Promise<void>;
  }

  /** Config for {@link HttpMcpClient}. */
  export interface HttpMcpClientConfig {
    /** Required: streamable-HTTP endpoint, e.g. "http://localhost:3456/mcp". */
    url: string;
    /** Extra HTTP headers sent with every request. */
    headers?: Record<string, string>;
    /** Per-request timeout in milliseconds. Must be a positive integer. Default: 30000. */
    timeoutMs?: number;
  }

  /**
   * MCP client over Streamable HTTP transport. Session management
   * (`Mcp-Session-Id`) and SSE responses are handled natively.
   */
  export class HttpMcpClient implements BaseMcpClient {
    constructor(config: HttpMcpClientConfig);
    connect(): Promise<void>;
    readonly serverInfo: McpInitializeResult | null;
    listTools(): Promise<McpToolsListResult>;
    callTool(name: string, args?: Record<string, unknown>): Promise<McpToolCallResult>;
    close(): Promise<void>;
  }
}
