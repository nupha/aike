// TypeScript basics test suite.
// Covers: type annotations, functions, interfaces, enums, generics, arrays, tuples.

import { ok, check } from "./runner.ts";

// ===================================================================
// 1. Function type annotations
// ===================================================================
export function test_basic_types(): void {
  check("double(4) === 8", (() => {
    function double(x: number): number { return x * 2; }
    return double(4) === 8;
  })());

  check("concat works", (() => {
    function concat(a: string, b: string): string { return a + b; }
    return concat("a", "b") === "ab";
  })());

  check("boolean param", (() => {
    function negate(v: boolean): boolean { return !v; }
    return negate(true) === false;
  })());

  check("void return", (() => {
    let called = false;
    function doNothing(): void { called = true; }
    const r = doNothing();
    return called && r === undefined;
  })());

  // never return type (function that throws)
  check("never return", (() => {
    function fail(msg: string): never { throw new Error(msg); }
    try {
      fail("boom");
      return false;
    } catch (e: any) {
      return e.message === "boom";
    }
  })());
}

// ===================================================================
// 2. Interface
// ===================================================================
export function test_interfaces(): void {
  interface Point { x: number; y: number; }

  function dist(p: Point): number {
    return Math.sqrt(p.x * p.x + p.y * p.y);
  }

  check("interface: dist(3,4) ≈ 5", Math.abs(dist({ x: 3, y: 4 }) - 5) < 0.001);
  check("interface: extra fields ok", dist({ x: 1, y: 2, z: 99 }) > 0);

  // Optional interface member
  interface Options { label: string; timeout?: number; }
  function run(opt: Options): string {
    return opt.timeout ? `${opt.label} (${opt.timeout}ms)` : opt.label;
  }
  check("interface optional: without", run({ label: "task" }) === "task");
  check("interface optional: with", run({ label: "task", timeout: 500 }) === "task (500ms)");

  // Readonly interface member
  interface Config { readonly id: string; name: string; }
  function getCfg(c: Config): string { return c.id; }
  check("interface readonly: access ok", getCfg({ id: "x1", name: "test" }) === "x1");

  // Interface extends
  interface Shape { color: string; }
  interface Circle extends Shape { radius: number; }
  function area(c: Circle): number {
    return 3.14159 * c.radius * c.radius;
  }
  const myCircle: Circle = { color: "red", radius: 2 };
  check("interface extends: area ≈ 12.566", Math.abs(area(myCircle) - 12.566) < 0.01);

  // Index signature
  interface Dict<T> { [key: string]: T; }
  function getVal(d: Dict<number>, k: string): number | undefined { return d[k]; }
  check("index signature", getVal({ a: 1, b: 2 }, "a") === 1);

  // Callable interface
  interface Adder { (a: number, b: number): number; }
  const myAdd: Adder = (a, b) => a + b;
  check("callable interface", myAdd(3, 4) === 7);
}

// ===================================================================
// 3. Enum
// ===================================================================
export function test_enums(): void {
  enum Color { Red, Green, Blue }
  check("enum: auto values", Color.Red === 0 && Color.Green === 1 && Color.Blue === 2);

  enum Status { Ok = 200, NotFound = 404, Error = 500 }
  check("enum: explicit values", Status.Ok === 200 && Status.NotFound === 404);
  check("enum: reverse lookup", Status[200] === "Ok");

  enum Fruit { Apple = "apple", Banana = "banana", Cherry = "cherry" }
  check("enum: string values", Fruit.Apple === "apple");

  // Const enum (inline values, no runtime object)
  const enum Direction { Up, Down, Left, Right }
  check("const enum: inline", Direction.Up === 0 && Direction.Right === 3);

  // Enum with mixed members
  enum Mixed { A, B = "hello", C = 99 }
  check("enum: mixed", Mixed.A === 0 && Mixed.B === "hello" && Mixed.C === 99);
}

