// Type declarations for the "mcp" module
// import { MCPServerDef, defineMCPStdio } from "mcp";

declare module "mcp" {
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

  /** Metadata returned by `MCPServerDef.info()`. */
  export interface McpToolMeta {
    name: string;
    description: string;
    inputSchema: Record<string, unknown>;
  }

  /** Server metadata object returned by `info()`. */
  export interface McpServerInfo {
    serverName: string;
    description: string;
    version: string;
    shared: boolean;
    tools: McpToolMeta[];
    extraFiles?: string[];
  }

  /**
   * Runtime env resolver. Receives the MCP context and returns a `string`,
   * or `undefined` to omit that env var.
   */
  export type EnvValFunc = (ctx?: McpCtx) => string | undefined;

  /**
   * Declarative MCP stdio server definition with a chainable Builder API.
   *
   * @example
   *   export default defineMCPStdio("my-server")
   *     .version("1.0.0")
   *     .description("My tools")
   *     .tool("greet", { desc: "...", handler: async args => ({...}), args: { ... } })
   *     .tool("calc", { ... });
   *
   * Use `info()` for metadata (no side effects) or `run()` in the bootstrap
   * runner to start the stdio server.
   */
  export class MCPServerDef {
    /**
     * Return server definition metadata
     */
    readonly info: McpServerInfo;

    /** Set the advertised server version. Returns `this` for chaining. */
    version(v: string): this;
    /** Set the human-readable server description. Returns `this`. */
    description(v: string): this;
    /** Mark the server as shared (discoverable). Returns `this`. */
    shared(v: boolean): this;

    /** Set static CLI args, or a resolver `(ctx) => string[]`. Returns `this`. */
    args(v: string[]): this;
    args(fn: (ctx: McpCtx) => string[]): this;

    /**
     * Register one or more env vars at once.
     * @param vars — map of env name to either a static `string` or a resolver
     *   `EnvValFunc` `(ctx) => string | undefined`. Keys whose resolver
     *   returns `undefined` (or a literal `undefined` value) are omitted.
     *   Returns `this` for chaining.
     */
    env(vars: Record<string, string | EnvValFunc>): this;

    /** Register a tool definition. Returns `this`. */
    tool(name: string, def: McpToolShortDef): this;
    /** Register extra file paths bundled with the server. Returns `this`. */
    extraFiles(...paths: string[]): this;
    /** Register an async hook that runs before the server starts. Returns `this`. */
    beforeRun(fn: () => Promise<void>): this;

    /**
     * Server entry point (called by the bootstrap runner): awaits
     * `beforeRun` if set, starts the stdio server via native `McpStdio`,
     * registers SIGINT/SIGTERM stop handlers, and force-exits once the
     * server stops.
     */
    run(): Promise<void>;
  }

  /**
   * Factory returning a new `MCPServerDef` for a stdio MCP server.
   * @param serverName — the advertised server name.
   */
  export function defineMCPStdio(serverName: string): MCPServerDef;
}
