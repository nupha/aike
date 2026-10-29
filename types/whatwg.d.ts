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

/**
 * Queue a microtask to run `callback` once the JavaScript execution stack is
 * next empty (before control returns to the event loop). Throws a TypeError
 * if `callback` is not callable. Per WHATWG HTML §8.8 Microtask queuing.
 */
declare function queueMicrotask(callback: () => void): void;

/**
 * Deep clone a value using the WHATWG structured clone algorithm. Clones
 * objects, arrays, maps, sets, dates, regexps, errors, ArrayBuffers and typed
 * arrays; cycles and duplicate references are preserved. Symbols, functions,
 * promises, WeakMap/WeakSet and other non-serializable objects throw a
 * `DataCloneError` DOMException. `options.transfer` transfers (detaches) the
 * listed ArrayBuffers instead of cloning them.
 */
declare function structuredClone<T>(
  value: T,
  options?: {
    /** ArrayBuffers to transfer (detached in the source) instead of clone. */
    transfer?: ArrayBuffer[];
  },
): T;

/**
 * WHATWG Fetch `Response` class (strict spec subset, no aike extensions).
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
  /** Response headers. */
  readonly headers: Headers;
  /** Whether the body has been consumed. */
  readonly bodyUsed: boolean;
  /** ReadableStream over response body chunks, or null for null-body responses. */
  readonly body: ReadableStream<Uint8Array> | null;
  /** Read the entire body as bytes. */
  bytes(): Promise<Uint8Array>;
  /** Read the entire body as a string. */
  text(): Promise<string>;
  /** Read and parse the entire body as JSON. */
  json(): Promise<any>;
  /** Read the entire body as an ArrayBuffer. */
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Clone the response (fails if the body was already consumed). */
  clone(): Response;
  /** Returns a network-error response (type "error", null body). */
  static error(): Response;
  /**
   * Returns a redirect response (status 301/302/303/307/308) with a Location header.
   * @param url - Redirect target.
   * @param status - Redirect status; defaults to 302.
   */
  static redirect(url: string, status?: number): Response;
  /** Returns a JSON response with Content-Type: application/json. */
  static json(
    data: any,
    init?: { status?: number; statusText?: string; headers?: HeadersInit },
  ): Response;
}

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
// Base64
// ---------------------------------------------------------------------------

/**
 * Encode a string as base64 (WHATWG: each code point becomes a byte, so only
 * Latin-1 code points ≤ 0xFF are allowed; throws InvalidCharacterError).
 */
declare function btoa(data: string): string;

/**
 * Decode a base64 string back to a string (each decoded byte becomes a Latin-1
 * code point). Follows WHATWG forgiving-base64: ASCII whitespace is ignored
 * and unpadded input is accepted; malformed input throws InvalidCharacterError.
 */
declare function atob(data: string): string;

/** Base64-encode a raw byte array. */
declare function bytesToBase64(bytes: Uint8Array | ArrayBuffer): string;

/**
 * Base64-decode a string into raw bytes. Follows WHATWG forgiving-base64
 * (whitespace ignored, unpadded accepted); throws on malformed input.
 */
declare function base64ToBytes(data: string): Uint8Array;

/**
 * Encode raw bytes as a lowercase hexadecimal string.
 * @param bytes - Uint8Array or ArrayBuffer to encode.
 */
declare function bytesToHex(bytes: Uint8Array | ArrayBuffer): string;

/**
 * Decode a hexadecimal string (lowercase or uppercase) into a Uint8Array.
 * Throws a RangeError on an odd length or a non-hex character.
 */
