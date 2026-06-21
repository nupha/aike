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

// ---------------------------------------------------------------------------
// EventEmitter
// ---------------------------------------------------------------------------

declare class EventEmitter {
  addListener(event: string, listener: (...args: any[]) => void): this;
  on(event: string, listener: (...args: any[]) => void): this;
  once(event: string, listener: (...args: any[]) => void): this;
  removeListener(event: string, listener: (...args: any[]) => void): this;
  off(event: string, listener: (...args: any[]) => void): this;
  emit(event: string, ...args: any[]): boolean;
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
// StdioHandle / FileHandle  —  used by Aike.stdin/stdout/stderr and fs.open()
// ---------------------------------------------------------------------------

interface FileStat {
  size: number;
  createdAt?: string;
  modifiedAt?: string;
  accessedAt?: string;
  isFile: boolean;
  isDirectory: boolean;
  isSymbolicLink: boolean;
  /** Unix file mode (only on Unix platforms). */
  mode?: number;
  /** Unix UID (only on Unix platforms). */
  uid?: number;
  /** Unix GID (only on Unix platforms). */
  gid?: number;
}

/**
 * I/O handle for standard streams (stdin, stdout, stderr) and pipe write ends.
 * Obtained via Aike.stdin/stdout/stderr or spawn().
 */
declare class StdioHandle {
  /** File descriptor number. */
  readonly fd: number;
  /** Read all content as a UTF-8 string. */
  text(): Promise<string>;
  /** Read and parse all content as JSON. */
  json(): Promise<any>;
  /** Read all content as a Uint8Array. */
  bytes(): Promise<Uint8Array>;
  /** Read up to `maxCount` bytes. */
  read(maxCount?: number): Promise<Uint8Array>;
  /** Get file statistics. */
  stat(): Promise<FileStat>;
  /**
   * Write data synchronously.
   * Returns boolean, no yield.
   */
  writeSync(data: string | Uint8Array | ArrayBuffer): boolean;
  /**
   * Flush write end — no-op for pipes and standard streams.
   */
  flush(): void;
  /** Close this I/O handle. */
  close(): void;
  /**
   * Register a callback for incoming data (streaming mode).
   * Pass null to unregister.
   */
  onData(callback: ((data: Uint8Array) => void) | null): void;
  /** Async iterator over data chunks. */
  [Symbol.asyncIterator](): AsyncIterableIterator<Uint8Array>;
}

/**
 * I/O handle for files and pipe read ends (child stdout/stderr).
 * Obtained via fs.open() or spawn().
 */
declare class FileHandle {
  /** File descriptor number. */
  readonly fd: number;
  /** Read all content as a UTF-8 string. */
  text(): Promise<string>;
  /** Read and parse all content as JSON. */
  json(): Promise<any>;
  /** Read all content as a Uint8Array. */
  bytes(): Promise<Uint8Array>;
  /** Read up to `maxCount` bytes. */
  read(maxCount?: number): Promise<Uint8Array>;
  /** Get file statistics. */
  stat(): Promise<FileStat>;
  /**
   * Write data asynchronously.
   */
  write(data: string | Uint8Array | ArrayBuffer): Promise<void>;
  /**
   * Flush buffered data asynchronously.
   */
  flush(): Promise<void>;
  /** Close this I/O handle. */
  close(): void;
  /**
   * Register a callback for incoming data (streaming mode).
   * Pass null to unregister.
   */
  onData(callback: ((data: Uint8Array) => void) | null): void;
  /** Async iterator over data chunks. */
  [Symbol.asyncIterator](): AsyncIterableIterator<Uint8Array>;
}
