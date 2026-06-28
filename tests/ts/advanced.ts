// Advanced TypeScript features test suite.
import { check } from "./runner.ts";

export function test_type_guards(): void {
  function isString(v: any): v is string {
    return typeof v === "string";
  }
  check("type predicate: true", isString("hello") === true);
  check("type predicate: false", isString(42) === false);

  function maybe(v: string | null | undefined): string {
    if (v) return v.toUpperCase();
    return "empty";
  }
  check("truthiness: value", maybe("hi") === "HI");
  check("truthiness: null", maybe(null) === "empty");
  check("truthiness: undefined", maybe(undefined) === "empty");
}

export function test_type_assertions(): void {
  const v: any = "hello world";
  const len = (v as string).length;
  check("as assertion: length", len === 11);

  const s: any = "ts";
  const up = (s as string).toUpperCase();
  check("as assertion: upper", up === "TS");
}

export function test_unknown_any(): void {
  let anything: any = 42;
  check("any: number", anything.toFixed(2) === "42.00");
  anything = "hello";
  check("any: string", anything.length === 5);

  function safeLength(v: unknown): number {
    if (typeof v === "string") return v.length;
    if (v instanceof Array) return v.length;
    return 0;
  }
  check("unknown: string", safeLength("hi") === 2);
  check("unknown: array", safeLength([1, 2, 3]) === 3);
  check("unknown: number", safeLength(42) === 0);
}

// Using top-level await (module context) for async test
async function fetchJSON<T>(url: string, fallback: T): Promise<T> {
  return fallback;
}
const asyncResult: number[] = await fetchJSON<number[]>("/data", [1, 2, 3]);
check("async generics", asyncResult.length === 3 && asyncResult[0] === 1);

export function test_generators(): void {
  function* counter(max: number) {
    for (let i = 0; i < max; i++) yield i;
  }
  const values = [...counter(4)];
  check("generator: values", values[0] === 0 && values[3] === 3);
  check("generator: length", values.length === 4);
}

export function test_rest_spread(): void {
  function sumAll(...nums: number[]): number {
    return nums.reduce((a, b) => a + b, 0);
  }
  check("rest: sumAll", sumAll(1, 2, 3, 4) === 10);
  check("rest: single", sumAll(42) === 42);
  check("rest: empty", sumAll() === 0);

  const arr1: number[] = [1, 2];
  const arr2: number[] = [3, 4];
  const merged: number[] = [...arr1, ...arr2];
  check("spread: array", merged.length === 4 && merged[2] === 3);
}

export function test_optional_chaining(): void {
  interface Tree { left?: Tree; right?: Tree; value: number; }
  const t: Tree = { value: 1, left: { value: 2 } };
  check("optional chain: exists", t.left?.value === 2);
  check("optional chain: missing", t.right?.value === undefined);

  function getConfig(v: number | null | undefined): number {
    return v ?? 42;
  }
  check("nullish: null", getConfig(null) === 42);
  check("nullish: zero", getConfig(0) === 0);
  check("nullish: value", getConfig(99) === 99);
}
