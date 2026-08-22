// Type declarations for the "json-schema" module

/** A type keyword accepted by JSON-Schema. */
type SchemaType = "string" | "number" | "integer" | "boolean" | "object" | "array" | "null";

/** A minimal JSON-Schema definition. */
interface Schema {
  type?: SchemaType | SchemaType[];
  properties?: Record<string, Schema>;
  required?: string[];
  items?: Schema;
  example?: unknown;
  [key: string]: unknown;
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
  };

  export default JSONSchema;
}
