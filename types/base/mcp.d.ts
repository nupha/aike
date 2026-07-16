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

    /** Advertised server name. */
    readonly name: string;

    /** Transport mode of this server: `"stdio"` or `"http"`. */
    readonly mode: "stdio" | "http";

    /** Schema for tool definitions */
    get toolSchema(): McpToolMeta[] | undefined;

    /** Get the advertised server version. */
    version(): string;
    /** Set the advertised server version. Returns `this` for chaining. */
    version(v: string): this;

    /** Get the human-readable server description. */
    description(): string;
    /** Set the human-readable server description. Returns `this`. */
    description(v: string): this;

    /** Get all registered tool definitions keyed by name. */
    tool(): Record<string, McpToolShortDef>;
    /** Register a tool definition. Returns `this`. */
    tool(name: string, def: McpToolShortDef): this;

    /**
     * Server entry point (called by the bootstrap runner): awaits
     * `beforeRun` if set, starts the server, registers SIGINT/SIGTERM stop
     * handlers, and force-exits once the server stops.
     */
    run(): Promise<void>;
  }

  /**
   * stdio view of `McpServerDef` — narrows `mode` to `"stdio"` and adds the
   * stdio-only config: env vars, shared flag, CLI args, extra files, and the
   * before-run hook. (At runtime these are no-ops on an http server, so they
   * are intentionally absent from `HttpMcpDef`.)
   */
  export interface StdioMcpDef extends McpServerDef {
    readonly mode: "stdio";

    /** Get the currently registered env entries. */
    env(): Record<string, string | ((ctx?: McpCtx) => string | undefined)>;
    /**
     * Register one or more env vars at once.
     * @param vars — map of env name to either a static `string` or a resolver
     *   `(ctx) => string | undefined`. Keys whose resolver returns `undefined`
     *   (or a literal `undefined` value) are omitted. Returns `this` for chaining.
     */
    env(
      vars: Record<string, string | ((ctx?: McpCtx) => string | undefined)>,
    ): this;

    /** Get whether the server is shared. */
    shared(): boolean;
    /** Mark the server as shared (discoverable). Returns `this`. */
    shared(v: boolean): this;

    /** Get the configured CLI args, resolver, or `undefined` if unset. */
    args(): string[] | ((ctx: McpCtx) => string[]) | undefined;
    /** Set static CLI args, or a resolver `(ctx) => string[]`. Returns `this`. */
    args(v: string[]): this;
    args(fn: (ctx: McpCtx) => string[]): this;

    /** Register extra file paths bundled with the server. Returns `this`. */
    extraFiles(...paths: string[]): this;

    /** Register an async hook that runs before the server starts. Returns `this`. */
    beforeRun(fn: () => Promise<void>): this;
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
    get headers(): Record<string, string>;

    /** Set the tool schema. http transport only. */
    set toolSchema(schema: McpToolMeta[]);
  }

  /**
   * Factory returning a new `McpServerDef` for a stdio MCP server.
   * @param serverName — the advertised server name.
   */
  export function defineStdioMcp(serverName: string): StdioMcpDef;

  /**
   * Runtime context passed to `env`/`args` resolver callbacks.
   * Add new fields by extending this interface. The native
   * implementation currently passes an empty object, but the
   * signature mirrors the TS source for compatibility.
   */
  export interface McpCtx {
    "agent.name": string;
    "agent.workspace": string | null;
    [key: string]: unknown;
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

  /** Metadata of mcp server */
  export interface McpServerInfo {
    transport: "stdio" | "http";
    serverName: string;
    description: string;
    version: string;
    tools: McpToolMeta[];

    // stdio only
    shared?: boolean;
    /** Only present when at least one extra file is registered. */
    extraFiles?: string[];

    // http only
    url?: string;
  }
}
