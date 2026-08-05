// Type declarations for process and Aike global objects

// ---------------------------------------------------------------------------
// StdioHandle  —  used by process.stdin/stdout/stderr and Aike.spawn()
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
   * Register a callback for incoming data (streaming mode).
   * Pass null to unregister.
   */
  onData(callback: ((data: Uint8Array) => void) | null): void;
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

/** Alias for SpawnAsyncResult, returned by Aike.spawn(). */
type ChildProcess = Aike.SpawnAsyncResult;

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
  function on(event: string, listener: (...args: unknown[]) => void): typeof process;
  /** Add a one-time listener. */
  function once(event: string, listener: (...args: unknown[]) => void): typeof process;
  /** Remove a listener. */
  function removeListener(
    event: string,
    listener: (...args: unknown[]) => void,
  ): typeof process;
  /** Alias for removeListener. */
  function off(event: string, listener: (...args: unknown[]) => void): typeof process;
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

// ---------------------------------------------------------------------------
// Aike — Aike-specific namespace
// ---------------------------------------------------------------------------

declare namespace Aike {
  // --- spawn ---

  /** Spawn a child process synchronously. */
  function spawnSync(
    command: string,
    args?: string[],
    options?: SpawnOptions,
  ): SpawnSyncResult;

  /** Spawn a child process asynchronously. */
  function spawn(
    command: string,
    args?: string[],
    options?: SpawnOptions,
  ): SpawnAsyncResult;

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
     * clock). The whole process tree is killed (process group on Unix,
     * taskkill /T on Windows). Set `killedByTimeout` on the result to detect
     * the timeout.
     */
    timeout?: number;
  }

  interface SpawnSyncResult {
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

  interface SpawnAsyncResult {
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
}
