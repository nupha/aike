/// <reference path="./process.d.ts" />
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

  /** Read a file as a UTF-8 string. `offset` (byte) + `length` (max bytes) slice a range; throws if the range is not valid UTF-8. */
  function readText(
    path: string,
    options?: { offset?: number; length?: number },
  ): Promise<string>;
  /** Read and parse a JSON file. */
  function readJSON<T = unknown>(path: string): Promise<T>;
  /** Read a file as binary data (Uint8Array). `offset` (byte) + `length` (max bytes) slice a range. */
  function readBytes(
    path: string,
    options?: { offset?: number; length?: number },
  ): Promise<Uint8Array>;
  /** Check if a file or directory exists. */
  function exists(path: string): Promise<boolean>;
  /** Get file/directory metadata (stat). */
  function stat(path: string): Promise<FileStat>;
  /** Read the contents of a directory. */
  function readDir(path: string): Promise<Dirent[]>;
  /** Canonicalize a path, resolving all symlinks. Throws if the path does not exist. */
  function realpath(path: string): Promise<string>;
  /** SHA-256 hash of a file's contents, returned as a lowercase hex string. */
  function sha256(path: string): Promise<string>;

  /** Match files/directories against a glob pattern. Returns matching paths, relative to `cwd` unless `absolute`. */
  function glob(
    pattern: string,
    options?: {
      /** Base directory for relative patterns. Default: process cwd. */
      cwd?: string;
      /** Patterns whose matches are excluded from the result. */
      exclude?: string[];
      /** Include hidden files/dirs (names starting with `.`). Default: false. */
      dot?: boolean;
      /** Match case-sensitively. Default: true. */
      caseSensitive?: boolean;
      /** Return absolute paths instead of cwd-relative ones. Default: false. */
      absolute?: boolean;
    },
  ): Promise<string[]>;

  /** A single grep match (one matching line). */
  interface GrepHit {
    /** File path (relative to `cwd` unless `absolute`). */
    file: string;
    /** 1-based line number of the match. */
    lineNumber: number;
    /** Full text of the matching line. */
    line: string;
    /** Byte offset of the first match's start within `line`. */
    start: number;
    /** Byte offset of the first match's end within `line`. */
    end: number;
    /** Every match on the line, each as `{ start, end }` byte offsets within `line`. */
    ranges: { start: number; end: number }[];
  }

  /** A per-file match count (for `mode: "count"`). */
  interface GrepCount {
    /** File path (relative to `cwd` unless `absolute`). */
    file: string;
    /** Number of retained lines in this file. */
    count: number;
  }

  /**
   * Search file contents for a regex `pattern` (ripgrep-style), traversing a
   * directory tree. Honors `.gitignore`/`.ignore`/hidden rules by default.
   *
   * Result shape depends on `mode`:
   * - `"content"` (default): `GrepHit[]` — one entry per retained line.
   * - `"files"`: `string[]` — distinct files with at least one retained line
   *   (`-l`). With `invert`, files with NO match (`--files-without-match`).
   * - `"count"`: `GrepCount[]` — retained-line count per file (`-c`). With
   *   `invert`, the count of non-matching lines.
   *
   * `invert` (`-v`) keeps lines that do NOT contain the pattern; it is
   * orthogonal to `mode`, so the two combine freely.
   *
   * Throws SyntaxError if `pattern` is not a valid regex.
   */
  function grep(
    pattern: string,
    options?: {
      /** Base directory for the search. Default: process cwd. */
      cwd?: string;
      /** Comma-separated glob(s) restricting which files to search. Default: "**\/\*" (all files). */
      paths?: string;
      /** Only search files matching these glob(s). */
      include?: string[];
      /** Skip files matching these glob(s). */
      exclude?: string[];
      /** Match case-sensitively. Default: false (case-insensitive). */
      caseSensitive?: boolean;
      /** Include hidden files/dirs (names starting with `.`). Default: false. */
      dot?: boolean;
      /** Honor `.gitignore`/`.ignore`/hidden rules. Default: true. */
      respectIgnore?: boolean;
      /** Return absolute file paths instead of cwd-relative ones. Default: false. */
      absolute?: boolean;
      /** Maximum number of retained lines to return (content mode). Default: 1000. */
      maxResults?: number;
      /** Result shape: `"content"` (default), `"files"` (-l), `"count"` (-c). */
      mode?: "content" | "files" | "count";
      /** Keep lines that do NOT match (grep `-v`). Default: false. */
      invert?: boolean;
    },
  ): Promise<GrepHit[] | string[] | GrepCount[]>;

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
  /** Replace oldText with replaceWith only when oldText appears exactly once. Resolves to 1 on success, 0 otherwise. */
  function replaceTextUnique(
    path: string,
    oldText: string,
    replaceWith: string,
  ): Promise<number>;
  /** Replace every occurrence of oldText with replaceWith (left-to-right, non-overlapping). Resolves to the count of replacements made (0 if none). */
  function replaceTextAll(
    path: string,
    oldText: string,
    replaceWith: string,
  ): Promise<number>;
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
