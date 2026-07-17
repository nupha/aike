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
    protocolVersion: "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25";
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
     * Dynamically add JS tools after construction. Accepts the same `tools` DSL
     * as `define` (object-keyed map or `McpToolDefinition[]` array).
     */
    addTool(tools: McpToolDefinition[] | Record<string, McpToolShortDef>): this;

    /**
     * Hot-reload: REPLACE all JS tools (native tools are preserved). Accepts the
     * same `tools` DSL as `define`.
     */
    updateTools(
      tools: McpToolDefinition[] | Record<string, McpToolShortDef>,
    ): this;
  }

  /**
   * MCP server over stdio transport (stdin/stdout).
   */
  export class StdioMcp extends BaseMcp {
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

  /**
   * MCP server over HTTP transport (Streamable HTTP).
   *
   * Unlike {@link StdioMcp}, an `HttpMcp` instance is NOT self-running — it is
   * mounted onto an `HttpServer` via `HttpServer.mcp(server)` and served over
   * Streamable HTTP. Tools can be hot-reloaded at runtime with
   * `server.updateTools(tools)` (replace all JS tools) or `server.addTool(tools)`
   * (append).
   */
  export class HttpMcp extends BaseMcp {
    private constructor();

    /**
     * Create an HTTP MCP server.
     *
     * The mount path is NOT set here — it is supplied when the server is
     * mounted via `HttpServer.mcp(mountPath, server)`.
     * @param name — the advertised server name (required, positional).
     * @param tools — required tool DSL, either an object-keyed map
     *   `{ name: { desc, handler, args } }` or a `McpToolDefinition[]` array.
     * @param options — optional config: `version` / `instructions`.
     */
    static define(
      name: string,
      tools: McpToolDefinition[] | Record<string, McpToolShortDef>,
      options?: {
        version?: string;
        instructions?: string;
      },
    ): HttpMcp;

    /** The URL mount path this server is served at. */
    readonly mountPath: string;
  }
}
