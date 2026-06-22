// Type declarations for the "http" module
// import { HttpServer } from "http";

declare module "http" {
  /**
   * MCP tool call arguments — the `arguments` field from the JSON-RPC
   * `tools/call` params, forwarded verbatim to the handler.
   * Per MCP spec, this is typically an object matching the tool's `inputSchema`.
   */
  type McpToolArguments = Record<string, any>;

  /**
   * A single content part inside an MCP tool result.
   * The most common variant is `{ type: "text", text: "..." }`.
   */
  type McpContentPart =
    | { type: "text"; text: string }
    | { type: "image"; data: string; mimeType: string }
    | { type: "resource"; resource: any };

  /**
   * MCP tool result returned by a handler.
   * A handler may return this object directly, OR return a plain value
   * (string / object / Error) which will be automatically wrapped into
   * `{ content: [{ type: "text", text: String(value) }] }`.
   */
  interface McpToolResult {
    content: McpContentPart[];
    isError?: boolean;
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
    args: McpToolArguments,
  ) => McpToolResult | Promise<McpToolResult>;

  /** JSON Schema describing a tool's input parameters. */
  interface McpToolInputSchema {
    type: "object";
    properties?: Record<string, any>;
    required?: string[];
    [key: string]: any;
  }

  /** Definition of a single MCP tool in array format (backward compat). */
  interface McpToolDefinition {
    /** Unique tool name (required by MCP spec). */
    name: string;
    /** Human-readable description of what the tool does. */
    description?: string;
    /** JSON Schema for the tool's arguments. */
    inputSchema: McpToolInputSchema;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
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
  interface McpToolArgShort {
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
  interface McpToolShortDef {
    /** Human-readable description of what the tool does. */
    desc: string;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
    /**
     * Argument DSL — Rust auto-converts this to standard JSON Schema inputSchema.
     */
    args: Record<string, McpToolArgShort>;
  }

  interface McpOptions {
    /** Server name advertised in `initialize` response. */
    serverName: string;
    /** Server version advertised in `initialize` response. */
    version: string;
    /**
     * Tool definitions. Accepts either:
     * - `McpToolDefinition[]` — array of standard mcp tool definitions
     * - `McpToolMap` — object-keyed format with simplified DSL
     */
    tools: McpToolDefinition[] | Record<string, McpToolShortDef>;
  }

  /** Incoming HTTP request passed to route handlers. Cannot be constructed directly. */
  export class HttpServerRequest {
    /** HTTP method (GET, POST, PUT, etc.). */
    readonly method: string;
    /** Request path (e.g. "/api/users/123"). */
    readonly path: string;
    /** URL path parameters (e.g. { id: "123" } for route "/api/users/:id"). */
    readonly params: Record<string, string>;
    /** Parsed query string parameters. */
    readonly query: Record<string, string>;
    /** Request headers as key-value pairs (lowercased keys). */
    readonly headers: Record<string, string>;
    /** Parse the request body as JSON. Consumes body — can only be called once. */
    json(): any;
    /** Read the request body as a UTF-8 string. Consumes body. */
    text(): string;
    /** Read the request body as a Uint8Array. Consumes body. */
    bytes(): Uint8Array;
  }

  /** HTTP response builder passed to route handlers. Cannot be constructed directly. */
  export class HttpServerResponse {
    /** Set the HTTP status code. Returns this for chaining. */
    status(code: number): this;
    /** Set a single response header. Returns this for chaining. */
    setHeader(key: string, value: string): this;
    /** Set multiple response headers at once. Returns this for chaining. */
    setHeaders(headers: Record<string, string>): this;
    /** End the response with an optional body string. */
    end(body?: string): void;
    /** Send a JSON response. Automatically sets Content-Type to application/json. */
    json(data: any): void;
    /** Send a response with only a status code and no body. */
    sendStatus(code: number): void;
    /**
     * Send a redirect response. Sets `Location` header and ends the response.
     * @param code HTTP status code (default: 302). Must be an integer.
     * @param url Redirect target URL. If omitted, only the status is set.
     */
    redirect(code?: number, url?: string): void;
  }

