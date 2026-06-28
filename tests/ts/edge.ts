// Edge cases test suite.
import { check } from "./runner.ts";

export function test_as_const(): void {
  const roles = ["admin", "user", "guest"] as const;
  check("as const: array length", roles.length === 3);
  check("as const: array access", roles[1] === "user");
  const config = { host: "localhost", port: 8080 } as const;
  check("as const: object", config.host === "localhost" && config.port === 8080);
}

export function test_definite_assignment(): void {
  class WithDefinite {
    name!: string;
    age!: number;
    constructor() {}
    init(): void { this.name = "test"; this.age = 10; }
  }
  const d = new WithDefinite();
  d.init();
  check("definite assignment", d.name === "test" && d.age === 10);
}

export function test_symbol(): void {
  const sym1: symbol = Symbol("key");
  const sym2: symbol = Symbol("key");
  check("symbol: unique", sym1 !== sym2);
}

export function test_arrow_complex(): void {
  const map: <T, U>(arr: T[], fn: (x: T) => U) => U[] = (arr, fn) => arr.map(fn);
  const doubled: number[] = map([1, 2, 3], (x) => x * 2);
  check("arrow complex", doubled[0] === 2 && doubled[2] === 6);
}

export function test_indexed_access(): void {
  interface Person { name: string; age: number; }
  const val: Person["age"] = 25;
  check("indexed access", val === 25);
}
