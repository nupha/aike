// Type declarations for the "json-schema" module

/** A valid JSON value (recursive). */
type JSONValue =
  | string
  | number
  | boolean
  | null
  | JSONValue[]
  | { [key: string]: JSONValue };

/** A type keyword accepted by JSON-Schema. */
type SchemaType =
  | "string"
  | "number"
  | "integer"
  | "boolean"
  | "object"
  | "array"
  | "null";

/** A minimal JSON-Schema definition. */
interface Schema {
  type?: SchemaType | SchemaType[];
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
  example?: unknown;
  [key: string]: unknown;
}

/**
 * Simplified schema definition for argument DSLs (e.g. MCP tool `args`).
 * Rust auto-converts it to standard JSON Schema `properties` entries.
 *
 * @example
 * // Basic string field
 * { desc: "File path.", required: true }
 * // Integer field
 * { desc: "Start line", type: "integer", default: 0 }
 * // Boolean field
 * { desc: "Show hidden files", type: "boolean", default: false }
 * // Array of strings
 * { desc: "File patterns", type: "array" }
 * // Enum
 * { desc: "Write mode", type: "string", enum: ["overwrite", "append"], default: "overwrite" }
 * // Array of integers
 * { desc: "Port numbers", type: "array", items: { type: "integer" } }
 * // Array with element description
 * { desc: "Tags", type: "array", items: { type: "string", desc: "Tag name" } }
 */
interface SimpleSchema {
  /** Human-readable description of this argument. */
  desc: string;
  /**
   * Value type. Defaults to `"string"` when omitted.
   * Allowed JSON-Schema types (per JSON Schema Validation 2020-12 §6.1.1):
   * - `"string"` — default
   * - `"integer"` — integer number (a subtype of `number`, zero fractional part)
   * - `"number"` — floating-point number (e.g. latitude/longitude)
   * - `"boolean"` — true/false
   * - `"array"` — array; use `items` to specify element type
   * - `"object"` — nested object
   * - `"null"` — JSON null
   */
  type?: SchemaType;
  /** Mark as required. */
  required?: boolean;
  /** Enum string values. */
  enum?: string[];
  /** Default value when not provided. */
  default?: any;
  /**
   * Schema for array items. Only meaningful when `type="array"`.
   * Defaults to `{ type: "string" }` when omitted.
   *
   * @example
   * // Array of integers
   * { desc: "Port numbers", type: "array", items: { type: "integer" } }
   */
  items?: {
    /** Element type */
    type: SchemaType;
    /** Description of each element. */
    desc?: string;
    /** Enum values for elements. */
    enum?: string[];
  };
  /** Exact-value constraint. */
  const?: unknown;
  /** Numeric: value must be a multiple of this. */
  multipleOf?: number;
  /** Numeric lower bound (inclusive). */
  minimum?: number;
  /** Numeric lower bound (exclusive). */
  exclusiveMinimum?: number;
  /** Numeric upper bound (inclusive). */
  maximum?: number;
  /** Numeric upper bound (exclusive). */
  exclusiveMaximum?: number;
  /** String length bounds (Unicode code points). */
  minLength?: number;
  maxLength?: number;
  /** String must match this regular expression. */
  pattern?: string;
  /** Array length bounds. */
  minItems?: number;
  maxItems?: number;
  /** Array items must be unique. */
  uniqueItems?: boolean;
  /** Object size bounds. */
  minProperties?: number;
  maxProperties?: number;
  /** Object: if the key is present, the listed keys must also be present. */
  dependentRequired?: Record<string, string[]>;
  /** Object applicator: false forbids undeclared keys; a schema validates them. */
  additionalProperties?: boolean | Record<string, unknown>;
  /** Object applicator: subschemas for property names matching each regex. */
  patternProperties?: Record<string, unknown>;
  /** Object applicator: subschema every property name must satisfy. */
  propertyNames?: Record<string, unknown>;
  /** Array applicator: positional subschemas for the leading items. */
  prefixItems?: Record<string, unknown>[];
  /** Array applicator: subschema at least `minContains` items must satisfy. */
  contains?: Record<string, unknown>;
  minContains?: number;
  maxContains?: number;
  /** Composition / conditional applicators. */
  not?: Record<string, unknown>;
  allOf?: Record<string, unknown>[];
  anyOf?: Record<string, unknown>[];
  oneOf?: Record<string, unknown>[];
  if?: Record<string, unknown>;
  then?: Record<string, unknown>;
  else?: Record<string, unknown>;
  dependentSchemas?: Record<string, unknown>;
}

