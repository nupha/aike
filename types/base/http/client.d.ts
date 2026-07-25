declare module "http" {
  // ── HTTP client ──────────────────────────────────────────────────

  /**
   * Reusable HTTP client with proxy, TLS, and cookie persistence.
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
    request(url: string, options?: HttpClientRequestOptions): Promise<Response>;

    /** Convenience GET request. */
    get(url: string, options?: HttpClientRequestOptions): Promise<Response>;

    /** Convenience POST request. */
    post(url: string, options?: HttpClientRequestOptions): Promise<Response>;

    /** Convenience PUT request. */
    put(url: string, options?: HttpClientRequestOptions): Promise<Response>;

    /** Convenience DELETE request. */
    delete(url: string, options?: HttpClientRequestOptions): Promise<Response>;

    /** Convenience PATCH request. */
    patch(url: string, options?: HttpClientRequestOptions): Promise<Response>;
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
    /** Redirect behaviour: `"follow"` (default), `"error"`, or `"manual"`. */
    redirect?: "follow" | "error" | "manual";
    /** AbortSignal — currently unsupported on HttpClient; use `fetch()` for abort. */
    signal?: AbortSignal;
  }
}
