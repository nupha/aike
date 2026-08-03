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

  // Read

  /** Read a file as a UTF-8 string. */
  function readText(path: string): Promise<string>;
  /** Read and parse a JSON file. */
  function readJSON<T = unknown>(path: string): Promise<T>;
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
  /** Replace oldText with replaceWith only when oldText appears exactly once. Resolves true on success, false otherwise. */
  function replaceTextUnique(
    path: string,
    oldText: string,
    replaceWith: string,
  ): Promise<boolean>;
  /** Write data to a file (auto-detect format: string, Uint8Array, or ArrayBuffer). */
  function write(
    path: string,
    data: string | Uint8Array | ArrayBuffer,
  ): Promise<void>;

  // Line counting

  /** Count all lines in a file (missing/unreadable file resolves to 0). */
  function countLines(path: string): Promise<number>;
  /** Count non-empty (trimmed) lines in a file (missing/unreadable file resolves to 0). */
  function countNonEmptyLines(path: string): Promise<number>;

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
  /** Remove a directory. Default recursive: false (empty dir only). Pass { recursive: true } to remove a directory tree. */
  function removeDir(
    path: string,
    options?: { recursive?: boolean },
  ): Promise<void>;

  // Remove

  /** Remove a file or symbolic link (POSIX unlink semantics). */
  function remove(path: string): Promise<void>;

  // Open

  /**
   * I/O handle for files and pipe read ends (child stdout/stderr).
   * Obtained via fs.open() or spawn().
   */
  export interface FileHandle {
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
    /** Write data asynchronously. */
    write(data: string | Uint8Array | ArrayBuffer): Promise<void>;
    /** Flush buffered data asynchronously. */
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

  /**
   * Open a file and return a FileHandle.
   * @param flags - "r"|"rb" read, "r+"|"r+b" read+write, "w"|"wb" write(truncate),
   *   "w+"|"w+b" read+write(truncate), "a"|"ab" append, "a+"|"a+b" read+append, "rw" read+write(create).
   *   Default: "r"
   */
  function open(path: string, flags?: string): Promise<FileHandle>;
}
