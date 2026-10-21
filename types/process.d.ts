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

  /**
   * Operating system platform, Node-style — `"win32"` on Windows, not
   * `"windows"`. `os.platform()` returns the same value.
   *
   * The set is closed; do not widen it with `| string`, which would erase the
   * literal types.
   */
  const platform:
    | "win32"
    | "darwin"
    | "linux"
    | "android"
    | "freebsd"
    | "openbsd"
    | "netbsd"
    | "aix"
    | "sunos";

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
    /** Whether this child was spawned inside a sandbox. */
    sandboxed: boolean;
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
    /**
     * Send a signal to the child process. Default: SIGTERM.
     *
     * Outside a sandbox the signal is delivered to the child directly on Unix,
     * so the child's own handler runs and, if the signal kills it instead,
     * `signalCode` reports it. Windows has no POSIX signals. Inside a sandbox
     * the signal goes to the whole tree, and whether the handler runs differs
     * by platform:
     *
     * | platform | sandbox | handler runs | signalCode |
     * | --- | --- | --- | --- |
     * | macOS | no | yes | the signal, if it died from one |
     * | Linux | no | yes | the signal, if it died from one |
     * | Windows | no | no (`TerminateProcess`) | always null |
     * | macOS | yes | yes (`sandbox-exec` forwards) | the signal, if it died from one |
     * | Linux | yes | no (`bwrap` SIGKILLs the namespace) | the signal `bwrap` died from |
     * | Windows | yes | no (`TerminateJobObject`) | always null |
     *
     * So a `SIGTERM` handler is only worth relying on when not sandboxed (on
     * Unix) or under macOS; elsewhere treat `kill` as "terminate the tree now".
     */
    kill(signal?: string | number): boolean;
    /** Check if the child process is still running. */
    isRunning(): boolean;

    /**
     * Graceful shutdown: closes stdin (EOF) and waits `eofTimeoutMs`,
     * then sends `termSignal` and waits `termTimeoutMs`, then SIGKILL and
     * waits `killTimeoutMs`. Resolves with the stage at which the child
     * exited, or "timed_out" if it survived SIGKILL's grace period.
     * Idempotent — a second call resolves with "already_closed".
     *
     * The `"term"` stage is reached on every platform. Whether the command's
     * own handler runs first depends on the platform and whether it is
     * sandboxed — see {@link Subprocess.kill}: outside a sandbox Unix delivers
     * the signal and the handler runs, while inside a sandbox only macOS does.
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

  /**
   * Runs the command inside an OS-enforced sandbox. Reads are open across the
   * machine except for `denied` paths — the same default the macOS backend gets
   * from Seatbelt and the one Claude Code and Codex use; the working directory
   * is the only writable place, plus any `rw` grant; and network is off unless
   * asked for.
   *
   * Three backends enforce this, each with the primitive that works on its
   * platform: Seatbelt on macOS, AppContainer on Windows, bubblewrap on Linux.
   * Ask `Aike.sandboxSupport()` before offering the option — on a machine
   * whose backend is missing or unusable this throws rather than running the
   * command without a sandbox.
   *
   * @example
   * Aike.sh("npm test", {
   *   cwd: "/path/to/project", // the one directory it may write
   *   sandbox: {
   *     grants: [{ path: "/opt/toolchain", access: "rx" }],
   *     denied: ["/path/to/project/.env"],
   *   },
   * });
   */
  interface SandboxOptions {
    /**
     * Extra paths the command may reach, in addition to the working
     * directory (`cwd`, or the current directory when it is unset).
     *
     * `access` is `"r"`, `"rw"`, `"rx"` or `"rwx"`. Write and execute both
     * imply read — opening a file to write it is itself a read, and no
     * platform can express otherwise, so a mode without `r` is rejected.
     *
     * A path granted `rw` without `x` is **not** runnable: that is what stops
     * a command from dropping a payload somewhere writable and executing it.
     * Two platforms have gaps here, both documented rather than hidden:
     *
     * - Linux: `bwrap --bind` cannot separate the execute bit, so a writable
     *   directory is also runnable.
     * - Windows: the launching process maps the *entry* image, so the image's
     *   own ACL never gates the first `exec`. `Aike.spawn`/`Aike.sh` refuse to
     *   start a program the sandbox left without execute, and a program
     *   started from outside the writable area cannot in turn launch one.
     */
    grants?: { path: string; access: "r" | "rw" | "rx" | "rwx" }[];

    /**
     * Paths the command may not reach at all, whatever `grants` says. Must
     * exist and be absolute. A `denied` path that contains a granted one, or
     * that covers `cwd`, is rejected at build time instead of silently
     * winning.
     *
     * The path's *name* stays visible on macOS and Linux — its parent still
     * lists it and `stat` on it still succeeds; only the contents are hidden.
     * macOS refuses to list a denied directory's own entries (since `readdir`
     * needs read access to the directory itself), while Linux mounts an empty
     * directory over it; a denied file cannot be read on either.
     *
     * Windows cannot express a `denied` path that lies *inside* `cwd` or a
     * grant: an AppContainer ACL grants access but cannot take it back from an
     * already-allowed subtree. Such a configuration is rejected at build time
     * rather than left silently unenforced.
     */
    denied?: string[];

    /**
     * `"none"` (default) refuses every outbound connection; `"host"` gives
     * the command the machine's full network access.
     *
     * A domain allowlist (`{allow: [...]}`) is not implemented yet — no proxy
     * runs to enforce it — so it is refused rather than quietly downgraded to
     * a total block.
     */
    network?: "none" | "host";

    // `cwd`, `env` and `timeout` are intentionally NOT here: they live on the
    // spawn options and reach the sandbox unchanged. "Where the command
    // starts" is one concept, and a second name for it is what once produced a
    // top-level `cwd` that was silently ignored.
  }

  /** What this machine can sandbox, and why not if it cannot. */
  interface SandboxSupport {
    available: boolean;
    /** "seatbelt" | "appcontainer" | "bubblewrap", or null when unavailable. */
    backend: "seatbelt" | "appcontainer" | "bubblewrap" | null;
    /** Why it is unavailable, phrased for the user. Null when it is. */
    reason: string | null;
  }

  interface SpawnOptions {
    /**
     * Working directory for the child process.
     *
     * With `sandbox`, this is also the one directory the command may write,
     * and is where the sandbox rules are anchored. Omitted means the current
     * directory, for a sandboxed and a plain spawn alike.
     */
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

    /** Run the child inside sandbox mode. */
    sandbox?: SandboxOptions;
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

  // `homedir()` / `tmpdir()` live on the `os` module — use `os.homedir()` and
  // `os.tmpdir()`. Not re-exported here.

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
    /** True when the command ran inside a sandbox. */
    sandboxed?: boolean;
    /** True when the command was killed for exceeding `ShOptions.timeout`. */
    killedByTimeout?: boolean;
    /** True when the command exited zero. */
    success?: boolean;
  }

  /** Options for `Aike.sh`. */
  interface ShOptions {
    /**
     * Working directory for the shell.
     *
     * With `sandbox`, this is also the one directory the command may write.
     * Omitted means the current directory.
     */
    cwd?: string;
    /** Environment; replaces the entire environment when provided. */
    env?: Record<string, string>;
    /** Timeout in milliseconds; the process tree is killed on overrun. */
    timeout?: number;
    /** Data written to the command's stdin. */
    input?: string | ArrayBufferView | ArrayBuffer;
    /** Throw an Error if the command exits non-zero or is killed by a signal. */
    check?: boolean;
    /**
     * Run the command inside an OS-enforced sandbox. See
     * {@link SandboxOptions}. The command starts in `cwd` (the current
     * directory when unset), which is also the only place it may write.
     */
    sandbox?: SandboxOptions;
  }

  /**
   * Run a command through the platform shell (`/bin/sh -c` on POSIX,
   * `%COMSPEC%` i.e. cmd.exe on Windows) and capture its output.
   */
  function sh(command: string, options?: ShOptions): ShResult;

  /**
   * Whether this machine can run a sandbox. Check before offering the
   * `sandbox` option: where the backend is missing or unusable, passing
   * `sandbox` throws instead of running the command unsandboxed.
   */
  function sandboxSupport(): SandboxSupport;

  // --- signal ---

  // The signal name → number table lives at `os.constants.signals`.
  // `Aike.onSignal` takes a name directly, so the table is not needed here.

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

  // --- multi-runtime (Aike 增强) ---

  /** Options for {@link Aike.createRuntime}. */
  interface RuntimeOptions {
    /** Heap upper bound in bytes; 0/omitted = unlimited. */
    memory?: number;
    /** Per-`run` wall-clock timeout in ms; 0/omitted = unlimited. */
    timeout?: number;
    /**
     * Host **globals** to install in the child runtime:
     * - `string[]` — whitelist (equivalent to `{ whiteList: [...] }`)
     * - `{ whiteList }` — install only these
     * - `{ whiteList, blackList }` — whiteList wins, blackList ignored
     * - `{ blackList }` — install all host globals except these
     * - omitted — install none (pure ECMA, `eval` stripped): minimum privilege
     *
     * Global names are `"console"`, `"crypto"`, `"fetch"`, `"url"`, `"timers"`,
     * `"microtask"`, `"abort"`, `"streams"`, `"blob"`, `"formData"`,
     * `"textEncoder"`, `"textDecoder"`, `"dom"`, `"base64"`,
     * `"structuredClone"`, `"process"` and `"Aike"`. Process/thread-global
     * APIs (e.g. `process.exit`, `Aike.onSignal`) are simply not installed in
     * a child runtime; `Aike.spawn({abortOnParentExit})` is rejected with a
     * `TypeError`.
     *
     * Modules are **not** capabilities: whether the child may import a
     * module is decided by {@link RuntimeOptions.importPolicy}.
     */
    global?: string[] | { whiteList?: string[]; blackList?: string[] };
    /**
     * Decides which modules the child runtime may import. Called with the
     * module's name (`"ai"`, `"sqlite"`) or, for a script file, its absolute
     * path; it must return a boolean synchronously.
     *
     * Omitted (or `undefined`) denies **every** import. The policy runs in
     * the creating runtime's context and must return synchronously; it must
     * not create or close runtimes (rejected with a `TypeError`).
     */
    importPolicy?: (name: string) => boolean;
    /** Optional debug name. */
    name?: string;
  }

  /**
   * A child runtime: an independent GC heap + context on the same thread,
   * cooperatively driven by the parent's event loop. Values crossing the
   * boundary must be serializable (functions/promises/symbols throw a
   * `TypeError`).
   */
  interface Runtime {
    /**
     * Execute `code` in the child (global scope) and resolve with its
     * serializable completion value. The child's uncaught exception rejects
     * with a serialized `{name, message, stack}` object; the parent loop is
     * unaffected. May be called repeatedly.
     */
    run(code: string, name?: string): Promise<any>;
    /** Send a serializable value to the child's `onmessage`. */
    postMessage(value: unknown): void;
    /** Receive values sent from the child via its `postMessage`. */
    onmessage: ((value: unknown) => void) | null;
    /** Abort and release the child runtime. Idempotent. */
    close(): void;
    /** Optional debug name (present only when set). */
    readonly name?: string;
  }

  /**
   * Create an isolated child runtime on the current thread. Only available in
   * the host/parent runtime — child runtimes never expose `createRuntime`.
   */
  function createRuntime(options?: RuntimeOptions): Runtime;
}

// ---------------------------------------------------------------------------
// ImportMeta  —  the import.meta object available in every ES module
// ---------------------------------------------------------------------------
// ES modules expose `import.meta`; `url` is the module's absolute file URL
// (e.g. "file:///abs/path/mod.ts"). aike resolves it against the base URL.
interface ImportMeta {
  url: string;
}
