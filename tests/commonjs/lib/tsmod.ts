// TypeScript loaded through `require()`: types are stripped, and because the
// file has no `import`/`export` it stays CommonJS.

type Label = string;

const label: Label = "ts-module";

interface Point {
    x: number;
    y: number;
}

const origin: Point = { x: 0, y: 0 };

function sum(a: number, b: number): number {
    return a + b;
}

module.exports = { label, sum, origin };
