/**
 * json-schema.mjs — comprehensive tests for the native json-schema module.
 *
 * Covers extract, validate, forExample, and matchesType across
 * happy paths, edge cases, error conditions, and coercion logic.
 *
 * Run: aike/target/debug/aike -m tests/json-schema.mjs
 */
import JSONSchema from "json-schema";

let passed = 0;
let failed = 0;

function check(name, cond) {
  if (cond) {
    passed++;
  } else {
    failed++;
    console.error(`  FAIL  ${name}`);
  }
}

function eq(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

// ═══════════════════════════════════════════════════════════════════════════════
// extract
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── extract ──");

// ── Stage 1: strict ──
check("strict: plain object", eq(JSONSchema.extractJSON('{"a":1}'), { a: 1 }));
check("strict: array", eq(JSONSchema.extractJSON("[1,2,3]"), [1, 2, 3]));
check(
  "strict: nested object",
  eq(JSONSchema.extractJSON('{"x":{"y":true}}'), { x: { y: true } }),
);
check("strict: string value", JSONSchema.extractJSON('"hello"') === "hello");
check("strict: number value", JSONSchema.extractJSON("42") === 42);
check("strict: float value", JSONSchema.extractJSON("3.14") === 3.14);
check("strict: boolean true", JSONSchema.extractJSON("true") === true);
check("strict: boolean false", JSONSchema.extractJSON("false") === false);
check("strict: null", JSONSchema.extractJSON("null") === null);
check(
  "strict: nested array",
  eq(JSONSchema.extractJSON('[{"a":1},[2,3]]'), [{ a: 1 }, [2, 3]]),
);
check(
  "strict: object with spaces",
  eq(JSONSchema.extractJSON('  { "a" : 1 }  '), { a: 1 }),
);
check(
  "strict: deeply nested",
  eq(JSONSchema.extractJSON('{"a":{"b":{"c":{"d":4}}}}'), {
    a: { b: { c: { d: 4 } } },
  }),
);

// ── Stage 2: fenced ──
check(
  "fenced: ```json block",
  eq(JSONSchema.extractJSON('text\n```json\n{"a":1}\n```\nmore'), { a: 1 }),
);
check(
  "fenced: ``` block (no lang tag)",
  eq(JSONSchema.extractJSON('prefix\n```\n{"b":2}\n```\n'), { b: 2 }),
);
check(
  "fenced: array inside",
  eq(JSONSchema.extractJSON("before\n```json\n[1,2]\n```\nafter"), [1, 2]),
);
check(
  "fenced: multiple blocks, uses first",
  eq(
    JSONSchema.extractJSON(
      '```json\n{"first":1}\n```\n```json\n{"second":2}\n```',
    ),
    { first: 1 },
  ),
);
check(
  "fenced: nested object inside",
  eq(JSONSchema.extractJSON('```json\n{"x":{"y":"z"}}\n```'), {
    x: { y: "z" },
  }),
);

// ── Stage 3: bare ──
check(
  "bare: { in prose",
  eq(JSONSchema.extractJSON('here is {"c":3} inline'), { c: 3 }),
);
check(
  "bare: nested braces",
  eq(JSONSchema.extractJSON('text {"d":{"e":4}} end'), { d: { e: 4 } }),
);
check(
  "bare: only first object",
  eq(JSONSchema.extractJSON('{"a":1} and {"b":2}'), { a: 1 }),
);

// ── Failure cases ──
check(
  "returns undefined for no JSON",
  JSONSchema.extractJSON("no json here") === undefined,
);
check(
  "returns undefined for empty string",
  JSONSchema.extractJSON("") === undefined,
);
check(
  "returns undefined for whitespace only",
  JSONSchema.extractJSON("   \n\t  ") === undefined,
);
check(
  "returns undefined for plain text",
  JSONSchema.extractJSON("hello world") === undefined,
);
check(
  "returns undefined for malformed JSON",
  JSONSchema.extractJSON("{invalid}") === undefined,
);
// unclosed fence → falls back to bare {} extraction
check(
  "unclosed fence: falls back to bare {}",
  eq(JSONSchema.extractJSON('```json\n{"a":1}\n'), { a: 1 }),
);

// ── Edge cases ──
check(
  "unicode in string",
  JSONSchema.extractJSON('{"msg":"héllo wörld"}').msg === "héllo wörld",
);
check(
  "escaped quotes",
  JSONSchema.extractJSON('{"msg":"say \\"hi\\""}').msg === 'say "hi"',
);
check(
  "fenced with blank line inside",
  eq(JSONSchema.extractJSON('```\n{"a":1}\n\n```'), { a: 1 }),
);
check(
  "large object",
  eq(JSONSchema.extractJSON(`{"${"k".repeat(200)}": "${"v".repeat(500)}"}`), {
    ["k".repeat(200)]: "v".repeat(500),
  }),
);

// ═══════════════════════════════════════════════════════════════════════════════
// validate
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── validate ──");

// ── happy path: valid object ──
const v1 = JSONSchema.validateSchema(
  { name: "alice", age: 30 },
  {
    type: "object",
    properties: { name: { type: "string" }, age: { type: "number" } },
    required: ["name", "age"],
  },
);
check("valid object: ok", v1.ok === true);
check(
  "valid object: value preserved",
  v1.value.name === "alice" && v1.value.age === 30,
);
check("valid object: no errors", v1.errors.length === 0);

// ── valid array ──
const v2 = JSONSchema.validateSchema([1, 2, 3], {
  type: "array",
  items: { type: "number" },
});
check("valid array: ok", v2.ok === true);
check("valid array: values preserved", eq(v2.value, [1, 2, 3]));

// ── missing required property ──
const v3 = JSONSchema.validateSchema(
  { name: "bob" },
  {
    type: "object",
    properties: { name: { type: "string" }, age: { type: "number" } },
    required: ["name", "age"],
  },
  "fail",
);
check("missing required: not ok", v3.ok === false);
check(
  "missing required: reports error",
  v3.errors.some((e) => e.includes("missing required")),
);

// ── type mismatch: string expected, got number ──
const v4 = JSONSchema.validateSchema(
  { x: 42 },
  {
    type: "object",
    properties: { x: { type: "string" } },
  },
  "fail",
);
check("type mismatch fail: not ok", v4.ok === false);
check(
  "type mismatch fail: reports expected string",
  v4.errors.some((e) => e.includes("expected string")),
);

// ── type mismatch: number expected, got string ──
const v5 = JSONSchema.validateSchema(
  { x: "hello" },
  {
    type: "object",
    properties: { x: { type: "number" } },
  },
  "fail",
);
check("type mismatch fail num: not ok", v5.ok === false);

// ── integer rejects float ──
const v6 = JSONSchema.validateSchema(
  { n: 3.5 },
  {
    type: "object",
    properties: { n: { type: "integer" } },
  },
  "fail",
);
check("integer: rejects float", v6.ok === false);
check(
  "integer: reports error",
  v6.errors.some((e) => e.includes("integer")),
);

// ── integer accepts whole number ──
const v7 = JSONSchema.validateSchema(
  { n: 42 },
  {
    type: "object",
    properties: { n: { type: "integer" } },
  },
  "fail",
);
check("integer: accepts whole number", v7.ok === true);

// ── boolean type ──
const v8 = JSONSchema.validateSchema(
  { flag: true },
  {
    type: "object",
    properties: { flag: { type: "boolean" } },
  },
  "fail",
);
check("boolean: valid", v8.ok === true);

const v8b = JSONSchema.validateSchema(
  { flag: "yes" },
  {
    type: "object",
    properties: { flag: { type: "boolean" } },
  },
  "fail",
);
check("boolean: rejects string", v8b.ok === false);

// ── null type ──
const v9 = JSONSchema.validateSchema(
  { val: null },
  {
    type: "object",
    properties: { val: { type: "null" } },
  },
  "fail",
);
check("null: valid", v9.ok === true);

const v9b = JSONSchema.validateSchema(
  { val: 0 },
  {
    type: "object",
    properties: { val: { type: "null" } },
  },
  "fail",
);
check("null: rejects number", v9b.ok === false);

// ── nested object ──
const v10 = JSONSchema.validateSchema(
  { user: { name: "alice", address: { city: "NYC" } } },
  {
    type: "object",
    properties: {
      user: {
        type: "object",
        properties: {
          name: { type: "string" },
          address: { type: "object", properties: { city: { type: "string" } } },
        },
      },
    },
  },
  "fail",
);
check("nested object: valid", v10.ok === true);

const v10b = JSONSchema.validateSchema(
  { user: { name: "alice", address: { city: 123 } } },
  {
    type: "object",
    properties: {
      user: {
        type: "object",
        properties: {
          name: { type: "string" },
          address: { type: "object", properties: { city: { type: "string" } } },
        },
      },
    },
  },
  "fail",
);
check("nested object: mismatch", v10b.ok === false);
check(
  "nested object: error path",
  v10b.errors.some((e) => e.includes("user.address.city")),
);

// ── array items validation ──
const v11 = JSONSchema.validateSchema(
  ["a", "b", "c"],
  { type: "array", items: { type: "string" } },
  "fail",
);
check("array items: valid", v11.ok === true);

const v11b = JSONSchema.validateSchema(
  [1, "two", 3],
  { type: "array", items: { type: "string" } },
  "fail",
);
check("array items: mismatch", v11b.ok === false);
check(
  "array items: error path [0] and [2]",
  v11b.errors.some((e) => e.includes("[0]")) &&
    v11b.errors.some((e) => e.includes("[2]")),
);

// ── coerce: string → number ──
const v12 = JSONSchema.validateSchema(
  { x: "42" },
  {
    type: "object",
    properties: { x: { type: "number" } },
  },
  "coerce",
);
check("coerce str→num: ok", v12.ok === true);
check("coerce str→num: value is number", v12.value.x === 42);
check("coerce str→num: type", typeof v12.value.x === "number");

// ── coerce: number → string ──
const v13 = JSONSchema.validateSchema(
  { x: 42 },
  {
    type: "object",
    properties: { x: { type: "string" } },
  },
  "coerce",
);
check("coerce num→str: ok", v13.ok === true);
check("coerce num→str: value is string", v13.value.x === "42");

// ── coerce: boolean → string ──
const v14 = JSONSchema.validateSchema(
  { x: true },
  {
    type: "object",
    properties: { x: { type: "string" } },
  },
  "coerce",
);
check("coerce bool→str: ok", v14.ok === true);
check("coerce bool→str: value", v14.value.x === "true");

// ── coerce: string → boolean ──
const v15 = JSONSchema.validateSchema(
  { flag: "true" },
  {
    type: "object",
    properties: { flag: { type: "boolean" } },
  },
  "coerce",
);
check("coerce str→bool true: ok", v15.ok === true);
check("coerce str→bool true: value", v15.value.flag === true);

const v15b = JSONSchema.validateSchema(
  { flag: "false" },
  {
    type: "object",
    properties: { flag: { type: "boolean" } },
  },
  "coerce",
);
check("coerce str→bool false: value", v15b.value.flag === false);

// ── coerce: string → integer ──
const v16 = JSONSchema.validateSchema(
  { n: "7" },
  {
    type: "object",
    properties: { n: { type: "integer" } },
  },
  "coerce",
);
check("coerce str→int: ok", v16.ok === true);
check("coerce str→int: value", v16.value.n === 7);

// ── coerce: non-numeric string → number fails ──
const v17 = JSONSchema.validateSchema(
  { x: "abc" },
  {
    type: "object",
    properties: { x: { type: "number" } },
  },
  "coerce",
);
check("coerce non-numeric→num: not ok", v17.ok === false);

// ── coerce: non-boolean string → boolean fails ──
const v17b = JSONSchema.validateSchema(
  { x: "yes" },
  {
    type: "object",
    properties: { x: { type: "boolean" } },
  },
  "coerce",
);
check("coerce non-bool-string→bool: not ok", v17b.ok === false);

// ── coerce array items ──
const v18 = JSONSchema.validateSchema(
  ["1", "2", "3"],
  {
    type: "array",
    items: { type: "integer" },
  },
  "coerce",
);
check("coerce array items: ok", v18.ok === true);
check("coerce array items: values", eq(v18.value, [1, 2, 3]));

// ── coerce: float string → integer fails ──
const v19 = JSONSchema.validateSchema(
  { n: "3.5" },
  {
    type: "object",
    properties: { n: { type: "integer" } },
  },
  "coerce",
);
check("coerce float str→int: not ok", v19.ok === false);

// ── best-effort mode (same as fail, no coercion) ──
const v20 = JSONSchema.validateSchema(
  { x: "hello" },
  {
    type: "object",
    properties: { x: { type: "number" } },
  },
  "best-effort",
);
check("best-effort: reports error", v20.ok === false);
check("best-effort: no coercion", v20.value.x === "hello");

// ── best-effort: valid passes ──
const v21 = JSONSchema.validateSchema(
  { x: 42 },
  {
    type: "object",
    properties: { x: { type: "number" } },
  },
  "best-effort",
);
check("best-effort: valid passes", v21.ok === true);

// ── no type specified (accepts anything) ──
const v22 = JSONSchema.validateSchema(
  { x: "anything" },
  {
    type: "object",
    properties: { x: {} },
  },
  "fail",
);
check("no type: accepts anything", v22.ok === true);

// ── unknown type keyword (accepts) ──
const v23 = JSONSchema.validateSchema(
  { x: "hello" },
  {
    type: "object",
    properties: { x: { type: "custom_type" } },
  },
  "fail",
);
check("unknown type keyword: accepts", v23.ok === true);

// ── coerce preserves valid values ──
const v24 = JSONSchema.validateSchema(
  { a: "hello", b: 42, c: true },
  {
    type: "object",
    properties: {
      a: { type: "string" },
      b: { type: "number" },
      c: { type: "boolean" },
    },
  },
  "coerce",
);
check("coerce preserves valid: ok", v24.ok === true);
check(
  "coerce preserves valid: values",
  v24.value.a === "hello" && v24.value.b === 42 && v24.value.c === true,
);

// ── validate: wrong top-level type ──
const v25 = JSONSchema.validateSchema(
  "not an object",
  { type: "object" },
  "fail",
);
check("wrong top-level type: not ok", v25.ok === false);
check(
  "wrong top-level type: reports error",
  v25.errors.some((e) => e.includes("expected object")),
);

const v26 = JSONSchema.validateSchema(
  "not an array",
  { type: "array" },
  "fail",
);
check("wrong type array: not ok", v26.ok === false);
check(
  "wrong type array: reports error",
  v26.errors.some((e) => e.includes("expected array")),
);

// ── empty object against schema with required ──
const v27 = JSONSchema.validateSchema(
  {},
  {
    type: "object",
    properties: { x: { type: "string" } },
    required: ["x"],
  },
  "fail",
);
check("empty object with required: not ok", v27.ok === false);
check(
  "empty object with required: missing x",
  v27.errors.some((e) => e.includes("x") && e.includes("missing")),
);

// ── schema without properties (no recursion) ──
const v28 = JSONSchema.validateSchema(
  { extra: true },
  { type: "object" },
  "fail",
);
check("no properties schema: ok", v28.ok === true);

// ═══════════════════════════════════════════════════════════════════════════════
// forExample
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── forExample ──");

const e1 = JSONSchema.forExample({
  type: "object",
  properties: {
    name: { type: "string" },
    count: { type: "number" },
    active: { type: "boolean" },
    tags: { type: "array", items: { type: "string" } },
    nothing: { type: "null" },
  },
});
check(
  "object: has all keys",
  "name" in e1 &&
    "count" in e1 &&
    "active" in e1 &&
    "tags" in e1 &&
    "nothing" in e1,
);
check("object: string → 'string'", e1.name === "string");
check("object: number → 0", e1.count === 0);
check("object: boolean → true", e1.active === true);
check("object: null → null", e1.nothing === null);
check(
  "object: array → [item]",
  Array.isArray(e1.tags) && e1.tags.length === 1 && e1.tags[0] === "string",
);

const e2 = JSONSchema.forExample({ type: "string" });
check("string type → 'string'", e2 === "string");

const e3 = JSONSchema.forExample({ type: "number" });
check("number type → 0", e3 === 0);

const e4 = JSONSchema.forExample({ type: "integer" });
check("integer type → 0", e4 === 0);

const e5 = JSONSchema.forExample({ type: "boolean" });
check("boolean type → true", e5 === true);

const e6 = JSONSchema.forExample({ type: "null" });
check("null type → null", e6 === null);

const e7 = JSONSchema.forExample({ type: "array", items: { type: "number" } });
check("array type → [0]", eq(e7, [0]));

const e8 = JSONSchema.forExample({ type: "array" });
check("array no items → []", eq(e8, []));

const e9 = JSONSchema.forExample({ example: { custom: "value" } });
check("explicit example used", eq(e9, { custom: "value" }));

const e10 = JSONSchema.forExample({
  type: "object",
  properties: {
    user: {
      type: "object",
      properties: { name: { type: "string" } },
    },
  },
});
check("nested object", eq(e10, { user: { name: "string" } }));

const e11 = JSONSchema.forExample({ properties: { a: { type: "string" } } });
check("no type but has properties → infer object", e11.a === "string");

const e12 = JSONSchema.forExample(null);
check("null schema → null", e12 === null);

const e13 = JSONSchema.forExample({});
check("empty schema → null", e13 === null);

// ═══════════════════════════════════════════════════════════════════════════════
// matchesType
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── matchesType ──");

// string
check(
  "string: matches string",
  JSONSchema.matchesType("hello", "string") === true,
);
check("string: rejects number", JSONSchema.matchesType(42, "string") === false);
check("string: rejects null", JSONSchema.matchesType(null, "string") === false);

// number
check("number: matches int", JSONSchema.matchesType(42, "number") === true);
check("number: matches float", JSONSchema.matchesType(3.14, "number") === true);
check(
  "number: rejects string",
  JSONSchema.matchesType("42", "number") === false,
);
check("number: rejects bool", JSONSchema.matchesType(true, "number") === false);

// integer
check("integer: matches 42", JSONSchema.matchesType(42, "integer") === true);
check("integer: rejects 3.5", JSONSchema.matchesType(3.5, "integer") === false);
check(
  "integer: rejects string",
  JSONSchema.matchesType("42", "integer") === false,
);

// boolean
check(
  "boolean: matches true",
  JSONSchema.matchesType(true, "boolean") === true,
);
check(
  "boolean: matches false",
  JSONSchema.matchesType(false, "boolean") === true,
);
check("boolean: rejects 1", JSONSchema.matchesType(1, "boolean") === false);
check(
  "boolean: rejects 'true'",
  JSONSchema.matchesType("true", "boolean") === false,
);

// null
check("null: matches null", JSONSchema.matchesType(null, "null") === true);
check("null: rejects 0", JSONSchema.matchesType(0, "null") === false);
check("null: rejects false", JSONSchema.matchesType(false, "null") === false);

// object
check(
  "object: matches plain obj",
  JSONSchema.matchesType({ a: 1 }, "object") === true,
);
check("object: rejects array", JSONSchema.matchesType([1], "object") === false);
check("object: rejects null", JSONSchema.matchesType(null, "object") === false);
check(
  "object: rejects string",
  JSONSchema.matchesType("{}", "object") === false,
);

// array
check("array: matches array", JSONSchema.matchesType([1, 2], "array") === true);
check(
  "array: rejects object",
  JSONSchema.matchesType({ a: 1 }, "array") === false,
);
check("array: rejects null", JSONSchema.matchesType(null, "array") === false);

// array of types
check(
  "type array: [string,null] accepts string",
  JSONSchema.matchesType("hi", ["string", "null"]) === true,
);
check(
  "type array: [string,null] accepts null",
  JSONSchema.matchesType(null, ["string", "null"]) === true,
);
check(
  "type array: [string,null] rejects number",
  JSONSchema.matchesType(42, ["string", "null"]) === false,
);
check(
  "type array: [number,boolean] accepts bool",
  JSONSchema.matchesType(true, ["number", "boolean"]) === true,
);

// unknown type keyword → accept
check(
  "unknown type: accepts",
  JSONSchema.matchesType("anything", "foobar") === true,
);
check(
  "unknown type: accepts null",
  JSONSchema.matchesType(null, "unknown") === true,
);

// edge cases
check(
  "nested array matches array",
  JSONSchema.matchesType([[1, 2]], "array") === true,
);
check(
  "empty object matches object",
  JSONSchema.matchesType({}, "object") === true,
);
check(
  "empty array matches array",
  JSONSchema.matchesType([], "array") === true,
);

// ═══════════════════════════════════════════════════════════════════════════════
// enum (validateSchema + fromSimpleSchema)
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── enum ──");

const en = {
  type: "object",
  properties: { mode: { type: "string", enum: ["overwrite", "append"] } },
};
check(
  "enum: declared value accepted",
  JSONSchema.validateSchema({ mode: "append" }, en, "fail").ok === true,
);
const enBad = JSONSchema.validateSchema({ mode: "patch" }, en, "fail");
check("enum: undeclared value rejected", enBad.ok === false);
check(
  "enum: error mentions the whitelist",
  enBad.errors.some((e) => e.includes("must be one of")),
);
check(
  "enum: wrong-typed value rejected",
  JSONSchema.validateSchema({ mode: 123 }, en, "fail").ok === false,
);

// fromSimpleSchema — SimpleSchema DSL → standard JSON Schema
const fs = JSONSchema.fromSimpleSchema({
  path: { desc: "File path", required: true },
  start: { desc: "Start", type: "integer", default: 1 },
  mode: {
    desc: "Mode",
    type: "string",
    enum: ["overwrite", "append"],
    default: "overwrite",
  },
  tags: { desc: "Tags", type: "array", items: { type: "string" } },
  plain: {},
});
check("fromSimpleSchema: root is object", fs.type === "object");
check("fromSimpleSchema: required extracted", eq(fs.required, ["path"]));
check(
  "fromSimpleSchema: default type string",
  fs.properties.path.type === "string",
);
check("fromSimpleSchema: integer kept", fs.properties.start.type === "integer");
check("fromSimpleSchema: default kept", fs.properties.start.default === 1);
check(
  "fromSimpleSchema: enum field keeps BOTH type and enum",
  fs.properties.mode.type === "string" &&
    eq(fs.properties.mode.enum, ["overwrite", "append"]),
);
check(
  "fromSimpleSchema: array carries items",
  fs.properties.tags.type === "array" &&
    fs.properties.tags.items.type === "string",
);
check(
  "fromSimpleSchema: bare {} → string",
  fs.properties.plain.type === "string",
);
check(
  "fromSimpleSchema: empty args → bare object",
  JSONSchema.fromSimpleSchema({}).type === "object",
);

// ═══════════════════════════════════════════════════════════════════════════════
// keywords — validation + applicator
// ═══════════════════════════════════════════════════════════════════════════════

console.log("── keywords ──");

const vsc = (v, s, m) => JSONSchema.validateSchema(v, s, m ?? "fail");
const OBJ = (props) => ({ type: "object", properties: props });

// const
check("const: match", vsc({ x: 1 }, OBJ({ x: { const: 1 } })).ok === true);
check("const: mismatch", vsc({ x: 2 }, OBJ({ x: { const: 1 } })).ok === false);

// numbers
check(
  "multipleOf: ok",
  vsc({ x: 6 }, OBJ({ x: { type: "number", multipleOf: 3 } })).ok === true,
);
check(
  "multipleOf: fails",
  vsc({ x: 7 }, OBJ({ x: { type: "number", multipleOf: 3 } })).ok === false,
);
check(
  "minimum: ok",
  vsc({ x: 3 }, OBJ({ x: { type: "number", minimum: 3 } })).ok === true,
);
check(
  "minimum: fails",
  vsc({ x: 2 }, OBJ({ x: { type: "number", minimum: 3 } })).ok === false,
);
check(
  "exclusiveMinimum: ok",
  vsc({ x: 4 }, OBJ({ x: { type: "number", exclusiveMinimum: 3 } })).ok ===
    true,
);
check(
  "exclusiveMinimum: equal fails",
  vsc({ x: 3 }, OBJ({ x: { type: "number", exclusiveMinimum: 3 } })).ok ===
    false,
);
check(
  "maximum: fails",
  vsc({ x: 9 }, OBJ({ x: { type: "number", maximum: 5 } })).ok === false,
);
check(
  "exclusiveMaximum: equal fails",
  vsc({ x: 5 }, OBJ({ x: { type: "number", exclusiveMaximum: 5 } })).ok ===
    false,
);

// strings
check(
  "minLength: ok",
  vsc({ s: "abc" }, OBJ({ s: { type: "string", minLength: 3 } })).ok === true,
);
check(
  "minLength: fails",
  vsc({ s: "ab" }, OBJ({ s: { type: "string", minLength: 3 } })).ok === false,
);
check(
  "maxLength: fails",
  vsc({ s: "abcd" }, OBJ({ s: { type: "string", maxLength: 3 } })).ok === false,
);
check(
  "pattern: ok",
  vsc({ s: "abc-1" }, OBJ({ s: { type: "string", pattern: "^[a-z]+-\\d$" } }))
    .ok === true,
);
check(
  "pattern: fails",
  vsc({ s: "NOPE" }, OBJ({ s: { type: "string", pattern: "^[a-z]+$" } })).ok ===
    false,
);

// arrays
check(
  "minItems: fails",
  vsc({ xs: [1] }, OBJ({ xs: { type: "array", minItems: 2 } })).ok === false,
);
check(
  "maxItems: fails",
  vsc({ xs: [1, 2, 3] }, OBJ({ xs: { type: "array", maxItems: 2 } })).ok ===
    false,
);
check(
  "uniqueItems: ok",
  vsc({ xs: [1, 2] }, OBJ({ xs: { type: "array", uniqueItems: true } })).ok ===
    true,
);
check(
  "uniqueItems: fails",
  vsc({ xs: [1, 1] }, OBJ({ xs: { type: "array", uniqueItems: true } })).ok ===
    false,
);
check(
  "prefixItems: ok",
  vsc(
    { xs: [1, "a"] },
    OBJ({
      xs: {
        type: "array",
        prefixItems: [{ type: "integer" }, { type: "string" }],
      },
    }),
  ).ok === true,
);
check(
  "prefixItems: fails",
  vsc(
    { xs: ["a", 1] },
    OBJ({
      xs: {
        type: "array",
        prefixItems: [{ type: "integer" }, { type: "string" }],
      },
    }),
  ).ok === false,
);
check(
  "contains: ok",
  vsc(
    { xs: [1, 2, 3] },
    OBJ({ xs: { type: "array", contains: { type: "integer", minimum: 3 } } }),
  ).ok === true,
);
check(
  "contains: fails",
  vsc(
    { xs: [1, 2] },
    OBJ({ xs: { type: "array", contains: { type: "integer", minimum: 3 } } }),
  ).ok === false,
);
check(
  "minContains 0: passes",
  vsc(
    { xs: [1] },
    OBJ({
      xs: { type: "array", contains: { type: "string" }, minContains: 0 },
    }),
  ).ok === true,
);

// objects: size + dependencies + applicators
check(
  "minProperties: fails",
  vsc({}, { type: "object", minProperties: 1 }).ok === false,
);
check(
  "maxProperties: fails",
  vsc({ a: 1, b: 2 }, { type: "object", maxProperties: 1 }).ok === false,
);
check(
  "dependentRequired: ok",
  vsc({ a: 1, b: 2 }, { type: "object", dependentRequired: { a: ["b"] } })
    .ok === true,
);
check(
  "dependentRequired: fails",
  vsc({ a: 1 }, { type: "object", dependentRequired: { a: ["b"] } }).ok ===
    false,
);
check(
  "additionalProperties:false ok",
  vsc(
    { a: 1 },
    { type: "object", properties: { a: {} }, additionalProperties: false },
  ).ok === true,
);
check(
  "additionalProperties:false fails",
  vsc(
    { a: 1, z: 9 },
    { type: "object", properties: { a: {} }, additionalProperties: false },
  ).ok === false,
);
check(
  "additionalProperties schema fails",
  vsc({ z: "x" }, { type: "object", additionalProperties: { type: "number" } })
    .ok === false,
);
check(
  "patternProperties ok",
  vsc(
    { s_1: "a" },
    { type: "object", patternProperties: { "^s_": { type: "string" } } },
  ).ok === true,
);
check(
  "patternProperties fails",
  vsc(
    { s_1: 1 },
    { type: "object", patternProperties: { "^s_": { type: "string" } } },
  ).ok === false,
);
check(
  "propertyNames ok",
  vsc({ abc: 1 }, { type: "object", propertyNames: { pattern: "^[a-z]+$" } })
    .ok === true,
);
check(
  "propertyNames fails",
  vsc({ A1: 1 }, { type: "object", propertyNames: { pattern: "^[a-z]+$" } })
    .ok === false,
);
check(
  "dependentSchemas fails",
  vsc(
    { a: 1, b: "x" },
    {
      type: "object",
      dependentSchemas: { a: { properties: { b: { type: "number" } } } },
    },
  ).ok === false,
);

// composition
check("not: ok", vsc({ x: 1 }, OBJ({ x: { not: { const: 2 } } })).ok === true);
check(
  "not: fails",
  vsc({ x: 2 }, OBJ({ x: { not: { const: 2 } } })).ok === false,
);
check(
  "allOf: ok",
  vsc({ x: 4 }, OBJ({ x: { allOf: [{ minimum: 3 }, { maximum: 5 }] } })).ok ===
    true,
);
check(
  "allOf: fails",
  vsc({ x: 6 }, OBJ({ x: { allOf: [{ minimum: 3 }, { maximum: 5 }] } })).ok ===
    false,
);
check(
  "anyOf: ok",
  vsc(
    { x: "s" },
    OBJ({ x: { anyOf: [{ type: "string" }, { type: "number" }] } }),
  ).ok === true,
);
check(
  "anyOf: fails",
  vsc(
    { x: true },
    OBJ({ x: { anyOf: [{ type: "string" }, { type: "number" }] } }),
  ).ok === false,
);
check(
  "oneOf: ok",
  vsc({ x: 5 }, OBJ({ x: { oneOf: [{ type: "string" }, { type: "number" }] } }))
    .ok === true,
);
check(
  "oneOf: two match fails",
  vsc({ x: 5 }, OBJ({ x: { oneOf: [{ type: "number" }, { minimum: 0 }] } }))
    .ok === false,
);
const IF = {
  type: "object",
  if: { properties: { k: { const: "a" } }, required: ["k"] },
};
check(
  "if/then: ok",
  vsc(
    { k: "a", v: 1 },
    { ...IF, then: { properties: { v: { type: "number" } } } },
  ).ok === true,
);
check(
  "if/then: fails",
  vsc(
    { k: "a", v: "x" },
    { ...IF, then: { properties: { v: { type: "number" } } } },
  ).ok === false,
);
check(
  "if/else: ok",
  vsc(
    { k: "b", v: "x" },
    { ...IF, else: { properties: { v: { type: "string" } } } },
  ).ok === true,
);

// fromSimpleSchema: extra keywords pass through
const fsx = JSONSchema.fromSimpleSchema({
  n: { desc: "n", type: "number", exclusiveMinimum: 0 },
  s: { desc: "s", type: "string", minLength: 2, pattern: "^[a-z]+$" },
  c: { desc: "c", const: "fixed" },
  xs: {
    desc: "xs",
    type: "array",
    items: { type: "string" },
    uniqueItems: true,
  },
});
check(
  "fromSimpleSchema: exclusiveMinimum kept",
  fsx.properties.n.exclusiveMinimum === 0,
);
check(
  "fromSimpleSchema: minLength + pattern kept",
  fsx.properties.s.minLength === 2 && fsx.properties.s.pattern === "^[a-z]+$",
);
check("fromSimpleSchema: const kept", fsx.properties.c.const === "fixed");
check(
  "fromSimpleSchema: uniqueItems kept",
  fsx.properties.xs.uniqueItems === true,
);

// ═══════════════════════════════════════════════════════════════════════════════
// summary
// ═══════════════════════════════════════════════════════════════════════════════

console.log(`\n── json-schema: ${passed} passed, ${failed} failed ──`);
process.exit(failed > 0 ? 1 : 0);