// ===================================================================
// 4. Generics
// ===================================================================
export function test_generics(): void {
  function identity<T>(x: T): T { return x; }
  check("generics: identity<number>", identity(42) === 42);
  check("generics: identity<string>", identity("hi") === "hi");

  function first<T>(arr: T[]): T { return arr[0]; }
  check("generics: first number", first([10, 20]) === 10);
  check("generics: first string", first(["a", "b"]) === "a");

  function pair<T, U>(a: T, b: U): [T, U] { return [a, b]; }
  const p = pair(1, "two");
  check("generics: multi type", p[0] === 1 && p[1] === "two");

  // Generic with constraint
  function length<T extends { length: number }>(x: T): number { return x.length; }
  check("generics: constraint array", length([1, 2, 3]) === 3);
  check("generics: constraint string", length("abc") === 3);

  // Generic class
  class Box<T> {
    value: T;
    constructor(v: T) { this.value = v; }
    get(): T { return this.value; }
    set(v: T): void { this.value = v; }
  }
  const nb = new Box(42);
  check("generic class: get", nb.get() === 42);
  nb.set(99);
  check("generic class: set", nb.get() === 99);

  const sb = new Box("hello");
  check("generic class: string", sb.get() === "hello");

  // Generic default type
  function createArray<T = string>(length: number, value: T): T[] {
    return new Array(length).fill(value);
  }
  const arr = createArray(3, "x");
  check("generics: default type", arr.length === 3 && arr[0] === "x");
}

// ===================================================================
// 5. Union & Intersection types (runtime behavior)
// ===================================================================
export function test_unions(): void {
  function formatId(id: string | number): string {
    return typeof id === "string" ? id.toUpperCase() : `#${id}`;
  }
  check("union: string branch", formatId("abc") === "ABC");
  check("union: number branch", formatId(42) === "#42");

  function handle(v: string | number | boolean): string {
    if (typeof v === "boolean") return v ? "yes" : "no";
    if (typeof v === "number") return `n:${v}`;
    return v;
  }
  check("union: multi type bool", handle(true) === "yes");
  check("union: multi type num", handle(5) === "n:5");
  check("union: multi type str", handle("hi") === "hi");

  // Discriminated union
  type Shape2 =
    | { kind: "circle"; radius: number }
    | { kind: "rect"; width: number; height: number };

  function area2(s: Shape2): number {
    if (s.kind === "circle") return 3.14 * s.radius * s.radius;
    return s.width * s.height;
  }
  check("discriminated union: circle", Math.abs(area2({ kind: "circle", radius: 2 }) - 12.56) < 0.1);
  check("discriminated union: rect", area2({ kind: "rect", width: 3, height: 4 }) === 12);

  // Type narrowing with `in`
  type A = { a: number };
  type B = { b: string };
  function narrow(x: A | B): string {
    if ("a" in x) return `a=${x.a}`;
    return `b=${x.b}`;
  }
  check("narrow: 'in' operator", narrow({ a: 1 }) === "a=1");
}

// ===================================================================
// 6. Tuple types
// ===================================================================
export function test_tuples(): void {
  type Pair = [string, number];
  const t1: Pair = ["age", 30];
  check("tuple: access", t1[0] === "age" && t1[1] === 30);
  check("tuple: destructure", (([name, val]: Pair) => val)(t1) === 30);

  // Optional tuple element
  type OptTuple = [string, number?];
  const t2: OptTuple = ["hello"];
  check("tuple: optional", t2[0] === "hello" && t2[1] === undefined);

  // Rest tuple
  type RestTuple = [string, ...number[]];
  const t3: RestTuple = ["nums", 1, 2, 3];
  check("tuple: rest length", t3.length === 4);
  check("tuple: rest values", t3[1] === 1 && t3[3] === 3);

  // Labeled tuple
  type Range = [start: number, end: number];
  const r: Range = [0, 100];
  check("tuple: labeled access", r[0] === 0 && r[1] === 100);
}

// ===================================================================
// 7. Literal types & const assertions
// ===================================================================
export function test_literals(): void {
  type Direction2 = "north" | "south" | "east" | "west";
  function move(dir: Direction2): string { return `moving ${dir}`; }
  check("literal union: valid dir", move("north") === "moving north");

  // Numeric literal types
  type Dice = 1 | 2 | 3 | 4 | 5 | 6;
  function roll(d: Dice): number { return d; }
  check("numeric literal", roll(4) === 4);

  // Template literal types (stripped to runtime string)
  type EventName = `on${string}`;
  function handleEvent(e: EventName): string { return `handling ${e}`; }
  check("template literal type", handleEvent("onClick") === "handling onClick");
}

// ===================================================================
// 8. Keyof & typeof type queries
// ===================================================================
export function test_keyof_typeof(): void {
  const obj = { a: 1, b: "hello", c: true };
  type Keys = keyof typeof obj;
  // At runtime, Keys is just the string union, test the behavior
  function getProp(k: string): any { return (obj as any)[k]; }
  check("keyof + typeof", getProp("a") === 1 && getProp("b") === "hello");
}
