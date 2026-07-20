declare module "jsonl" {
  // Options for tolerant parsing.
  interface JSONLParseOpts<T> {
    onError?: "replace" | "skip" | "throw";
    failsafe?: T;
  }

  /** Parse a full JSONL string into an array of T. */
  export function parse<T = unknown>(
    text: string,
    opts?: JSONLParseOpts<T>,
  ): T[];

  /** Serialize an array (or single object) of T into a JSONL string (one element per line). */
  export function stringify(data: unknown[] | unknown): string;

  /** Read a JSONL file and resolve to an array of T. */
  export function read<T = unknown>(
    path: string,
    opts?: JSONLParseOpts<T>,
  ): Promise<T[]>;

  /** Append one or more items to a JSONL file. */
  export function append<T>(path: string, ...items: T[]): Promise<void>;

  /** Rewrite (truncate) a JSONL file with the given items. No items clears the file. */
  export function write(path: string, ...items: unknown[]): Promise<void>;

  /** Parse a chunk of JSONL, returning { values: T[], attempted: number }. */
  export function parseChunk<T = unknown>(
    text: string,
    opts?: JSONLParseOpts<T>,
  ): { values: T[]; attempted: number };

  /** Stream-parse a JSONL string, returning { iterator: AsyncIterable<T>, lines: number }. */
  export function parseStream<T = unknown>(
    text: string,
    opts?: JSONLParseOpts<T>,
  ): { iterator: AsyncIterable<T>; lines: number };
}
