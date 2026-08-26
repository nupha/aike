// Type declarations for the "sandbox" module

declare module "sandbox" {
  interface SandboxTmpfs {
    /** Path inside the sandbox. */
    path: string;
    /** Size in bytes (positive integer). */
    size: number;
  }

  /** "none" (default) | full host network | allowlist of domains. */
  type SandboxNetwork = "none" | "host" | { allow: string[] };

  /** Named profile or a custom syscall whitelist (Linux only). */
  type SandboxSeccomp =
    | "disabled"
    | "strict"
    | "standard"
    | "permissive"
    | string[];

  /** Mount options for one host source path. */
  interface MountOptions {
    /**
     * In-sandbox path. Omitted = mount at the same path as the source.
     * Path remapping only takes effect on Linux (bind mounts); macOS
     * profiles enforce permissions by host path and ignore this field.
     */
    target?: string;
    /** Default false (read-only). */
    writable?: boolean;
  }

  export interface SandboxOptions {
    /** Map of host source path → mount options (insertion order preserved). */
    mounts?: Record<string, MountOptions>;
    tmpfs?: SandboxTmpfs[];
    /**
     * Working directory of sandboxed processes. Required and validated:
     * must be an existing directory. It doubles as a write-whitelist entry
     * on macOS — nanosandbox's own default ("/") would whitelist the whole
     * filesystem, hence aike refuses to construct without it.
     * For throwaway runs that need no real workspace, pass a temp dir:
     * `new Sandbox({ cwd: Aike.tmpdir() })`.
     */
    cwd: string;
    rootfs?: string;
    /** Memory limit in bytes. */
    memoryLimit?: number;
    /** CPU limit, 0 < n <= core count; fractions allowed. */
    cpuLimit?: number;
    /** Wall-clock time limit in milliseconds. */
    wallTimeLimitMs?: number;
    /** CPU time limit in milliseconds. */
    cpuTimeLimitMs?: number;
    /** Maximum number of processes/threads. */
    maxPids?: number;
    /** Maximum file size in bytes. */
    maxFileSize?: number;
    /** Maximum number of open files. */
    maxOpenFiles?: number;
    network?: SandboxNetwork;
    seccompProfile?: SandboxSeccomp;
    uid?: number;
    gid?: number;
    /** Environment variables for sandboxed processes. */
    env?: Record<string, string>;
    hostname?: string;
  }

  export interface SandboxRunOptions {
    /** Data written to the child's stdin. */
    stdin?: string | Uint8Array;
  }

  export interface ExecutionResult {
    stdout: string;
    stderr: string;
    exitCode: number;
    durationMs: number;
    killedByTimeout: boolean;
    killedByOom: boolean;
    signal: number | null;
    peakMemory: number | null;
    cpuTimeMs: number | null;
  }

  /**
   * A configured sandbox for secure process execution.
   * Instances are reusable: each `run` spawns a fresh sandboxed process.
   */
  export class Sandbox {
    constructor(options: SandboxOptions);
    run(
      cmd: string,
      args?: string[],
      options?: SandboxRunOptions,
    ): Promise<ExecutionResult>;
  }

  /** Whether the current platform supports sandboxing. */
  export function isSupported(): boolean;

  /** Name of the current platform's sandbox implementation. */
  export function platformName(): string;
}
