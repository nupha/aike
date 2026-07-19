// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

declare module "jsonl" {
  /**
   * A JSONL array: an ordered list of JS objects, one JSON object per line.
   * Operates on parsed JS values (not raw text).
   */
  export class JSONLArray<T = unknown> {
    constructor(values?: readonly T[]);

    /** Number of elements. */
    readonly length: number;

    /** Get element at `i` (negative = from end); returns fallback if absent. */
    get(i: number, fallback?: T): T | undefined;

    /** Append values, return new length. */
    push(...values: T[]): number;

    /** Remove/insert elements; returns removed as a new JSONLArray. */
    splice(start: number, deleteCount?: number, ...items: T[]): JSONLArray<T>;

    /** Shallow slice, returns a new JSONLArray. */
    slice(start?: number, end?: number): JSONLArray<T>;

    /** Iterate elements. */
    [Symbol.iterator](): IterableIterator<T>;

    /** Serialize to a JSONL string. */
    stringify(): string;

    /** Serialize to a JSONL string. */
    toString(): string;

    /** Return a plain array copy. */
    toJSON(): T[];
  }

  export interface JSONLParseOpts<T> {
    /** "skip" (default) | "throw" | "replace". */
    onError?: "skip" | "throw" | "replace";
    /** Replacement value when onError=="replace". */
    failsafe?: T;
  }

  /** Parse full JSONL text into a JSONLArray. */
  export function parse<T = unknown>(
    text: string,
    opts?: JSONLParseOpts<T>,
  ): JSONLArray<T>;

  /** Parse JSONL text; never throws (bad lines skipped/replaced). */
  export function tryParse<T = unknown>(
    text: string,
    opts?: JSONLParseOpts<T>,
  ): JSONLArray<T>;

  /** Read + parse a file; returns Promise<JSONLArray>. */
  export function readFromFile<T = unknown>(
    path: string,
    opts?: JSONLParseOpts<T>,
  ): Promise<JSONLArray<T>>;

  /** Append values to a file (one line each). */
  export function appendToFile(path: string, ...values: unknown[]): Promise<void>;

  /** Parse a complete chunk: returns { values, read, done, error }. */
  export function parseChunk(
    input: string | ArrayBuffer | Uint8Array,
    opts?: JSONLParseOpts,
  ): { values: JSONLArray; read: number; done: boolean; error: unknown };

  /** Async-iterable parse of a stream (aike fetch body / FileHandle). */
  export function parseStream<T = unknown>(
    stream: AsyncIterable<string | ArrayBuffer | Uint8Array>,
    opts?: JSONLParseOpts,
  ): AsyncIterable<T>;

  /** Serialize a JSONLArray or array to a JSONL string. */
  export function stringify(data: JSONLArray | unknown[]): string;
}
