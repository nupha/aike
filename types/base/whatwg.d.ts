// Type declarations for Aike JS runtime — Global APIs
// Generated from llama-js/src/ Rust source code.

// ---------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------
declare namespace console {
  function log(...args: any[]): void;
  function debug(...args: any[]): void;
  function info(...args: any[]): void;
  function warn(...args: any[]): void;
  function error(...args: any[]): void;
  function assert(condition?: boolean, ...args: any[]): void;
  function trace(...args: any[]): void;
  function dir(obj: any, ...args: any[]): void;
  function dirxml(obj: any, ...args: any[]): void;
  function count(label?: string): void;
  function countReset(label?: string): void;
  function time(label?: string): void;
  function timeLog(label?: string, ...args: any[]): void;
  function timeEnd(label?: string): void;
  function group(...args: any[]): void;
  function groupCollapsed(...args: any[]): void;
  function groupEnd(): void;
  function clear(): void;
  function table(data: any, ...args: any[]): void;
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/** Schedule a callback after `delayMs` milliseconds. Returns a timer ID. */
declare function setTimeout(callback: () => void, delayMs?: number): number;

/** Schedule a callback repeatedly every `intervalMs` milliseconds. Returns a timer ID. */
declare function setInterval(callback: () => void, intervalMs?: number): number;

/** Cancel a timer created by setTimeout. */
declare function clearTimeout(timerId: number): void;

/** Cancel a timer created by setInterval. */
declare function clearInterval(timerId: number): void;

// ---------------------------------------------------------------------------
// request()  —  global HTTP client function
// ---------------------------------------------------------------------------

interface RequestOptions {
  /** HTTP method (GET, POST, PUT, DELETE, etc.). Default: "GET" */
  method?: string;
  /** Request headers as key-value pairs. */
  headers?: Record<string, string>;
  /** Request body string. */
  body?: string;
  /** Request timeout in seconds. Default: 30 */
  timeout?: number;
}

/** HTTP response returned by the global `request()` function. */
declare class HttpResponse {
  /** HTTP status code (e.g. 200, 404). */
  readonly status: number;
  /** Whether the status is in the 2xx range. */
  readonly ok: boolean;
  /** HTTP status text (e.g. "OK", "Not Found"). */
  readonly statusText: string;
  /** Response Content-Type header value. */
  readonly contentType: string;
  /** Response Content-Length header value, or null if absent. */
  readonly contentLength: number | null;
  /** Response headers as key-value pairs. */
  readonly headers: Record<string, string>;

  /** Async iterator over response body chunks (Uint8Array). */
  get body(): AsyncIterableIterator<Uint8Array>;

