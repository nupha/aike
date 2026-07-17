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
   * Handler for an MCP tool. Receives validated arguments and returns the
   * result object as plain business data — the Rust framework automatically
   * wraps it into the proper MCP `{ content: [{ type: "text", text: ... }] }`
   * response format.
   *
   * To report a tool execution error, simply throw an Error. The framework
   * catches it and converts it to the standard MCP error response
   * `{ content: [{ type: "text", text: "Error: ..." }], isError: true }`.
   *
   * @example
   * // Normal return — plain data, no manual MCP wrapping needed
   * async function handleRead(args) {
   *   const content = await fs.readFile(args.path, "utf-8");
   *   return { content, size: content.length };
   * }
   *
   * // Error reporting — throw an Error
   * async function handleStat(args) {
   *   if (!args.path) throw new Error("path is required");
   *   const st = await fs.stat(args.path);
   *   if (!st.isFile) throw new Error("not a file: " + args.path);
   *   return { type: "file", size: st.size };
   * }
   *
   * // Sync throw also works
   * function handleDelete(args) {
   *   if (!args.path) throw new Error("path is required");
   *   return { deleted: [args.path] };
   * }
   */
  type McpToolHandler = (
    args: Record<string, any>,
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
  export interface McpToolShortArg {
    /** Human-readable description of this argument. */
    desc: string;
    /**
     * Value type. Defaults to `"string"` when omitted.
     * - `"string"` — default
     * - `"integer"` — integer number
     * - `"boolean"` — true/false
     * - `"array"` — array; use `items` to specify element type
     */
    type?: "string" | "integer" | "boolean" | "array";
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
      type: "string" | "integer" | "boolean";
      /** Description of each element. */
      desc?: string;
      /** Enum values for elements. */
      enum?: string[];
    };
  }

  /**
   * Tool definition using object-keyed DSL format.
   *
   * Tool name is the key — no `name` field needed.
   * Arguments are defined via `args` DSL, auto-converted to JSON Schema.
   * For raw JSON Schema, use the array format (`McpToolDefinition[]`).
   *
   * @example
   * const tools = {
   *   read_file: {
   *     desc: "Read file content.",
   *     handler: handleRead,
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
    args: Record<string, McpToolShortArg>;
  }

  /** Definition of a single MCP tool in array format (backward compat). */
  export interface McpToolDefinition {
    /** Unique tool name (required by MCP spec). */
    name: string;
    /** Human-readable description of what the tool does. */
    description?: string;
    /** JSON Schema for the tool's arguments. */
    inputSchema: McpToolInputSchema;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
  }

  export interface McpOptions {
    /** Server name advertised in `initialize` response. */
    serverName: string;
    /** Server version advertised in `initialize` response. */
    version: string;
    /**
     * Tool definitions. Accepts either:
     * - `McpToolDefinition[]` — array of standard mcp tool definitions
     * - `Record<string, McpToolShortDef>` — object-keyed format with simplified DSL
     */
    tools: McpToolDefinition[] | Record<string, McpToolShortDef>;
  }

  /**
   * The native `McpServerDef` class — a single runtime class backing both
   * stdio and http transports. `StdioMcpDef` / `HttpMcpDef` (and the `McpDef`
   * union) are transport-narrowed *views*: this class declares only the
   * members common to every server; each transport adds its own config
   * methods via `extends`. Every member is declared exactly once — no
   * duplication, and the split is what makes `mode`-based narrowing real.
   */
  export class McpServerDef {
    private constructor(serverName: string);

    /** Transport mode of this server: `"stdio"` or `"http"`. */
    readonly mode: "stdio" | "http";

    /** Advertised server name. */
    readonly name: string;

    /** Advertised server version (read-only; set via the `defineStdioMcp` options). */
    readonly version: string;

    /** Human-readable server description (read-only; set via the `defineStdioMcp` options). */
    readonly instructions: string;

    /** Schema for tool definitions */
    get toolSchema(): McpToolMeta[] | undefined;
  }

  /** what "initialize" method returns */
  interface McpInitializeResult {
    protocolVersion: "2025-06-18";
    capabilities: {
      logging?: {};
      prompts?: { listChanged: boolean };
      resources?: {
        subscribe: boolean;
        listChanged: boolean;
      };
      tools?: { listChanged: boolean };
    };
    serverInfo: {
      name: string;
      version: string;
    };
    instructions?: string;
  }

  /**
   * stdio view of `McpServerDef` — narrows `mode` to `"stdio"` and adds the
   * stdio-only config: env vars, shared flag, CLI args, extra files, and the
   * before-run hook. (At runtime these are no-ops on an http server, so they
   * are intentionally absent from `HttpMcpDef`.)
   */
  export interface StdioMcpDef extends McpServerDef {
    readonly mode: "stdio";

    /** Whether the server is shared (read-only; set via the `defineStdioMcp` options). */
    readonly shared: boolean;

    /** Extra file paths bundled with the server (read-only; set via options). */
    readonly extraFiles: string[];

    /** Register a tool definition. Returns `this`. */
    tool(name: string, def: McpToolShortDef): this;

    /** Async hook run before the server starts, or `undefined` (read-only; set via options). */
    readonly beforeRun: (() => Promise<void>) | null;

    /**
     * Server entry point (called by the bootstrap runner): awaits
     * `beforeRun` if set, starts the server, registers SIGINT/SIGTERM stop
     * handlers, and force-exits once the server stops.
     */
    run(): Promise<void>;
  }

  /**
   * http view of `McpServerDef` — narrows `mode` to `"http"` and adds the
   * http-only config: request headers. (At runtime `env`/`shared`/`args`/…
   * are no-ops on an http server, so they are intentionally absent here.)
   */
  export interface HttpMcpDef extends McpServerDef {
    readonly mode: "http";

    /** http endpoint url. */
    readonly url: string;

    /** Request headers registered for the http transport. */
    readonly headers: Record<string, string>;

    /** Set the tool schema. http transport only. */
    set toolSchema(schema: McpToolMeta[]);
  }

  /** Simplified argument definition for the DSL format. */
  export interface McpToolShortArg {
    /** Human-readable description of this argument. */
    desc: string;
    /**
     * Value type. Defaults to `"string"` when omitted.
     * - `"string"` — default
     * - `"integer"` — integer number
     * - `"boolean"` — true/false
     * - `"array"` — array; use `items` to specify element type
     */
    type?: "string" | "integer" | "boolean" | "array";
    /** Mark as required. */
    required?: boolean;
    /** Enum string values. */
    enum?: string[];
    /** Default value when not provided. */
    default?: any;
    /**
     * Schema for array items. Only meaningful when `type="array"`.
     * @example { type: "integer" } | { type: "string", desc: "Tag name" }
     */
    items?: {
      type: "string" | "integer" | "boolean";
      desc?: string;
      enum?: string[];
    };
  }

  /** Simplified tool definition consumed by `MCPServerDef.tool`. */
  export interface McpToolShortDef {
    /** Human-readable description of what the tool does. */
    desc: string;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
    /**
     * Argument DSL — Rust auto-converts this to a standard JSON Schema
     * `inputSchema` (type/enum/default/required).
     */
    args: Record<string, McpToolShortArg>;
  }

  /** Metadata of tool defination */
  export interface McpToolMeta {
    name: string;
    description: string;
    inputSchema: Record<string, unknown>;
  }

  export type McpCtx = {
    MCP_AGENT_NAME: string;
    MCP_AGENT_WORKSPACE: string;
    [key: string]: string;
  };

  /**
   * Factory returning a new `McpServerDef` for a stdio MCP server.
   * @param serverName — the advertised server name.
   */
  export function defineStdioMcp(
    serverName: string,
    options?: {
      description?: string;
      version?: string;
      env?: Record<string, string | (() => string | undefined)>;
      args?: string[] | (() => string[]);
      beforeRun?: () => Promise<void>;
      shared?: boolean;
      extraFiles?: string[];
    },
  ): StdioMcpDef;

  /**
   * MCP server over stdio transport (stdin/stdout).
   */
  export class StdioMcp {
    private constructor();

    /**
     * Create a stdio MCP server.
     * @param serverName — the advertised server name.
     * @param tools — required object-keyed tool DSL
     *   `{ name: { desc, handler, args } }`.
     * @param options — optional config (everything except `tools`):
     *   version / instructions / env / args / beforeRun / shared / extraFiles.
     */
    static define(
      serverName: string,
      tools: Record<string, McpToolShortDef>,
      options?: {
        version?: string;
        instructions?: string;
        env?: Record<string, string | (() => string | undefined)>;
        args?: string[] | (() => string[]);
        beforeRun?: () => Promise<void>;
        shared?: boolean;
        extraFiles?: string[];
      },
    ): StdioMcp;

    /** Advertised server name. */
    readonly name: string;

    /** Advertised server version. */
    readonly version: string;

    /** Human-readable server description. */
    readonly instructions: string;

    /** Tool definitions */
    readonly toolSchema: McpToolMeta[];

    /** Whether the server is shared. */
    readonly shared: boolean;

    /** Extra file paths bundled with the server. */
    readonly extraFiles: string[];

    /** Async hook run before the server starts, or `null`. */
    readonly beforeRun: (() => Promise<void>) | null;

    /**
     * Launch this server: awaits `beforeRun` if set, starts the stdio
     * server, registers SIGINT/SIGTERM stop handlers, and force-exits once
     * the server stops.
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
}
