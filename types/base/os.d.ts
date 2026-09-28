// Type declarations for the "os" module
// import * as os from "os";

declare module "os" {
  /** End-of-line sequence: "\r\n" on Windows, "\n" on POSIX. */
  const EOL: "\r\n" | "\n";

  /** Path to the null device: "\\.\nul" on Windows, "/dev/null" on POSIX. */
  const devNull: "\\\\.\\nul" | "/dev/null";

  /**
   * CPU architecture the binary was compiled for, Node-style. Returns the same
   * value as `process.arch`.
   */
  function arch():
    | "arm"
    | "arm64"
    | "ia32"
    | "loong64"
    | "mips"
    | "mipsel"
    | "ppc64"
    | "riscv64"
    | "s390x"
    | "x64";

  /**
   * Operating system platform, Node-style — same value as `process.platform`.
   *
   * The set is closed; do not widen it with `| string`, which would erase the
   * literal types.
   */
  function platform():
    | "win32"
    | "darwin"
    | "linux"
    | "android"
    | "freebsd"
    | "openbsd"
    | "netbsd"
    | "aix"
    | "sunos";

  /**
   * OS name as reported by the system (`uname(3)` sysname).
   * e.g. "Windows_NT", "Darwin", "Linux".
   */
  function type():
    | "Windows_NT"
    | "Darwin"
    | "Linux"
    | "Android"
    | "FreeBSD"
    | "OpenBSD"
    | "NetBSD"
    | "DragonFlyBSD"
    | "SunOS"
    | "AIX";

  /**
   * Kernel / OS version — **not** the aike runtime version (that is
   * `process.version`).
   *
   * Windows: the product name from the registry, e.g. "Windows 10 Pro".
   * POSIX: the `uname(3)` version field.
   */
  function version(): string;

  /**
   * Seconds since the machine booted — **not** the process uptime (that is
   * `process.uptime()`).
   */
  function uptime(): number;

  /** Byte order of the platform: "BE" (big endian) or "LE" (little endian). */
  function endianness(): "BE" | "LE";

  /**
   * Estimated parallelism available to the process — a hint for how many
   * workers to spawn. Never less than 1.
   */
  function availableParallelism(): number;

  /**
   * The current user's home directory path, or null if it cannot be determined.
   */
  function homedir(): string | null;

  /** The system temporary directory path. */
  function tmpdir(): string;

  /**
   * Operating-system-specific constants. Each group is defined only here —
   * `Aike` exposes no aliases.
   *
   * The `errno` group Node also defines is not implemented — aike does not
   * surface C errno values to JS.
   */
  const constants: {
    /**
     * Platform-available signal name → number mapping.
     * On Windows, only SIGINT, SIGABRT, SIGFPE, SIGILL, SIGSEGV, SIGTERM are present.
     * On Unix, all POSIX signals are available (including SIGQUIT, SIGUSR1, etc.).
     * Check `os.constants.signals.SIGUSR1 !== undefined` to detect platform support.
     */
    signals: {
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
     * Process scheduling priority levels.
     *
     * Windows: the `*_PRIORITY_CLASS` values. POSIX: `nice` values —
     * 19, 10, 0, -7, -14, -20 from low to highest.
     */
    priority: {
      PRIORITY_LOW: number;
      PRIORITY_BELOW_NORMAL: number;
      PRIORITY_NORMAL: number;
      PRIORITY_ABOVE_NORMAL: number;
      PRIORITY_HIGH: number;
      PRIORITY_HIGHEST: number;
    };

    /**
     * `dlopen(3)` binding flags. **Empty on Windows** — the object exists for
     * shape compatibility only.
     */
    dlopen: {
      RTLD_LAZY?: number;
      RTLD_NOW?: number;
      RTLD_GLOBAL?: number;
      RTLD_LOCAL?: number;
      /** glibc only; absent on macOS/BSD. */
      RTLD_DEEPBIND?: number;
    };
  };

  // --- aike: XDG base directories (aike 增强，非 Node) ---

  /**
   * Per-user config directory, or null if undeterminable.
   *
   * Linux: `$XDG_CONFIG_HOME` or `$HOME/.config`
   * macOS: `$HOME/Library/Preferences`
   * Windows: `%AppData%`
   */
  function configDir(): string | null;

  /**
   * Per-user data directory, or null if undeterminable.
   *
   * Linux: `$XDG_DATA_HOME` or `$HOME/.local/share`
   * macOS: `$HOME/Library/Application Support`
   * Windows: `%AppData%`
   */
  function dataDir(): string | null;

  /**
   * Per-user *local* data directory, or null if undeterminable. Differs from
   * {@link dataDir} on Windows only (`%LocalAppData%`), where it is the right
   * choice for machine-local caches and state. Same value as `dataDir()`
   * elsewhere.
   */
  function dataLocalDir(): string | null;

  /**
   * Per-user cache directory, or null if undeterminable.
   *
   * Linux: `$XDG_CACHE_HOME` or `$HOME/.cache`
   * macOS: `$HOME/Library/Caches`
   * Windows: `%LocalAppData%`
   */
  function cacheDir(): string | null;

  /**
   * Per-user state directory, or null if undeterminable. For data that should
   * persist but need not be backed up (logs, pid files, history).
   *
   * Linux: `$XDG_STATE_HOME` or `$HOME/.local/state`
   *
   * **null on Windows and macOS** — like {@link runtimeDir}, `$XDG_STATE_HOME`
   * is Linux-only. Use `dataLocalDir()` there.
   */
  function stateDir(): string | null;

  /**
   * Per-user runtime directory, or null if undeterminable. For non-persistent
   * runtime state such as sockets and pid files.
   *
   * **Always null on Windows and macOS** — this is defined by the XDG Base
   * Directory spec, which only Linux implements. Check for null before use.
   */
  function runtimeDir(): string | null;
}