  /** Read the entire body as a Uint8Array. */
  bytes(): Promise<Uint8Array>;
  /**
   * Read the entire body as a string.
   * @param convert - Optional conversion for HTML responses: "strip" removes HTML tags, "markdown" converts HTML to Markdown.
   */
  text(convert?: "strip" | "markdown"): Promise<string>;
  /** Read and parse the entire body as JSON. */
  json(): Promise<any>;
  /** Close the response and release underlying resources. */
  close(): void;
}

/**
 * Send an HTTP request.
 * @param url - The URL to request.
 * @param options - Optional request configuration.
 */
declare function request(
  url: string,
  options?: RequestOptions,
): Promise<HttpResponse>;

declare class EventEmitter {
  addListener(event: string, listener: (...args: unknown[]) => void): this;
  on(event: string, listener: (...args: unknown[]) => void): this;
  once(event: string, listener: (...args: unknown[]) => void): this;
  removeListener(event: string, listener: (...args: unknown[]) => void): this;
  off(event: string, listener: (...args: unknown[]) => void): this;
  emit(event: string, ...args: unknown[]): boolean;
  removeAllListeners(event?: string): this;
  listeners(event: string): Function[];
  listenerCount(event: string): number;
  eventNames(): string[];
}

// ---------------------------------------------------------------------------
// TextDecoder
// ---------------------------------------------------------------------------

interface TextDecoderOptions {
  fatal?: boolean;
  ignoreBOM?: boolean;
}

interface TextDecodeOptions {
  stream?: boolean;
  fatal?: boolean;
}

/**
 * Decode text from binary data.
 * Supported encodings: utf-8, utf-16le, utf-16be, iso-8859-1 (latin1).
 */
declare class TextDecoder {
  constructor(label?: string, options?: TextDecoderOptions);
  /** The encoding name (normalized). */
  readonly encoding: string;
  /** Whether the decoder throws on invalid sequences. */
  readonly fatal: boolean;
  /** Whether the BOM is ignored. */
  readonly ignoreBOM: boolean;
  /**
   * Decode input bytes to a string.
   * @param input - Uint8Array or ArrayBuffer to decode.
   * @param options - Streaming and fatal options.
   */
  decode(input?: Uint8Array | ArrayBuffer, options?: TextDecodeOptions): string;
}

// ---------------------------------------------------------------------------
// TextEncoder
// ---------------------------------------------------------------------------

/**
 * Encode text as UTF-8 Uint8Array.
 * Encoding is always "utf-8" per spec.
 */
declare class TextEncoder {
  constructor();
  /** Always "utf-8". */
  readonly encoding: string;
  /** Encode a string into a Uint8Array. */
  encode(input?: string): Uint8Array;
  /**
   * Encode a string into a pre-allocated Uint8Array destination.
   * Returns the number of code units read and bytes written.
   */
  encodeInto(
    source: string,
    destination: Uint8Array,
  ): { read: number; written: number };
}

// ---------------------------------------------------------------------------
// URL
// ---------------------------------------------------------------------------

/**
 * Parse, construct, and manipulate URLs per the WHATWG URL Standard.
 */
declare class URL {
  constructor(url: string, base?: string | URL);
  /** Complete URL string. */
  href: string;
  /** Read-only origin string (e.g. "https://example.com:8080"). */
  readonly origin: string;
  /** Protocol with trailing colon (e.g. "https:"). */
  protocol: string;
  /** Username portion of the URL. */
  username: string;
  /** Password portion of the URL. */
  password: string;
  /** Host including port (e.g. "example.com:8080"). */
  host: string;
  /** Hostname without port. */
  hostname: string;
  /** Port number as string, or empty string if default. */
  port: string;
  /** Path segment (e.g. "/foo/bar"). */
  pathname: string;
  /** Query string including leading "?", or empty string. */
  search: string;
  /** Fragment including leading "#", or empty string. */
  hash: string;
  /** URLSearchParams object tied to this URL's query. */
  readonly searchParams: URLSearchParams;
  /** Returns the href. */
  toString(): string;
  /** Returns the href (for JSON serialization). */
  toJSON(): string;
}

// ---------------------------------------------------------------------------
// URLSearchParams
// ---------------------------------------------------------------------------

/**
 * Parse and manipulate URL query strings per the WHATWG URL Standard.
 */
declare class URLSearchParams {
  constructor(init?: string | Record<string, string> | URLSearchParams);
  /** Append a key-value pair. */
  append(name: string, value: string): void;
  /** Delete all entries with the given key. */
  delete(name: string): void;
  /** Get the first value for a key, or null if missing. */
  get(name: string): string | null;
  /** Get all values for a key. */
  getAll(name: string): string[];
  /** Check if a key exists. */
  has(name: string): boolean;
  /** Set a key to a single value (removes duplicates). */
  set(name: string, value: string): void;
  /** Sort all entries by key name. */
  sort(): void;
  /** Serialize to query string (without leading "?"). */
  toString(): string;
  /**
   * Iterate over entries.
   * @param callback - Called as (value, key, searchParams) per spec.
   */
  forEach(
    callback: (value: string, key: string, parent: URLSearchParams) => void,
  ): void;
}

// ---------------------------------------------------------------------------
// AbortSignal
// ---------------------------------------------------------------------------

/**
 * Represents a signal that can be used to abort asynchronous tasks.
 * Cannot be constructed directly — obtain via AbortController.signal
 * or AbortSignal.abort()/timeout() statics.
 */
declare class AbortSignal {
  /** Prevent direct construction. */
  private constructor();
  /** Whether the operation has been aborted. */
  readonly aborted: boolean;
  /** The abort reason, or undefined if not aborted. */
  readonly reason: any;
  /** Throws the abort reason if this signal has been aborted. */
  throwIfAborted(): void;
  /**
   * Returns an AbortSignal that is already aborted with the given reason.
   * @param reason - Optional abort reason.
   */
  static abort(reason?: any): AbortSignal;
  /**
   * Returns an AbortSignal that aborts after the given duration in ms.
   * @param delay - Timeout in milliseconds.
   */
  static timeout(delay: number): AbortSignal;
}

// ---------------------------------------------------------------------------
// AbortController
// ---------------------------------------------------------------------------

/**
 * Controller that creates an AbortSignal and can abort it on demand.
 */
declare class AbortController {
  constructor();
  /** The associated AbortSignal. */
  readonly signal: AbortSignal;
  /**
   * Abort the operation.
   * @param reason - Optional abort reason.
   */
  abort(reason?: any): void;
}

// ---------------------------------------------------------------------------
// WHATWG Fetch — Headers / Request / Response / fetch
// ---------------------------------------------------------------------------

/** A header name/value pair. */
type HeadersInit = Headers | Record<string, string> | Array<[string, string]>;

/** Request/response body: a string or a binary buffer. */
type BodyInit = string | ArrayBufferView | ArrayBuffer;

/** Fetch input: a `Request` object or a URL string. */
type RequestInfo = Request | string;

/**
 * WHATWG Fetch `Headers` class (per the Fetch Standard).
 *
 * Throws `TypeError` on an invalid header name (must be a token) or value
 * (no control characters). `set`/`append` strip leading/trailing whitespace
 * from the value.
 */
declare class Headers {
  /** Construct from name/value pairs, a record, or another Headers. */
  constructor(init?: HeadersInit);
  /** Append a value (comma-joined if the name already exists). */
  append(name: string, value: string): void;
  /** Delete all values for a name. */
  delete(name: string): void;
  /** Get the combined value for a name, or null if absent. */
  get(name: string): string | null;
  /** Check if a name exists. */
  has(name: string): boolean;
  /** Set a name to a single value (replacing any existing values). */
  set(name: string, value: string): void;
  /** Iterate as (value, key, headers). */
  forEach(
    callback: (value: string, key: string, parent: Headers) => void,
  ): void;
  /** Returns an array of header names. */
  keys(): string[];
  /** Returns an array of header values. */
  values(): string[];
  /** Returns an array of [name, value] pairs. */
  entries(): Array<[string, string]>;
  /** Default iterator yields [name, value] pairs. */
  [Symbol.iterator](): IterableIterator<[string, string]>;
}

/** Options for `fetch()` and `new Request()`. */
interface RequestInit {
  /** HTTP method. Defaults to "GET". */
  method?: string;
  /** Request headers. */
  headers?: HeadersInit;
  /** Request body (string or TypedArray). */
  body?: BodyInit;
  /** Redirect mode: "follow" | "error" | "manual". Default "follow". */
  redirect?: "follow" | "error" | "manual";
  /** AbortSignal to cancel the request. */
  signal?: AbortSignal;
  /** Aike extension: timeout in seconds (>0 overrides the 30s default). */
  timeout?: number;
  /** Request mode (structural only): "cors" | "no-cors" | "same-origin" | "navigate". */
  mode?: string;
  /** Credentials mode: "omit" | "same-origin" | "include". */
  credentials?: "omit" | "same-origin" | "include";
  /** Cache mode (structural only). */
  cache?: string;
  /** Referrer URL, or "about:client" for none. */
  referrer?: string;
  /** Referrer policy (structural only). */
  referrerPolicy?: string;
  /** Subresource integrity metadata (structural only). */
  integrity?: string;
  /** Keep-alive flag. */
  keepalive?: boolean;
}

/**
 * WHATWG Fetch `Request` class.
 */
declare class Request {
  constructor(input: RequestInfo, init?: RequestInit);
  /** HTTP method. */
  readonly method: string;
  /** Request URL. */
  readonly url: string;
  /** Request destination (always ""). */
  readonly destination: string;
  /** Referrer URL. */
  readonly referrer: string;
  /** Referrer policy. */
  readonly referrerPolicy: string;
  /** Request mode. */
  readonly mode: string;
  /** Credentials mode. */
  readonly credentials: string;
  /** Cache mode. */
  readonly cache: string;
  /** Redirect mode. */
  readonly redirect: "follow" | "error" | "manual";
  /** Integrity metadata. */
  readonly integrity: string;
  /** Keep-alive flag. */
  readonly keepalive: boolean;
  /** AbortSignal, or null. */
  readonly signal: AbortSignal | null;
  /** Request headers. */
  readonly headers: Headers;
  /** Whether the body has been consumed. */
  readonly bodyUsed: boolean;
  /** Async iterator over request body chunks, or null if none. */
  readonly body: AsyncIterableIterator<Uint8Array> | null;
  /** Read the body as bytes. */
  bytes(): Promise<Uint8Array>;
  /** Read the body as an ArrayBuffer. */
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Clone the request (fails if the body was already consumed). */
  clone(): Request;
}

/**
 * WHATWG Fetch `Response` class. Shared by `fetch()` and the legacy
 * `request()` global.
 */
declare class Response {
  constructor(
    body?: BodyInit,
    init?: { status?: number; statusText?: string; headers?: HeadersInit },
  );
  /** Response type: "default" | "basic" | "error" | "opaqueredirect". */
  readonly type: string;
  /** Response URL. */
  readonly url: string;
  /** Whether the request was redirected. */
  readonly redirected: boolean;
  /** HTTP status code. */
  readonly status: number;
  /** Whether status is in the 2xx range. */
  readonly ok: boolean;
  /** HTTP status text. */
  readonly statusText: string;
  /** Aike extension: the Content-Type header value. */
  readonly contentType: string;
  /** Aike extension: the Content-Length, or null if absent. */
  readonly contentLength: number | null;
  /** Response headers. */
  readonly headers: Headers;
  /** Whether the body has been consumed. */
  readonly bodyUsed: boolean;
  /** Async iterator over response body chunks, or null for null-body responses. */
  readonly body: AsyncIterableIterator<Uint8Array> | null;
  /** Read the entire body as bytes. */
  bytes(): Promise<Uint8Array>;
  /**
   * Read the entire body as a string. For text/html, an optional mode
   * converts the body: "strip" removes HTML tags, "markdown" converts to Markdown.
   */
  text(convert?: "strip" | "markdown"): Promise<string>;
  /** Read and parse the entire body as JSON. */
  json(): Promise<any>;
  /** Read the entire body as an ArrayBuffer. */
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Clone the response (fails if the body was already consumed). */
  clone(): Response;
  /** Close the response and release underlying resources. */
  close(): void;
  /** Returns a network-error Response (type "error", null body). */
  static error(): Response;
  /**
   * Returns a redirect Response (status 301/302/303/307/308) with a Location header.
   * @param url - Redirect target.
   * @param status - Redirect status; defaults to 302.
   */
  static redirect(url: string, status?: number): Response;
}

/**
 * Fetch a resource. Resolves to a `Response` once response headers arrive.
 *
 * If the request is aborted via `init.signal`, the returned Promise rejects
 * with a `TypeError` whose `name` is "AbortError".
 *
 * @param input - A `Request` object or a URL string.
 * @param init - Optional `RequestInit`.
 */
declare function fetch(
  input: RequestInfo,
  init?: RequestInit,
): Promise<Response>;