declare function hexToBytes(data: string): Uint8Array;

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
  /** Get an iterator over all key-value pairs. */
  entries(): IterableIterator<[string, string]>;
  /** Get an iterator over all keys. */
  keys(): IterableIterator<string>;
  /** Get an iterator over all values. */
  values(): IterableIterator<string>;
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
   * EventTarget subset (AbortSignal is an EventTarget per WHATWG). Only the
   * `"abort"` event is dispatched; the listener is called with a minimal
   * `{ type: "abort", target }` event object.
   */
  addEventListener(type: string, callback: (...args: unknown[]) => void): void;
  /** EventTarget subset — remove a listener previously added. */
  removeEventListener(
    type: string,
    callback: (...args: unknown[]) => void,
  ): void;
  /** EventTarget subset — spec-compliant no-op for AbortSignal (returns true). */
  dispatchEvent(event: unknown): boolean;

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
  /**
   * Returns every `Set-Cookie` value as a separate array element.
   *
   * `get("set-cookie")` cannot express several cookies — each value may itself
   * contain a comma (`Expires=Wed, 21 Oct 2035 07:28:00 GMT`), so the
   * comma-joined result is not splittable.
   */
  getSetCookie(): string[];
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
  /**
   * Credentials mode: "omit" | "same-origin" | "include". Default "include".
   *
   * Aike default (no `fetch_cors` feature): "same-origin" also behaves as
   * "include", as there is no client origin to scope cookies to.
   */
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
  /** Request mode. Default "cors"; not enforced without `fetch_cors`. */
  readonly mode: string;
  /** Credentials mode, as given. See {@link RequestInit.credentials}. */
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
  /** ReadableStream over request body chunks, or null if none. */
  readonly body: ReadableStream<Uint8Array> | null;
  /** Read the body as bytes. */
  bytes(): Promise<Uint8Array>;
  /** Read the body as an ArrayBuffer. */
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Clone the request (fails if the body was already consumed). */
  clone(): Request;
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

/**
 * Runtime-level switch on `fetch` itself (aike extension, not per-request).
 * Browser CORS/cookie semantics have no switch — they follow the compile-time
 * `fetch_cors_cookie` feature (off by default; WPT builds enable it).
 */
declare namespace fetch {
  /**
   * Whether this runtime's `fetch()` skips TLS certificate
   * validation. A test/dev escape hatch for self-signed servers; default off.
   */
  let allowInsecureTLS: boolean;
}

// ---------------------------------------------------------------------------
// ReadableStream
// ---------------------------------------------------------------------------

/** WhatWG ReadableStream — a source of data that can be read chunk by chunk. */
declare class ReadableStream<T = any> {
  constructor(
    underlyingSource?: UnderlyingSource<T>,
    strategy?: QueuingStrategy<T>,
  );

  /** Whether the stream is locked to a reader. */
  readonly locked: boolean;

  /** Create a reader and lock the stream. */
  getReader(): ReadableStreamDefaultReader<T>;

  /** Cancel the stream, optionally with a reason. */
  cancel(reason?: any): Promise<void>;

  /** Pipe the stream through a transform and into a writable destination. */
  pipeThrough<R>(
    transform: { readable: ReadableStream<R>; writable: WritableStream<T> },
    options?: {
      preventClose?: boolean;
      preventAbort?: boolean;
      preventCancel?: boolean;
    },
  ): ReadableStream<R>;

  /** Pipe the stream to a writable stream. */
  pipeTo(
    dest: WritableStream<T>,
    options?: {
      preventClose?: boolean;
      preventAbort?: boolean;
      preventCancel?: boolean;
    },
  ): Promise<void>;

  /** Async iterator over stream chunks (spans multiple reads). */
  [Symbol.asyncIterator](): AsyncIterableIterator<T>;
}

/** Options for constructing a ReadableStream. */
interface UnderlyingSource<T = any> {
  start?: (controller: ReadableStreamDefaultController<T>) => void;
  pull?: (controller: ReadableStreamDefaultController<T>) => void;
  cancel?: (reason?: any) => void;
  type?: "bytes";
  autoAllocateChunkSize?: number;
}

/** Default reader for a ReadableStream. */
declare class ReadableStreamDefaultReader<T = any> {
  /** Lock the stream and create a reader. */
  private constructor();

  /** Read the next chunk (or signal done). */
  read(): Promise<ReadableStreamReadResult<T>>;

  /** Cancel the stream. */
  cancel(reason?: any): Promise<void>;

  /** Release the lock, allowing another reader. */
  releaseLock(): void;
}

/** Result of a stream read. */
interface ReadableStreamReadResult<T> {
  value: T | undefined;
  done: boolean;
}

