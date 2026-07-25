declare module "http" {
  import { HttpMcp } from "mcp";

  // ── HTTP server ──────────────────────────────────────────────────

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
    /**
     * Switch this response into SSE (Server-Sent Events) streaming mode and
     * return an independent `HttpResponseSse` object. The response is opened
     * immediately (with `text/event-stream` + no-cache + `X-Accel-Buffering: no`
     * headers by default) and this `HttpServerResponse` is invalidated — calling
     * `json`/`setHeader`/`end`/`sse` on it afterwards throws.
     */
    sse(): HttpResponseSse;
  }

  /**
   * SSE streaming response returned by `HttpServerResponse.sse()`. Only exposes
   * write/flush/close — `json`/`setHeader`/`end` are intentionally absent.
   */
  export class HttpResponseSse {
    /** Push one SSE frame. Returns a Promise that resolves when the write drains. */
    write(data: string): void | Promise<void>;
    /** Force-flush buffered frames (no-op here; frames are sent immediately). */
    flush(): void;
    /** End the stream. Idempotent. */
    close(): void;
  }

  /**
   * WebSocket connection handle delivered to `server.ws(path, handler)`.
   * A real bidirectional long connection with its own lifecycle.
   */
  export class WebSocketStream {
    /** Unique stream id, used by `server.stream_registry` dispatch. */
    readonly id: number;
    /** Connection state: 0 connecting / 1 open / 2 closing / 3 closed. */
    readonly readyState: number;
    /** Remote client address (Bun `remoteAddress`). */
    readonly remoteAddress: string;

    /** Async-iterable: `for await (const msg of stream) { ... }` (pull mode). */
    [Symbol.asyncIterator](): AsyncIterator<string>;
    /** Write a frame to the client. Returns a Promise when the write must await drain. */
    write(data: string): void | Promise<void>;
    /** Send a ping frame. */
    ping(): void;
    /** Close the connection. */
    close(): void;

    /** Push mode (alternative to `for await`): receive messages via callback. */
    onMessage(cb: (data: string) => void): void;
    onOpen(cb: () => void): void;
    onClose(cb: () => void): void;
    onError(cb: (err: any) => void): void;
    /** Fired when the write backlog drains (Bun `drain`). */
    onDrain(cb: () => void): void;
    /** Subscribe to a broadcast topic (Bun `subscribe`). */
    subscribe(topic: string): void;
    /** Unsubscribe from a broadcast topic. */
    unsubscribe(topic: string): void;
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

    /** Whether the server is currently running. */
    readonly isRunning: boolean;

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

    /**
     * Register a WebSocket route. The `handler` receives a `WebSocketStream`
     * handle; read via `for await (const msg of stream)` and write via
     * `stream.write(...)`.
     */
    ws(
      path: string,
      handler: (stream: WebSocketStream) => void | Promise<void>,
    ): this;

    /**
     * Register an HTTP-transport MCP (Model Context Protocol) server.
     *
     * Pass the mount path and an `HttpMcp` instance created via
     * `HttpMcp.define(config)`. The `mountPath` (must start with `/`)
     * is assigned here — it is the URL the endpoint is served at. Rust
     * auto-generates the four protocol methods: `initialize`,
     * `notifications/initialized`, `tools/list`, `tools/call`.
     * Must be called BEFORE `listen()`.
     */
    mcp(mountPath: string, server: HttpMcp): this;

    /**
     * Serve a directory of static files at `mountPath`. `GET /` falls back to
     * `index.html`. Returns this for chaining.
     */
    staticFiles(mountPath: string, dir: string): this;

    /**
     * Broadcast `data` to every WebSocket that subscribed to `topic`
     * (Bun `server.publish` semantics).
     */
    publish(topic: string, data: string): void;

    /** Remove a previously registered route. Returns true if a route was removed. */
    removeRoute(method: string, path: string): boolean;

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
}
