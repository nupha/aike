// Shared helper module for TypeScript test suite.
// Tests import transpilation of .ts modules with various export patterns.

export interface GreeterConfig {
  prefix: string;
  suffix: string;
}

export function greet(name: string, config: GreeterConfig): string {
  return `${config.prefix}${name}${config.suffix}`;
}

export const PI: number = 3.14159;
export const VERSION: string = "2.0.0";

export function add<T extends number>(a: T, b: T): number {
  return a + b;
}

export class HelperCounter {
  value: number;
  constructor(init: number = 0) {
    this.value = init;
  }
  tick(by: number = 1): number {
    this.value += by;
    return this.value;
  }
}

export type StatusCode = number;

export function isPositive(n: number): n is number {
  return n > 0;
}

export function createPair<T, U>(a: T, b: U): [T, U] {
  return [a, b];
}