/** Controller for a ReadableStream (exposed to the underlying source). */
declare class ReadableStreamDefaultController<T = any> {
  private constructor();

  /** Enqueue a chunk. */
  enqueue(chunk: T): void;

  /** Close the stream (no more data). */
  close(): void;

  /** Signal an error. */
  error(e?: any): void;

  /** The desired size to fill the internal buffer. */
  readonly desiredSize: number | null;
}

// ---------------------------------------------------------------------------
// WritableStream
// ---------------------------------------------------------------------------

/** WhatWG WritableStream — a destination for data. */
declare class WritableStream<T = any> {
  constructor(
    underlyingSink?: UnderlyingSink<T>,
    strategy?: QueuingStrategy<T>,
  );

  /** Whether the stream is locked to a writer. */
  readonly locked: boolean;

  /** Create a writer and lock the stream. */
  getWriter(): WritableStreamDefaultWriter<T>;

  /** Abort the stream with a reason. */
  abort(reason?: any): Promise<void>;

  /** Close and signal no more writes. */
  close(): Promise<void>;
}

/** Options for constructing a WritableStream. */
interface UnderlyingSink<T = any> {
  start?: (controller: WritableStreamDefaultController) => void;
  write?: (chunk: T, controller: WritableStreamDefaultController) => void;
  close?: () => void;
  abort?: (reason?: any) => void;
  type?: "bytes";
}

/** Default writer for a WritableStream. */
declare class WritableStreamDefaultWriter<T = any> {
  private constructor();

  /** Write a chunk. */
  write(chunk: T): Promise<void>;

  /** Close the writer (and the stream). */
  close(): Promise<void>;

  /** Abort the stream. */
  abort(reason?: any): Promise<void>;

  /** Release the lock. */
  releaseLock(): void;

  /** The desired size to fill the internal buffer. */
  readonly desiredSize: number | null;
}

/** Controller for a WritableStream (exposed to the underlying sink). */
declare class WritableStreamDefaultController {
  private constructor();
  error(e?: any): void;
}

// ---------------------------------------------------------------------------
// TransformStream
// ---------------------------------------------------------------------------

/** WhatWG TransformStream — transforms data passing from readable to writable. */
declare class TransformStream<T = any, R = any> {
  constructor(
    transformer?: Transformer<T, R>,
    writableStrategy?: QueuingStrategy<T>,
    readableStrategy?: QueuingStrategy<R>,
  );

  /** The readable side. */
  readonly readable: ReadableStream<R>;
  /** The writable side. */
  readonly writable: WritableStream<T>;
}

/** Options for constructing a TransformStream. */
interface Transformer<T = any, R = any> {
  start?: (controller: TransformStreamDefaultController<R>) => void;
  transform?: (
    chunk: T,
    controller: TransformStreamDefaultController<R>,
  ) => void;
  flush?: (controller: TransformStreamDefaultController<R>) => void;
  cancel?: (reason?: any) => void;
  readableType?: never;
  writableType?: never;
}

/** Controller for a TransformStream. */
declare class TransformStreamDefaultController<R = any> {
  private constructor();

  /** Enqueue a chunk to the readable side. */
  enqueue(chunk: R): void;

  /** Signal an error. */
  error(reason?: any): void;

  /** Terminate the stream — no more data on readable. */
  terminate(): void;

  /** The desired size to fill the internal buffer. */
  readonly desiredSize: number | null;
}

// ---------------------------------------------------------------------------
// Queuing strategies
// ---------------------------------------------------------------------------

/** WhatWG QueuingStrategy — determines how chunks are counted for backpressure. */
interface QueuingStrategy<T = any> {
  highWaterMark?: number;
  size?: (chunk: T) => number;
}

/** Strategy that sizes chunks by byte length. */
declare class ByteLengthQueuingStrategy<
  T extends ArrayBufferView = ArrayBufferView,
> {
  constructor(init: { highWaterMark: number });
  highWaterMark: number;
  size(chunk: T): number;
}

/** Strategy that counts each chunk as 1. */
declare class CountQueuingStrategy {
  constructor(init: { highWaterMark: number });
  highWaterMark: number;
  size(): number;
}

