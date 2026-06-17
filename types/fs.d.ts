// Type declarations for the "fs" module
// import * as fs from "fs";

declare module "fs" {
  interface Dirent {
    /** File/directory name. */
    name: string;
    /** Whether this entry is a regular file. */
    isFile: boolean;
    /** Whether this entry is a directory. */
    isDirectory: boolean;
    /** Whether this entry is a symbolic link. */
    isSymbolicLink: boolean;
    /** File size in bytes. */
    size: number;
  }

  interface FileStat {
    size: number;
    createdAt?: string;
    modifiedAt?: string;
    accessedAt?: string;
    isFile: boolean;
    isDirectory: boolean;
    isSymbolicLink: boolean;
    /** Unix file mode (only on Unix). */
    mode?: number;
    /** Unix UID (only on Unix). */
    uid?: number;
    /** Unix GID (only on Unix). */
    gid?: number;
  }

  // Sandbox

  /** Get the current sandbox path, or null if no sandbox is active. */
  function getSandbox(): string | null;
  /** Set or clear the sandbox path. Pass null to disable. */
  function setSandbox(path: string | null): void;

  // Read

  /** Read a file as a UTF-8 string. */
  function readText(path: string): Promise<string>;
  /** Read and parse a JSON file. */
  function readJSON(path: string): Promise<any>;
  /** Read a file as binary data (Uint8Array). */
  function readBytes(path: string): Promise<Uint8Array>;
  /** Check if a file or directory exists. */
  function exists(path: string): Promise<boolean>;
  /** Get file/directory metadata (stat). */
  function stat(path: string): Promise<FileStat>;
  /** Read the contents of a directory. */
  function readDir(path: string): Promise<Dirent[]>;

  // Write

  /** Write a UTF-8 string to a file (overwrites). */
  function writeText(path: string, data: string): Promise<void>;
  /** Write a JSON-serializable value to a file. Pass `space` to pretty-print. */
  function writeJSON(path: string, data: any, space?: number): Promise<void>;
  /** Write binary data (Uint8Array or ArrayBuffer) to a file. */
  function writeBytes(
    path: string,
    data: Uint8Array | ArrayBuffer,
  ): Promise<void>;
  /** Append a UTF-8 string to a file. Creates the file if it doesn't exist. */
  function appendText(path: string, data: string): Promise<void>;
  /** Write data to a file (auto-detect format: string, Uint8Array, or ArrayBuffer). */
  function write(
    path: string,
    data: string | Uint8Array | ArrayBuffer,
  ): Promise<void>;

  // Directory

  /** Ensure a directory exists. Creates it (and parents) if missing. Default recursive: true */
  function ensureDir(
    path: string,
    options?: { recursive?: boolean },
  ): Promise<void>;
  /** Create a directory. Default recursive: false */
  function makeDir(
    path: string,
    options?: { recursive?: boolean },
  ): Promise<void>;
  /** Remove an empty directory. */
  function removeDir(path: string): Promise<void>;

  // Remove

  /** Remove a file or symbolic link (POSIX unlink semantics). */
  function remove(path: string): Promise<void>;

  // Open

  /**
   * Open a file and return an IoHandle.
   * @param flags - "r"|"rb" read, "r+"|"r+b" read+write, "w"|"wb" write(truncate),
   *   "w+"|"w+b" read+write(truncate), "a"|"ab" append, "a+"|"a+b" read+append, "rw" read+write(create).
   *   Default: "r"
   */
  function open(path: string, flags?: string): Promise<IoHandle>;
}
