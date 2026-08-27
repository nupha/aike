interface FileStat {
  size: number;
  /** Times in epoch milliseconds. createdAt absent when the platform has no birthtime (e.g. Linux). */
  createdAt?: number;
  modifiedAt?: number;
  accessedAt?: number;
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
 * Obtained via process.stdin/stdout/stderr or Aike.spawn().
 */
interface StdioHandle {
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
   * Register a callback for incoming raw byte chunks (streaming mode).
   * One consumer per handle: if an onLine listener is already active, this
   * registration is IGNORED with a printed warning (first registration
   * wins). Unsubscribing (`null`) pauses reading — buffered stream data is
   * preserved for the next subscription.
   */
  onData(callback: ((data: Uint8Array) => void) | null): void;
  /**
   * Register a newline-delimited text callback: incremental UTF-8 decoding,
   * each complete line delivered as a string. Lines split at "\n" with one
   * trailing "\r" stripped (CRLF handled); empty lines and lines over 1 MiB
   * are dropped. At EOF any trailing partial line is flushed first, then
   * `callback(null)` signals end-of-stream.
   *
   * One consumer per handle: if an onData listener is already active, this
   * registration is IGNORED with a printed warning (first registration
   * wins). Unsubscribing (`null`) pauses reading — data still buffered in
   * the underlying stream is preserved for the next subscription, but a
   * partially received line is discarded (an incomplete line belongs to no
   * one).
   */
  onLine(callback: ((line: string) => void) | null): void;

  /** Async iterator over data chunks. */
  [Symbol.asyncIterator](): AsyncIterableIterator<Uint8Array>;
}

interface CronJob {
  /** Unique job ID. */
  id: number;
  /** Optional job name. */
  name: string | null;
  /** The cron expression pattern. */
  pattern: string;
  /** ISO 8601 string of the next scheduled run time, or null. */
  nextRun: string | null;
  /** Whether the job is currently active. */
  active: boolean;
  /** Cancel this cron job. */
  cancel(): void;
}

/** Signal names supported by Aike.onSignal(). */
type SignalName =
  | "SIGINT"
  | "SIGABRT"
  | "SIGFPE"
  | "SIGILL"
  | "SIGSEGV"
  | "SIGTERM"
  // Unix-only signals:
  | "SIGQUIT"
  | "SIGPIPE"
  | "SIGALRM"
  | "SIGUSR1"
  | "SIGUSR2"
  | "SIGCHLD"
  | "SIGCONT"
  | "SIGSTOP"
  | "SIGTSTP"
  | "SIGTTIN"
  | "SIGTTOU";

// ---------------------------------------------------------------------------
// process — Node.js-compatible namespace
// ---------------------------------------------------------------------------

declare namespace process {
  // --- event emitter (process is an EventEmitter instance) ---

  /** Add a listener for an event. */
  function addListener(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Alias for addListener. */
  function on(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Add a one-time listener. */
  function once(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Remove a listener. */
  function removeListener(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Alias for removeListener. */
  function off(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Emit an event; returns false when no listener handled it. */
  function emit(event: string, ...args: unknown[]): boolean;
  /** Remove all listeners for an event, or all events when omitted. */
  function removeAllListeners(event?: string): typeof process;
  /** Get the listeners registered for an event. */
  function listeners(event: string): Function[];
  /** Get the number of listeners for an event. */
  function listenerCount(event: string): number;
  /** Get the names of all events with registered listeners. */
  function eventNames(): string[];

  // --- proc ---

  /** Returns the current working directory as a string. */
  function cwd(): string;

  /** Exits the process with the given exit code (default 0). */
  function exit(code?: number): never;

  /** Environment variables as a read-only key-value object. */
  const env: Record<string, string>;

  // --- info ---

  /** Process ID. */
  const pid: number;

  /** Parent process ID. */
  const ppid: number;

  /** CPU architecture ("x64", "arm64", etc.). */
  const arch: string;

  /** Runtime version string (e.g. "v0.1.0"). */
  const version: string;

  /** Version info object. */
  const versions: { aike: string };

  /** Process title (getter/setter). */
  let title: string;

  /** Returns the number of seconds the process has been running. */
  function uptime(): number;

  // --- control ---

  /** Change the current working directory. */
  function chdir(dir: string): void;

  /** Send a signal to a process. Default signal: SIGTERM. */
  function kill(pid: number, signal?: string | number): void;

  /** Schedule a callback to run on the next microtask. */
  function nextTick(callback: () => void): void;

  /** Returns memory usage statistics. */
  function memoryUsage(): {
    /** Resident set size in bytes. */
    rss: number;
    /** Total heap size in bytes. */
    heapTotal: number;
    /** Used heap size in bytes. */
    heapUsed: number;
    /** External memory usage in bytes. */
    external: number;
  };

  /** Returns CPU usage in microseconds. */
  function cpuUsage(): { user: number; system: number };

  /**
   * High-resolution monotonic time.
   * Returns [seconds, nanoseconds]. Pass a previous hrtime result to get the difference.
   */
  function hrtime(time?: [number, number]): [number, number];

  /**
   * Get or set the file mode creation mask (Unix only).
   * Returns the previous umask. On Windows, always returns 0.
   */
  function umask(mask?: number): number;

  // --- stdio ---

  /** Standard input handle. */
  const stdin: StdioHandle;
  /** Standard output handle. */
  const stdout: StdioHandle;
  /** Standard error handle. */
  const stderr: StdioHandle;

  // --- runtime ---

  /** Operating system platform. ('windows', 'darwin', 'linux', etc.) */
  const platform:
    | "windows"
    | "darwin"
    | "linux"
    | "android"
    | "freebsd"
    | "openbsd"
    | "aix"
    | string;

  /** Command-line arguments (argv[1:]). */
  const argv: readonly string[];
  /** Runtime flags passed to the aike binary. */
  const execArgv: readonly string[];
  /** Absolute path to the aike executable. */
  const execPath: string;
}

declare namespace Aike {
  interface Subprocess {
    /** Child process PID. */
    pid: number;
    /** Writable stdin handle (StdioHandle or null). */
    stdin: StdioHandle | null;
    /** Readable stdout handle (StdioHandle or null). */
    stdout: StdioHandle | null;
    /** Readable stderr handle (StdioHandle or null). */
    stderr: StdioHandle | null;
    /** Promise resolving to the exit code (or -1 on error). */
    exitCode: Promise<number>;
    /** Promise resolving to signal name or null. */
    signalCode: Promise<string | null>;
    /** Promise resolving to true if the process was killed by `timeout`. */
    killedByTimeout: Promise<boolean>;
    /** Send a signal to the child process. Default: SIGTERM. */
    kill(signal?: string | number): boolean;
    /** Check if the child process is still running. */
    isRunning(): boolean;

    /**
     * Graceful shutdown: closes stdin (EOF) and waits `eofTimeoutMs`,
     * then sends `termSignal` and waits `termTimeoutMs`, then SIGKILL and
     * waits `killTimeoutMs`. Resolves with the stage at which the child
     * exited, or "timed_out" if it survived SIGKILL's grace period.
     * Idempotent — a second call resolves with "already_closed".
     */
    shutdown(options?: ShutdownOptions): Promise<ShutdownResult>;
  }

  /** Options for {@link Subprocess.shutdown}. */
  interface ShutdownOptions {
    /** Close stdin, wait this long for the child to exit on its own. Default 2000. */
    eofTimeoutMs?: number;
    /** Send `termSignal`, wait this long. Default 2000. */
    termTimeoutMs?: number;
    /** Send SIGKILL, wait this long. Default 1000. */
    killTimeoutMs?: number;
    /** Second-stage signal. Default "SIGTERM". */
    termSignal?: "SIGTERM" | "SIGINT";
  }

  /**
   * The shutdown stage at which the child exited:
   * - `"eof"` — exited after stdin EOF
   * - `"term"` — exited after the term signal
   * - `"kill"` — exited after SIGKILL
   * - `"timed_out"` — still alive after SIGKILL's grace period
   */
  type ShutdownResult = "eof" | "term" | "kill" | "timed_out";

  interface SyncSubprocess {
    /** Child process PID. */
    pid: number;
    /** stdout as Uint8Array. */
    stdout: Uint8Array;
    /** stderr as Uint8Array. */
    stderr: Uint8Array;
    /** Exit code, or null if terminated by signal. */
    exitCode: number | null;
    /** Signal name if terminated by signal, or null. */
    signalCode: string | null;
    /** Whether the process exited successfully (exitCode 0, no signal). */
    success: boolean;
    /** Whether the process was killed because it exceeded `timeout`. */
    killedByTimeout: boolean;
  }

  /** Spawn a child process synchronously. */
  function spawnSync(
    command: string,
    args?: string[],
    options?: SpawnOptions,
  ): SyncSubprocess;

  /** Spawn a child process asynchronously. */
  function spawn(
    command: string,
    args?: string[],
    options?: SpawnOptions,
  ): Subprocess;

  interface SandboxMountOptions {
    /** In-sandbox path. Omitted = mount at the same path as the source. */
    target?: string;
    /** Default false (read-only). */
    writable?: boolean;
  }

  interface SpawnSandboxOptions {
    /** Map of host source path → mount options (insertion order preserved). */
    mounts?: Record<string, SandboxMountOptions>;
    tmpfs?: { path: string; size: number }[];
    rootfs?: string;
    /**
     * Memory limit in bytes.
     * NOTE: Not enforced on macOS (sandbox uses per-process memlimit only).
     */
    memoryLimit?: number;
    /**
     * CPU limit, 0 < n <= core count; fractions allowed.
     * NOTE: Not enforced on macOS.
     */
    cpuLimit?: number;
    /** Wall-clock time limit in milliseconds (also enforced independently). */
    wallTimeLimitMs?: number;
    /** CPU time limit in milliseconds. */
    cpuTimeLimitMs?: number;
    /**
     * Max concurrent processes (PIDs) in the sandbox.
     * NOTE: Not enforced on macOS.
     */
    maxPids?: number;
    /** Max file size in bytes for files created inside the sandbox. */
    maxFileSize?: number;
    /**
     * Max open file descriptors per process.
     * NOTE: Not enforced on macOS.
     */
    maxOpenFiles?: number;
    /** "none" (default) | full host network | domain allowlist (proxied). */
    network?: "none" | "host" | { allow: string[] };
    /**
     * Seccomp syscall filter profile (Linux only).
     * NOTE: Ignored on macOS (no seccomp support).
     */
    seccompProfile?:
      | "disabled"
      | "strict"
      | "standard"
      | "permissive"
      | string[];
    /**
     * Run as this UID inside the sandbox.
     * NOTE: Not enforced on macOS.
     */
    uid?: number;
    /**
     * Run as this GID inside the sandbox.
     * NOTE: Not enforced on macOS.
     */
    gid?: number;
    env?: Record<string, string>;
    /**
     * Set hostname inside the sandbox.
     * NOTE: Not enforced on macOS.
     */
    hostname?: string;
    // `cwd` intentionally NOT here: spawn's top-level `cwd` doubles as the
    // sandbox working directory when sandboxing is enabled.
  }

  interface SpawnOptions {
    /** Working directory for the child process. */
    cwd?: string;
    /** Environment variables (replaces entire env when provided). */
    env?: Record<string, string>;
    /** String, TypedArray, or ArrayBuffer to write to stdin (sync only). */
    input?: string | Uint8Array | ArrayBuffer;
    /** stdin mode: "pipe" | "inherit" | "ignore". Default: "inherit" */
    stdin?: "pipe" | "inherit" | "ignore";
    /** stdout mode: "pipe" | "inherit" | "ignore". Default: "pipe" */
    stdout?: "pipe" | "inherit" | "ignore";
    /** stderr mode: "pipe" | "inherit" | "ignore". Default: "pipe" */
    stderr?: "pipe" | "inherit" | "ignore";
    /**
     * Kill the child if it runs longer than this many milliseconds (wall
     * clock). Must be a positive integer. The whole process tree is killed
     * (process group on Unix, taskkill /T on Windows). Set `killedByTimeout`
     * on the result to detect the timeout.
     */
    timeout?: number;
    /**
     * When true, the child is killed (whole tree) synchronously when the
     * aike process exits — normally, via `process.exit`, or on a catchable
     * signal. Default: false — an unawaited child neither blocks aike from
     * exiting nor is killed; it is reparented and keeps running.
     *
     * On Linux a kernel `PDEATHSIG` additionally covers SIGKILL/crash; on
     * Windows a kill-on-close job object covers every termination path.
     *
     * Ignored by `spawnSync`/`Aike.sh` (the child has already exited by the
     * time the call returns).
     */
    abortOnParentExit?: boolean;

    /** Run the child inside a security sandbox (nanosandbox fork). */
    sandbox?: SpawnSandboxOptions;
  }

  // --- sys ---

  /**
   * Evaluate a JavaScript string in the global scope.
   * @param code - JavaScript source code.
   * @param options - evaluation options:
   *   - topLevelAwait?: compile the code with top-level `await` support.
   *   - stackBarrier?: truncate the error stack trace at this eval frame.
   *   - baseUrl?: filename or URL used as the base for relative `fs`/`fetch`
   *     resolution and reported in stack traces; defaults to `<evalScript>`.
   */
  function evalScript(
    code: string,
    options?: {
      topLevelAwait?: boolean;
      stackBarrier?: boolean;
      baseUrl?: string;
    },
  ): any;

  /**
   * Evaluate a JavaScript string as an ES module with an explicit base URL.
   *
   * Unlike `evalScript` (script/global mode), the code is compiled as a module,
   * so `import`/`export` and top-level `await` are allowed and `import.meta.url`
   * equals `baseUrl`. Relative `fs`/`fetch` specifiers inside the module
   * resolve against `baseUrl` (Node/Bun `import.meta.url` semantics).
   *
   * @param code - JavaScript source code (module).
   * @param baseUrl - A valid `file://`, `http://` or `https://` URL used as the
   *   module's identity and the base for relative resolution.
   * @returns A `Promise` resolving to the module's default/named exports.
   */
  function evalModule(code: string, baseUrl: string): Promise<any>;

  /** Trigger a manual garbage collection cycle. */
  function gc(): void;

  /** Check whether a file descriptor refers to a terminal. */
  function isatty(fd: number): boolean;

  /** Get the terminal window size for the given fd. Returns [cols, rows] or null. */
  function ttyGetWinSize(fd: number): [number, number] | null;

  /** Put the terminal referred to by fd into raw mode. */
  function ttySetRaw(fd: number): void;

  /** Returns the current monotonic time in milliseconds. */
  function now(): number;

  /** Returns the current user's home directory path. */
  function homedir(): string;

  /** Returns the system temporary directory path. */
  function tmpdir(): string;

  /**
   * Resolve a command name to its absolute executable path by searching `PATH`,
   * or `null` when not found. A `cmd` containing a path separator is treated as
   * a literal path. On Windows each `PATHEXT` extension is tried too.
   */
  function which(cmd: string): string | null;

  // --- gzip (aike 增强，非 WHATWG；memo §5.1) ---

  /**
   * Gzip-compress bytes into a complete RFC 1952 gzip member.
   * @param data - Uint8Array or ArrayBuffer to compress.
   */
  function gzip(data: Uint8Array | ArrayBuffer): Uint8Array;

  /** Gzip-decompress bytes. Throws a RangeError on invalid gzip data. */
  function gunzip(data: Uint8Array | ArrayBuffer): Uint8Array;

  // --- sh (aike 增强，非 WHATWG；memo §5.4) ---

  /** Result of `Aike.sh`: captured stdout/stderr as strings and the exit code. */
  interface ShResult {
    stdout: string;
    stderr: string;
    /** Exit code, or null if the command was terminated by a signal. */
    exitCode: number | null;
  }

  /** Options for `Aike.sh`. */
  interface ShOptions {
    /** Working directory for the shell. */
    cwd?: string;
    /** Environment; replaces the entire environment when provided. */
    env?: Record<string, string>;
    /** Timeout in milliseconds; the process tree is killed on overrun. */
    timeout?: number;
    /** Data written to the command's stdin. */
    input?: string | ArrayBufferView | ArrayBuffer;
    /** Throw an Error if the command exits non-zero or is killed by a signal. */
    check?: boolean;
  }

  /**
   * Run a command through the platform shell (`/bin/sh -c` on POSIX,
   * `%COMSPEC%` i.e. cmd.exe on Windows) and capture its output.
   */
  function sh(command: string, options?: ShOptions): ShResult;

  // --- download (aike 增强，非 WHATWG；memo §5.4) ---

  /** Options for `Aike.download`. */
  interface DownloadOptions {
    /** AbortSignal to cancel an in-flight download. */
    signal?: AbortSignal;
  }

  /**
   * Download a URL. With `destPath` the body is streamed to the file (written
   * to a temp file then atomically renamed) and the promise resolves with the
   * path; without it, the promise resolves with the body as a `Uint8Array`.
   * When `options.signal` aborts, the download is cancelled, any partial temp
   * file is removed, and the promise rejects with the signal's reason.
   */
  function download(
    url: string,
    destPath?: string,
    options?: DownloadOptions,
  ): Promise<string | Uint8Array>;

  // --- signal ---

  /**
   * Platform-available signal name → number mapping.
   * On Windows, only SIGINT, SIGABRT, SIGFPE, SIGILL, SIGSEGV, SIGTERM are present.
   * On Unix, all POSIX signals are available (including SIGQUIT, SIGUSR1, etc.).
   * Check `Aike.signals.SIGUSR1 !== undefined` to detect platform support.
   */
  const signals: {
    SIGINT: number;
    SIGABRT: number;
    SIGFPE: number;
    SIGILL: number;
    SIGSEGV: number;
    SIGTERM: number;
    // Unix-only signals (optional — check via !== undefined before use):
    SIGQUIT?: number;
    SIGPIPE?: number;
    SIGALRM?: number;
    SIGUSR1?: number;
    SIGUSR2?: number;
    SIGCHLD?: number;
    SIGCONT?: number;
    SIGSTOP?: number;
    SIGTSTP?: number;
    SIGTTIN?: number;
    SIGTTOU?: number;
  };

  /**
   * Register or unregister a signal handler.
   * Pass null as handler to unregister.
   */
  function onSignal(
    signal: SignalName,
    handler: (() => void) | null,
  ): typeof Aike;

  // --- cron / timer ---

  /**
   * Schedule a recurring task using a cron expression.
   * Supports 5-field (min hour dom month dow) or 6/7-field expressions.
   * Returns a CronJob instance.
   */
  function schedule(
    pattern: string,
    callback: () => void,
    options?: { name?: string },
  ): CronJob;

  /**
   * Delay execution for `ms` milliseconds.
   *
   * Uses the event loop — does NOT block the thread. Returns a `Promise` that
   * resolves with `undefined` after the delay. Analogous to `Bun.sleep(ms)`.
   *
   * @param ms - Milliseconds to wait (clamped to >= 0).
   */
  function sleep(ms: number): Promise<void>;

  // --- persistent ---

  /**
   * Serialize any JavaScript value into a self-contained bytecode blob and
   * return it as a `Uint8Array`. The blob can be reloaded in any Runtime
   * (including a fresh one) via `Aike.restoreValue`.
   *
   * Supported value types: undefined, null, booleans, numbers, strings, bigint,
   * symbols (global/well-known), plain objects, arrays (with cyclic references),
   * functions (closures, generators, async, arrow), and modules.
   * Native (C) functions are persisted by name only and must be re-registered
   * on the target runtime.
   *
   * Throws a TypeError when the value cannot be persisted.
   */
  function dumpValue(value: unknown): Uint8Array;

  /**
   * Rebuild the value previously produced by `Aike.dumpValue` from a
   * `Uint8Array` blob and return it.
   *
   * Throws a TypeError when `bytes` is not a `Uint8Array`, the blob is
   * malformed/truncated, or a referenced value cannot be reconstructed.
   */
  function restoreValue(bytes: Uint8Array): unknown;
}

// ---------------------------------------------------------------------------
// ImportMeta  —  the import.meta object available in every ES module
// ---------------------------------------------------------------------------
// ES modules expose `import.meta`; `url` is the module's absolute file URL
// (e.g. "file:///abs/path/mod.ts"). aike resolves it against the base URL.
interface ImportMeta {
  url: string;
}
