// Type declarations for the "http" module
// import { HttpServer } from "http";

declare module "http" {
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
    rpc(
      mountPath: string,
      handlers: Record<string, (params: any) => Promise<any>>,
    ): this;

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
