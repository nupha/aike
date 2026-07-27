declare module "http" {
  // ── HTTP client ──────────────────────────────────────────────────

  /**
   * Enhanced response returned by `HttpClient` requests.
   * Extends the WHATWG `Response` with aike-specific metadata.
   * Not constructable from JS.
   */
  export class HttpClientResponse extends Response {
    /** The Content-Type header value. */
    readonly contentType: string;
    /** The Content-Length, or null if absent. */
    readonly contentLength: number | null;
    /** Negotiated HTTP version ("1.1", "2", …), or null for synthetic responses. */
    readonly httpVersion: string | null;
    /**
     * Read the entire body as a string. For text/html, an optional mode
     * converts the body: "strip" removes HTML tags, "markdown" converts to Markdown.
     */
    text(convert?: "strip" | "markdown"): Promise<string>;
    /** Clone the response (fails if the body was already consumed). */
    clone(): HttpClientResponse;
    /** Close the response and release underlying resources. */
    close(): void;
  }

  /**
   * Reusable HTTP client with proxy, TLS, and cookie persistence.
   *
   * Network failures reject with an `Error` whose `code` property is one of:
   * `"timeout"` | `"connect"` | `"dns"` | `"tls"` | `"redirect"` | `"other"`.
   *
   * ```ts
   * const api = new HttpClient({ proxy: "http://proxy:8080", cookieJar: true });
   * await api.post("https://example.com/login", { body: creds });
   * const data = await api.get("https://example.com/data");
   * ```
   */
  export class HttpClient {
    constructor(options?: HttpClientOptions);

    /** Generic request. The HTTP method is read from `options.method`. */
    request(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;

    /** Convenience GET request. */
    get(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;

    /** Convenience POST request. */
    post(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;

    /** Convenience PUT request. */
    put(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;

    /** Convenience DELETE request. */
    delete(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;

    /** Convenience PATCH request. */
    patch(
      url: string,
      options?: HttpClientRequestOptions,
    ): Promise<HttpClientResponse>;
  }

  interface HttpClientOptions {
    /** HTTP/HTTPS proxy URL. Also reads `http_proxy` / `https_proxy` env vars. */
    proxy?: string;
    /** Comma-separated domains to bypass the proxy. Also reads `no_proxy` env var. */
    noProxy?: string;

    /** Skip HTTPS certificate validation. */
    rejectUnauthorized?: boolean;
    /** PEM-encoded CA certificate to add to the trust root. */
    ca?: string;
    /** PEM-encoded client certificate (mTLS). */
    cert?: string;
    /** PEM-encoded client private key (mTLS). */
    key?: string;

    /** Enable cookie jar, persisting cookies across requests. */
    cookieJar?: boolean;

    /** DNS overrides: `{ hostname: "IP" }` mapping. */
    resolve?: Record<string, string>;
    /** Force protocol version: `"http1"`, `"http2"`, or `"auto"` (default). */
    protocol?: "http1" | "http2" | "auto";

    /** Connection timeout in seconds. */
    connectTimeout?: number;
  }

  interface HttpClientRequestOptions {
    /** HTTP method (default: `"GET"` for `.get()`, `"POST"` for `.post()`, etc.) */
    method?: string;
    /** Request headers. */
    headers?: Record<string, string>;
    /** Request body as a UTF-8 string. */
    body?: string;
    /** Total request timeout in seconds (default: 30). */
    timeout?: number;
    /** AbortSignal — currently unsupported on HttpClient; use `fetch()` for abort. */
    signal?: AbortSignal;
  }
}