  /** Route handler function. May be sync or async. */
  type RouteHandler = (
    req: HttpServerRequest,
    res: HttpServerResponse,
  ) => void | Promise<void>;

  interface ListenOptions {
    /** Port to listen on. Default: 8080 */
    port?: number;
    /** Host address to bind. Default: "0.0.0.0" */
    host?: string;
  }

  /** HTTP server with Express-like routing and JSON-RPC support. */
  export class HttpServer {
    constructor();

    /** Register a GET route. Supports Express-style `:param` path segments. */
    get(path: string, handler: RouteHandler): this;
    /** Register a POST route. */
    post(path: string, handler: RouteHandler): this;
    /** Register a PUT route. */
    put(path: string, handler: RouteHandler): this;
    /** Register a PATCH route. */
    patch(path: string, handler: RouteHandler): this;
    /** Register a DELETE route. */
    delete(path: string, handler: RouteHandler): this;
    /** Register a HEAD route. */
    head(path: string, handler: RouteHandler): this;
    /** Register an OPTIONS route. */
    options(path: string, handler: RouteHandler): this;

    /** Remove a previously registered route. Returns true if a route was removed. */
    removeRoute(method: string, path: string): boolean;

    /**
     * Register an MCP (Model Context Protocol) server at `mountPath`.
     * Rust auto-generates the four protocol methods: `initialize`,
     * `notifications/initialized`, `tools/list`, `tools/call`.
     * Must be called BEFORE `listen()`.
     */
    mcp(mountPath: string, options: McpOptions): this;

    /**
     * Hot-reload JS tools on an existing MCP endpoint (native tools preserved).
     * The new handler table takes effect on the very next request.
     */
    updateMcpTools(
      mountPath: string,
      tools: McpToolDefinition[] | Record<string, McpToolShortDef>,
    ): this;

    /** Mount OpenAI-compatible LLM inference routes. */
    openai(engine: any, mountPath: string): this;

    /** Whether the server is currently running. */
    readonly isRunning: boolean;

    /** Start listening for HTTP requests. Returns a handle with server lifecycle control. */
    listen(options?: ListenOptions): HttpServerHandle;

    /** Gracefully shut down the server. */
    shutdown(): Promise<void>;
  }

  /**
   * Handle returned by {@link HttpServer.listen} for controlling
   * the server lifecycle outside the HttpServer instance itself.
   */
  interface HttpServerHandle {
    /**
     * A Promise that resolves when the server fully stops.
     * Use this to wait for graceful shutdown after calling `shutdown()`.
     */
    readonly exit: Promise<void>;

    /**
     * Send a stop request to the server.
     * This returns immediately — use {@link exit} to wait for actual stop.
     */
    shutdown(): void;
  }

  /**
   * MCP server over stdio transport (stdin/stdout).
   *
   * stdin/stdout is a process-wide exclusive resource, so McpStdio
   * is a singleton — all methods are static. Only one instance can
   * be running at a time.
   *
   * Usage:
   *   const p = McpStdio.start({
   *     serverName: "my-tools",
   *     version: "1.0.0",
   *     tools: {
   *       echo: {
   *         desc: "Echo back the message",
   *         handler(args) { return { message: args.text }; },
   *         args: { text: { desc: "Text to echo", required: true } }
   *       }
   *     }
   *   });
   *   // server is running in background...
   *   McpStdio.stop(); // or wait for stdin EOF
   *   await p;
   */
  export class McpStdio {
    /** Cannot instantiate — use static methods. */
    private constructor();

    /**
     * Start the MCP stdio server.
     *
     * Returns a Promise that resolves when the server stops,
     * either because:
     * - stop() was called
     * - The MCP client disconnected (stdin EOF)
     * - An error occurred
     *
     * This is NON-BLOCKING — the server runs in the background.
     *
     * @throws TypeError if the server is already running.
     */
    static start(options: McpOptions): Promise<void>;

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
