// Type declarations for the "path" module
// import * as path from "path";

declare module "path" {
  interface ParsedPath {
    /** The root of the path (e.g. "/" or "C:\"). */
    root: string;
    /** The full directory path (excluding the file name). */
    dir: string;
    /** The file name including extension (e.g. "index.html"). */
    base: string;
    /** The file name without extension (e.g. "index"). */
    name: string;
    /** The file extension including the dot (e.g. ".html"). */
    ext: string;
  }

  /** Return the last portion of a path, similar to the Unix `basename` command. */
  function basename(p: string, ext?: string): string;
  /** Return the directory name of a path, similar to the Unix `dirname` command. */
  function dirname(p: string): string;
  /** Return the extension of the path, including the leading dot (e.g. ".html"). */
  function extname(p: string): string;
  /** Join all path segments together and normalize the result. */
  function join(...paths: string[]): string;
  /** Resolve a sequence of path segments into an absolute path. */
  function resolve(...paths: string[]): string;
  /** Parse a path string into its components. */
  function parse(p: string): ParsedPath;
  /** Build a path string from a ParsedPath-like object. */
  function format(pathObj: Partial<ParsedPath>): string;
  /** Normalize a path by resolving `.` and `..` segments. */
  function normalize(p: string): string;
  /** Compute the relative path from `from` to `to`. */
  function relative(from: string, to: string): string;
  /** Test whether a path is absolute. */
  function isAbsolute(p: string): boolean;

  /** Platform-specific path segment separator. "/" on POSIX, "\" on Windows. */
  const sep: string;
  /** Platform-specific environment variable delimiter. ":" on POSIX, ";" on Windows. */
  const delimiter: string;
}
