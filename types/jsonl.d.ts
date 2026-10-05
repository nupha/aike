declare module "jsonl" {
  // Options for tolerant parsing.
  interface JSONLParseOpts {
    onError?: "null" | "skip" | "throw";
  }

  /** Serialize an array (or single object) of T into a JSONL string (one element per line). */
  export function stringify(data: unknown[] | unknown): string;

  // --- Conditional return type on onError ---

  // onError = "null" (or default) → T | null
  export function parse<T = unknown>(
    text: string,
    opts?: JSONLParseOpts & { onError?: "null" },
  ): (T | null)[];

  // onError = "skip" | "throw" → T
  export function parse<T = unknown>(
    text: string,
    opts: JSONLParseOpts & { onError: "skip" | "throw" },
  ): T[];

  export function read<T = unknown>(
    path: string,
    opts?: JSONLParseOpts & { onError?: "null" },
  ): Promise<(T | null)[]>;
  export function read<T = unknown>(
    path: string,
    opts: JSONLParseOpts & { onError: "skip" | "throw" },
  ): Promise<T[]>;

  export function parseChunk<T = unknown>(
    text: string,
    opts?: JSONLParseOpts & { onError?: "null" },
  ): { values: (T | null)[]; attempted: number };
  export function parseChunk<T = unknown>(
    text: string,
    opts: JSONLParseOpts & { onError: "skip" | "throw" },
  ): { values: T[]; attempted: number };

  export function parseStream<T = unknown>(
    text: string,
    opts?: JSONLParseOpts & { onError?: "null" },
  ): { iterator: AsyncIterable<T | null>; lines: number };
  export function parseStream<T = unknown>(
    text: string,
    opts: JSONLParseOpts & { onError: "skip" | "throw" },
  ): { iterator: AsyncIterable<T>; lines: number };

  /** Append one or more items to a JSONL file. */
  export function append<T>(path: string, ...items: T[]): Promise<void>;

  /** Rewrite (truncate) a JSONL file with the given items. No items clears the file. */
  export function write(path: string, ...items: unknown[]): Promise<void>;
}
