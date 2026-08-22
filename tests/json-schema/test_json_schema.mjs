/**
 * test_json_schema.mjs — verify the native json-schema module.
 *
 * Run: aike/target/debug/aike -m tests/json-schema/test_json_schema.mjs
 */
import JSONSchema from "json-schema";

let passed = 0;
let failed = 0;
function check(name, cond) {
  if (cond) {
    passed++;
    console.log(`  ok  - ${name}`);
  } else {
    failed++;
    console.error(`  FAIL- ${name}`);
  }
}

// ── extract ──

console.log("\n=== JSONSchema.extract ===");

// Stage 1: strict
check("extract strict: plain object", JSON.stringify(JSONSchema.extract('{"a":1}')) === '{"a":1}');
check("extract strict: array", JSON.stringify(JSONSchema.extract('[1,2,3]')) === '[1,2,3]');
check("extract strict: nested", JSON.stringify(JSONSchema.extract('{"x":{"y":true}}')) === '{"x":{"y":true}}');

// Stage 2: fenced
check("extract fenced: ```json block", JSON.stringify(JSONSchema.extract('some text\n```json\n{"a":1}\n```\nmore')) === '{"a":1}');
check("extract fenced: ``` block (no lang)", JSON.stringify(JSONSchema.extract('prefix\n```\n{"b":2}\n```\n')) === '{"b":2}');

// Stage 3: bare
check("extract bare: { in prose", JSON.stringify(JSONSchema.extract('here is {"c":3} inline')) === '{"c":3}');
check("extract bare: nested { }", JSON.stringify(JSONSchema.extract('text {"d":{"e":4}} end')) === '{"d":{"e":4}}');

// Failure
check("extract returns undefined for no JSON", JSONSchema.extract('no json here') === undefined);
check("extract returns undefined for empty", JSONSchema.extract('') === undefined);

// ── validate ──

console.log("\n=== JSONSchema.validate ===");

// basic type check
const r1 = JSONSchema.validate({ name: "alice", age: 30 }, {
  type: "object",
  properties: { name: { type: "string" }, age: { type: "number" } },
  required: ["name", "age"],
});
check("validate: valid object ok", r1.ok === true);
check("validate: valid object preserves value", r1.value.name === "alice" && r1.value.age === 30);

// missing required
const r2 = JSONSchema.validate({ name: "bob" }, {
  type: "object",
  properties: { name: { type: "string" }, age: { type: "number" } },
  required: ["name", "age"],
}, "fail");
check("validate fail: missing required", r2.ok === false);
check("validate fail: reports missing", r2.errors.some(e => e.includes("missing required")));

// coerce mode: string → number
const r3 = JSONSchema.validate({ x: "42" }, {
  type: "object",
  properties: { x: { type: "number" } },
}, "coerce");
check("validate coerce: string→number", r3.ok === true);
check("validate coerce: value is number", r3.value.x === 42);

// coerce mode: number → string
const r4 = JSONSchema.validate({ x: 42 }, {
  type: "object",
  properties: { x: { type: "string" } },
}, "coerce");
check("validate coerce: number→string", r4.ok === true);
check("validate coerce: value is string", r4.value.x === "42");

// coerce mode: string → boolean
const r5 = JSONSchema.validate({ flag: "true" }, {
  type: "object",
  properties: { flag: { type: "boolean" } },
}, "coerce");
check("validate coerce: string→boolean", r5.ok === true);
check("validate coerce: value is boolean true", r5.value.flag === true);

// array validation
const r6 = JSONSchema.validate([1, 2, "three"], {
  type: "array",
  items: { type: "number" },
}, "fail");
check("validate fail: array item type mismatch", r6.ok === false);
check("validate fail: reports array item error", r6.errors.some(e => e.includes("[2]")));

// coerce array items
const r7 = JSONSchema.validate(["1", "2", "3"], {
  type: "array",
  items: { type: "integer" },
}, "coerce");
check("validate coerce: array string→int", r7.ok === true);
check("validate coerce: array values are ints", JSON.stringify(r7.value) === '[1,2,3]');

// integer validation
const r8 = JSONSchema.validate({ n: 3.5 }, {
  type: "object",
  properties: { n: { type: "integer" } },
}, "fail");
check("validate fail: non-integer rejected", r8.ok === false);
check("validate fail: reports integer error", r8.errors.some(e => e.includes("integer")));

// best-effort mode (same as fail, no coercion)
const r9 = JSONSchema.validate({ x: "hello" }, {
  type: "object",
  properties: { x: { type: "number" } },
}, "best-effort");
check("validate best-effort: reports error", r9.ok === false);
check("validate best-effort: no coercion", r9.value.x === "hello");

// ── forExample ──

console.log("\n=== JSONSchema.forExample ===");

const ex1 = JSONSchema.forExample({
  type: "object",
  properties: {
    name: { type: "string" },
    count: { type: "number" },
    active: { type: "boolean" },
    tags: { type: "array", items: { type: "string" } },
  },
});
check("forExample: object has all keys", "name" in ex1 && "count" in ex1 && "active" in ex1 && "tags" in ex1);
check("forExample: string is 'string'", ex1.name === "string");
check("forExample: number is 0", ex1.count === 0);
check("forExample: boolean is true", ex1.active === true);
check("forExample: array has one item", ex1.tags.length === 1 && ex1.tags[0] === "string");

const ex2 = JSONSchema.forExample({ example: { custom: "value" } });
check("forExample: uses explicit example", JSON.stringify(ex2) === '{"custom":"value"}');

// ── matchesType ──

console.log("\n=== JSONSchema.matchesType ===");

check("matchesType: string", JSONSchema.matchesType("hello", "string") === true);
check("matchesType: number", JSONSchema.matchesType(42, "number") === true);
check("matchesType: integer", JSONSchema.matchesType(42, "integer") === true);
check("matchesType: integer rejects float", JSONSchema.matchesType(3.5, "integer") === false);
check("matchesType: boolean", JSONSchema.matchesType(true, "boolean") === true);
check("matchesType: null", JSONSchema.matchesType(null, "null") === true);
check("matchesType: array", JSONSchema.matchesType([1], "array") === true);
check("matchesType: object", JSONSchema.matchesType({ a: 1 }, "object") === true);

// array of types
check("matchesType: array types [string,null]", JSONSchema.matchesType("hi", ["string", "null"]) === true);
check("matchesType: array types rejects", JSONSchema.matchesType(42, ["string", "null"]) === false);

// unknown type → accept
check("matchesType: unknown type accepts", JSONSchema.matchesType("anything", "foobar") === true);

// ── summary ──

console.log(`\n--- json-schema: ${passed} passed, ${failed} failed ---`);
process.exit(failed > 0 ? 1 : 0);