/** Result of `JSONSchema.validate()`. */
interface ValidateResult {
  /** `true` when no errors were found. */
  ok: boolean;
  /** The (possibly coerced) value. In "coerce" mode, type mismatches are fixed. */
  value: unknown;
  /** Error messages in the form `"path: expected <type>"`. Empty when `ok` is `true`. */
  errors: string[];
}

declare module "json-schema" {
  const JSONSchema: {
    /**
     * Extract the first JSON value from free-form LLM text.
     *
     * Tries three strategies in order:
     *  1. **Strict** — `JSON.parse` the entire trimmed text.
     *  2. **Fenced** — find the first `` ```json … ``` `` (or `` ``` … ``` ``)
     *     code block and parse its contents.
     *  3. **Bare** — locate the first `{` and match balanced braces, then parse.
     *
     * Returns `undefined` if none succeed.
     *
     * @example
     * ```ts
     * JSONSchema.extract('Here is the result: ```json\n{"id":1}\n```');
     * // → { id: 1 }
     * ```
     */
    extractJSON(text: string): unknown;

    /**
     * Validate a parsed value against a JSON-Schema, with optional coercion.
     *
     * Supports `type`, `properties`, `required`, and `items` keywords.
     *
     * @param value  - The parsed data to validate.
     * @param schema - A minimal JSON-Schema.
     * @param mode   - How to handle mismatches:
     *   - `"fail"`       — collect errors, return the original values.
     *   - `"coerce"`     — fix obvious type mismatches (str↔num, str↔bool).
     *   - `"best-effort"` (default) — same as `"fail"`.
     *
     * @example
     * ```ts
     * JSONSchema.validate({ n: "42" }, { type: "object", properties: { n: { type: "number" } } }, "coerce");
     * // → { ok: true, value: { n: 42 }, errors: [] }
     * ```
     */
    validateSchema(
      value: unknown,
      schema: Schema,
      mode?: "fail" | "coerce" | "best-effort",
    ): ValidateResult;

    /**
     * Generate a synthetic example value that conforms to the given schema.
     * Useful for building LLM prompt contracts that require structured output.
     *
     *   - object  → recursively fill each property
     *   - array   → `[forExample(items)]`
     *   - string  → `"string"` ; number/integer → `0` ; boolean → `true`
     *   - schema with `example` → returns the example directly
     *
     * @example
     * ```ts
     * JSONSchema.forExample({ type: "object", properties: { name: { type: "string" } } });
     * // → { name: "string" }
     * ```
     */
    forExample(schema: Schema): unknown;

    /**
     * Check whether a JavaScript value matches a JSON-Schema `type` keyword.
     *
     * Handles all standard type keywords (`"string"`, `"number"`, `"integer"`,
     * `"boolean"`, `"object"`, `"array"`, `"null"`) and accepts unknown keywords
     * (returns `true` so unsupported keywords never cause false-rejection).
     *
     * `type` may be a single string or an array of allowed types.
     *
     * @example
     * ```ts
     * JSONSchema.matchesType(42, "number");          // true
     * JSONSchema.matchesType(3.5, "integer");        // false
     * JSONSchema.matchesType("hi", ["string", "null"]); // true
     * ```
     */
    matchesType(value: unknown, type: SchemaType | SchemaType[]): boolean;

    /**
     * Convert a SimpleSchema argument DSL record into a standard JSON Schema
     * object schema (`{ type: "object", properties, required }`).
     *
     * Missing fields follow the SimpleSchema defaults: `type` → `"string"`,
     * array `items` type → `"string"`. An empty record yields
     * `{ type: "object" }`.
     *
     * @example
     * ```ts
     * JSONSchema.fromSimpleSchema({
     *   path:  { desc: "File path", required: true },
     *   start: { desc: "Start line", type: "number", default: 0 },
     * });
     * // → {
     * //   type: "object",
     * //   properties: {
     * //     path:  { type: "string", description: "File path" },
     * //     start: { type: "number", description: "Start line", default: 0 },
     * //   },
     * //   required: ["path"],
     * // }
     * ```
     */
    fromSimpleSchema(simple: Record<string, SimpleSchema>): Schema;
  };

  export default JSONSchema;
}
