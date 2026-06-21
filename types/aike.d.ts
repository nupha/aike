// Type declarations for Aike JS runtime — Aike global object
// Generated from llama-js/src/aike/ Rust source code.

// ---------------------------------------------------------------------------
// Aike — the global branding namespace
// ---------------------------------------------------------------------------

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
   * Register or unregister a signal handler.
   * Pass null as handler to unregister.
   */
  function onSignal(
    signal: SignalName,
    handler: (() => void) | null,
  ): void;

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

  // --- stdio ---

  /** Standard input handle. */
  const stdin: StdioHandle;
  /** Standard output handle. */
  const stdout: StdioHandle;
  /** Standard error handle. */
  const stderr: StdioHandle;

  // --- runtime ---

  /** Command-line arguments (argv[1:]). */
  const argv: readonly string[];
  /** Runtime flags passed to the aike binary. */
  const execArgv: readonly string[];
  /** Absolute path to the aike executable. */
  const execPath: string;

}
