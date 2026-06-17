// Type declarations for the "http" module
// import { HttpServer } from "http";

declare module "http" {
  /** JSON-RPC 2.0 params: object | array | omitted (undefined). */
  type RpcParams = Record<string, any> | any[] | undefined;

  /** JSON-RPC request handler: has id, must return result. */
  type RpcHandler = (params: RpcParams) => Promise<unknown>;

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
   * MCP tool handler function. Receives the `arguments` object from the
   * client's `tools/call` request. May be sync or async.
   *
   * Must return a value conforming to `McpToolResult` (the MCP protocol
   * requires `{ content: McpContentPart[], isError?: boolean }`).
   * Throwing an Error is caught by the framework and translated into a
   * JSON-RPC error response.
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

  /** Definition of a single MCP tool, passed to `server.mcp()` or `server.updateTools()`. */
  interface McpToolDefinition {
    /** Unique tool name (required by MCP spec). */
    name: string;
    /** Human-readable description of what the tool does. */
    description?: string;
    /** JSON Schema for the tool's arguments. */
    inputSchema?: McpToolInputSchema;
    /** Handler invoked on `tools/call`. */
    handler: McpToolHandler;
  }

  /** Options for `server.mcp()`. */
  interface McpServerOptions {
    /** Server name advertised in `initialize` response. */
    serverName: string;
    /** Server version advertised in `initialize` response. */
    version: string;
    /** Tool definitions to register at this mount path. */
    tools: McpToolDefinition[];
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
    /** Send a redirect response. Default status: 302. */
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

    /** Remove a previously registered route. */
    removeRoute(method: string, path: string): boolean;

    /** Register JSON-RPC method handlers at a mount path. */
    rpc(mountPath: string, handlers: { [name: string]: RpcHandler }): this;

    /**
     * Register an MCP (Model Context Protocol) server at `mountPath`.
     * Rust auto-generates the four protocol methods: `initialize`,
     * `notifications/initialized`, `tools/list`, `tools/call`.
     * Must be called BEFORE `listen()`.
     */
    mcp(mountPath: string, options: McpServerOptions): this;

    /**
     * Hot-reload JS tools on an existing MCP endpoint (native tools preserved).
     * The new handler table takes effect on the very next request.
     */
    updateMcpTools(mountPath: string, tools: McpToolDefinition[]): this;

    /** Mount OpenAI-compatible LLM inference routes. */
    openai(engine: any, mountPath: string): this;

    /** Whether the server is currently running. */
    readonly isRunning: boolean;

    /** Start listening for HTTP requests. */
    listen(options?: ListenOptions): void;

    /** Gracefully shut down the server. */
    shutdown(): Promise<void>;
  }
}
