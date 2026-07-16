declare module "mcp" {
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
    readonly description: string;

    /** Schema for tool definitions */
    get toolSchema(): McpToolMeta[] | undefined;
  }

  /** Metadata of mcp server */
  interface BaseMcpServerInfo {
    mode: "stdio" | "http";
    serverName: string;
    description: string;
    version: string;
    tools: McpToolMeta[];
  }

  export interface StdioMcpServerInfo extends BaseMcpServerInfo {
    mode: "stdio";
    shared?: boolean;
    extraFiles?: string[];
  }

  export interface HttpMcpServerInfo extends BaseMcpServerInfo {
    mode: "http";
    url: string;
    headers: Record<string, string>;
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

    readonly info: StdioMcpServerInfo;

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

    readonly info: HttpMcpServerInfo;

    /** http endpoint url. */
    readonly url: string;

    /** Request headers registered for the http transport. */
    readonly headers: Record<string, string>;

    /** Set the tool schema. http transport only. */
    set toolSchema(schema: McpToolMeta[]);
  }

  /** MCP tool call arguments — forwarded verbatim to the handler. */
  type McpToolHandler = (args: Record<string, any>) => any;

  /** Simplified argument definition for the DSL format. */
  export interface McpToolArgShort {
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
    args: Record<string, McpToolArgShort>;
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
}
