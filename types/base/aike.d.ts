// Type declarations for Aike JS runtime — Aike global object

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

/**
 * I/O handle for files and pipe read ends (child stdout/stderr).
 * Obtained via fs.open() or spawn().
 */
interface FileHandle {
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
type AikeChildProcess = Aike.SpawnAsyncResult;

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

declare namespace Aike {
  // --- proc ---

  /** Returns the current working directory as a string. */
  function cwd(): string;

  /** Exits the process with the given exit code (default 0). */
  function exit(code?: number): never;

  /** Environment variables as a read-only key-value object. */
  const env: Record<string, string>;

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
    /** Send a signal to the child process. Default: SIGTERM. */
    kill(signal?: string | number): boolean;
    /** Check if the child process is still running. */
    isRunning(): boolean;
  }

  // --- sys ---

  /**
   * Evaluate a JavaScript string in the global scope.
   * @param code - JavaScript source code.
   * @param options - { async?: boolean; backtrace_barrier?: boolean }
   */
  function evalScript(
    code: string,
    options?: { async?: boolean; backtrace_barrier?: boolean },
  ): any;

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

  // --- cron ---

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

  // --- stdio ---

  /** Standard input handle. */
  const stdin: StdioHandle;
  /** Standard output handle. */
  const stdout: StdioHandle;
  /** Standard error handle. */
  const stderr: StdioHandle;

  // --- runtime ---

  /** Operating system platform. Follows Node.js convention: 'win32', 'darwin', 'linux', etc. */
  const platform: "windows" | "darwin" | "linux" | "android" | "freebsd" | "openbsd" | "aix" | string;

  /** Command-line arguments (argv[1:]). */
  const argv: readonly string[];
  /** Runtime flags passed to the aike binary. */
  const execArgv: readonly string[];
  /** Absolute path to the aike executable. */
  const execPath: string;
}