// ---------------------------------------------------------------------------
// Blob
// ---------------------------------------------------------------------------

/** A part accepted by the `Blob` constructor: string, BufferSource, or Blob. */
type BlobPart = string | ArrayBufferView | ArrayBuffer | Blob;

/** Options for the `Blob` constructor and `Blob.slice()`. */
interface BlobPropertyBag {
  /** MIME type; control characters are stripped and the value is lower-cased. */
  type?: string;
  /**
   * Line-ending handling. Only validated (`"transparent"` or `"native"`);
   * on this runtime both behave identically (no newline translation).
   */
  endings?: "transparent" | "native";
}

/**
 * WHATWG `Blob`: an immutable, type-tagged bag of bytes.
 * Slicing shares the underlying buffer without copying.
 */
declare class Blob {
  constructor(blobParts?: BlobPart[], options?: BlobPropertyBag);
  /** Size of the blob contents in bytes. */
  readonly size: number;
  /** Lower-cased MIME type of the blob contents. */
  readonly type: string;
  /**
   * Return a new `Blob` covering the byte range [start, end), sharing the
   * underlying buffer. Negative indices count from the end; out-of-range
   * values are clamped.
   * @param start - Start byte index (default 0).
   * @param end - End byte index, exclusive (default size).
   * @param contentType - MIME type for the new blob (default inherited).
   */
  slice(start?: number, end?: number, contentType?: string): Blob;
  /** Read the entire blob as an `ArrayBuffer`. */
  arrayBuffer(): Promise<ArrayBuffer>;
  /** Decode the entire blob as UTF-8 text. */
  text(): Promise<string>;
  /** Read the entire blob as a `Uint8Array`. */
  bytes(): Promise<Uint8Array>;
  /** Return a `ReadableStream` over the blob's bytes. */
  stream(): ReadableStream<Uint8Array>;
}

// ---------------------------------------------------------------------------
// Cookie Store API — https://cookiestore.spec.whatwg.org/
// ---------------------------------------------------------------------------

/** Options for `CookieStore.get()` / `getAll()`. */
interface CookieStoreGetOptions {
  name?: string;
  url?: string;
}

/** `CookieInit.sameSite` enum. */
type CookieSameSite = "strict" | "lax" | "none";

/** Options for `CookieStore.set()`. */
interface CookieInit {
  name: string;
  value: string;
  /** Expiry as a DOMHighResTimeStamp (ms since epoch); `null` = session. */
  expires?: number | null;
  domain?: string | null;
  path?: string;
  sameSite?: CookieSameSite;
  partitioned?: boolean;
  /** Lifetime in seconds; mutually exclusive with `expires`. */
  maxAge?: number | null;
}

/** Options for `CookieStore.delete()`. */
interface CookieStoreDeleteOptions {
  name: string;
  domain?: string | null;
  path?: string;
  partitioned?: boolean;
}

/** A cookie as returned by `get()` / `getAll()`. */
interface CookieListItem {
  name: string;
  value: string;
}

type CookieList = CookieListItem[];

/**
 * WHATWG `CookieStore`: asynchronous cookie access. Operates on the same
 * cookie store as `fetch()`.
 *
 * Only present in `fetch_cors_cookie` builds (off in the default lean build).
 *
 * aike has no `Document`; the "current URL" is the running script/module URL
 * (`import.meta.url`). A `file://` script has an opaque origin, so the methods
 * reject with a `SecurityError` — run under an `http(s)` base URL (e.g. via
 * `Aike.evalModule(code, baseUrl)`) to use it.
 */
declare class CookieStore {
  get(name: string): Promise<CookieListItem | null>;
  get(options: CookieStoreGetOptions): Promise<CookieListItem | null>;
  getAll(name: string): Promise<CookieList>;
  getAll(options?: CookieStoreGetOptions): Promise<CookieList>;
  set(name: string, value: string): Promise<void>;
  set(options: CookieInit): Promise<void>;
  delete(name: string): Promise<void>;
  delete(options: CookieStoreDeleteOptions): Promise<void>;
}

/** The global `CookieStore` instance. */
declare const cookieStore: CookieStore;
