// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Shared JSON-Schema operations — the single home for schema type matching,
//! the recursive validator ([`json_schema_check`]), example synthesis, and MCP
//! tool-argument checking. Both the `json-schema` JS module and the MCP server
//! run the *same* validator; only error rendering differs per surface.
//!
//! Two `integer` contracts coexist on purpose (frozen behavior per caller),
//! selected via [`IntegerPolicy`]:
//! - `Lenient` ([`json_schema_type_matches`]): JS `jsonSchema` API; accepts
//!   i64/u64 and whole-number floats like `2.0`.
//! - `Strict` ([`json_schema_type_strict`]): MCP tool-arg validation; only
//!   i64-representable values pass.
//!
//! # Implemented keywords
//!
//! validation — `type`, `enum`, `const`, `multipleOf`,
//! `minimum` / `exclusiveMinimum` / `maximum` / `exclusiveMaximum`,
//! `minLength` / `maxLength`, `pattern`, `minItems` / `maxItems`,
//! `uniqueItems`, `minProperties` / `maxProperties`, `dependentRequired`.
//!
//! applicator — `properties`, `required`, `patternProperties`,
//! `additionalProperties`, `propertyNames`, `dependentSchemas`,
//! `prefixItems`, `items`, `contains` (+`minContains`/`maxContains`),
//! `not`, `allOf`, `anyOf`, `oneOf`, `if` / `then` / `else`.
//!
//! annotation — `description`, `default`, `example` (non-validating;
//! `default` is applied for MCP by [`json_schema_apply_defaults`]).
//!
//! Unknown keywords are ignored (never false-reject). See the "Not yet
//! implemented" note at the end of this file.

// ── type matching ──

/// Check if a JSON value matches a JSON-Schema type keyword (lenient contract).
pub(crate) fn json_schema_type_matches(value: &serde_json::Value, schema_type: &str) -> bool {
    match schema_type {
        "string" => value.is_string(),
        "number" => value.is_f64() || value.is_i64() || value.is_u64(),
        "integer" => {
            value.is_i64() || value.is_u64() ||
            // serde_json doesn't distinguish integer from float, so check
            // that an f64 value is actually an integer.
            value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true, // unknown keyword → accept
    }
}

/// Type match under the MCP tool-args contract: `integer` accepts only values
/// representable as i64 (whole floats like `2.0` and huge u64 are rejected).
/// Other keywords follow [`json_schema_type_matches`].
pub(crate) fn json_schema_type_strict(value: &serde_json::Value, schema_type: &str) -> bool {
    match schema_type {
        "integer" => value.as_i64().is_some(),
        _ => json_schema_type_matches(value, schema_type),
    }
}

/// Check if a value matches a type which may be a string or array of strings.
pub(crate) fn json_schema_matches_type_field(
    value: &serde_json::Value,
    type_field: &serde_json::Value,
) -> bool {
    match type_field {
        serde_json::Value::String(s) => json_schema_type_matches(value, s),
        serde_json::Value::Array(arr) => arr.iter().any(|t| {
            if let serde_json::Value::String(s) = t {
                json_schema_type_matches(value, s)
            } else {
                false
            }
        }),
        _ => true, // no type specified → accept
    }
}

/// Whether a schema's `enum` keyword (when present) admits `value`. A schema
/// with no `enum` admits anything. Used by the shared validator
/// ([`json_schema_check`]).
pub(crate) fn json_schema_enum_allows(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> bool {
    match schema.get("enum").and_then(|v| v.as_array()) {
        Some(list) => list.iter().any(|v| v == value),
        None => true,
    }
}

// ── unified validation ──

/// Integer matching policy — frozen per caller:
/// - `Lenient`: the JS `jsonSchema` API; accepts i64/u64 and whole-number
///   floats like `2.0`.
/// - `Strict`: MCP tool-arg validation; only i64-representable values pass.
#[derive(Clone, Copy)]
pub(crate) enum IntegerPolicy {
    Lenient,
    Strict,
}

impl IntegerPolicy {
    fn matches(self, value: &serde_json::Value, schema_type: &str) -> bool {
        match self {
            IntegerPolicy::Lenient => json_schema_type_matches(value, schema_type),
            IntegerPolicy::Strict => json_schema_type_strict(value, schema_type),
        }
    }
}

/// One schema violation, in schema/property order. Rendered per caller surface
/// by [`json_schema_error_js`] / [`json_schema_error_mcp`].
pub(crate) struct Violation {
    path: String,
    kind: ViolationKind,
}

enum ViolationKind {
    NotObject,
    NotArray,
    /// Wrong (or missing) `type`; carries the expected type keyword.
    Type(String),
    /// A `required` property is absent.
    Missing,
    /// Value not in the declared `enum`.
    NotInEnum {
        allowed: Vec<serde_json::Value>,
        value: serde_json::Value,
    },
    /// Any other keyword failure. `phrase` reads after the subject, e.g.
    /// `must be >= 3` / `must have at most 5 items` / `must match "^a"`.
    Keyword(String),
}

/// The single recursive JSON-Schema validator used by BOTH the JS
/// `validateSchema` and the MCP tool-arg validator. Walks `value` against
/// `schema`, pushing structured [`Violation`]s and returning the possibly
/// coerced value. `coerce` repairs str↔num↔bool mismatches (JS "coerce" mode;
/// MCP passes `false`). Only error rendering differs per surface.
pub(crate) fn json_schema_check(
    value: serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    policy: IntegerPolicy,
    coerce: bool,
    changed: &mut bool,
    out: &mut Vec<Violation>,
) -> serde_json::Value {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return value,
    };

    // `enum` / `const` — checked before the type, reported regardless of type.
    if !json_schema_enum_allows(schema, &value) {
        out.push(Violation {
            path: path.to_string(),
            kind: ViolationKind::NotInEnum {
                allowed: obj
                    .get("enum")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default(),
                value: value.clone(),
            },
        });
    }
    if let Some(c) = obj.get("const")
        && c != &value
    {
        push_kw(out, path, format!("must be {}", json_str(c)));
    }

    let schema_type = obj.get("type");

    // Object: explicit `type:"object"`, or inferred from any object keyword.
    let schema_is_object = schema_type
        .map_or(OBJECT_KEYWORDS.iter().any(|k| obj.contains_key(*k)), |t| {
            t.as_str() == Some("object")
        });

    if schema_is_object {
        if !value.is_object() {
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::NotObject,
            });
            return value;
        }
        let mut map = value.as_object().cloned().unwrap_or_default();
        check_object_keywords(obj, &mut map, path, policy, coerce, changed, out);
        let result = serde_json::Value::Object(map);
        apply_composition(obj, &result, path, policy, changed, out);
        return result;
    }

    // Array: explicit `type:"array"`, or (untyped) an array value.
    let schema_is_array = schema_type.is_some_and(|t| t.as_str() == Some("array"))
        || (schema_type.is_none() && !value.is_null() && value.is_array());
    if schema_is_array {
        if !value.is_array() {
            out.push(Violation {
                path: path.to_string(),
                kind: ViolationKind::NotArray,
            });
            return value;
        }
        let arr = value.as_array().cloned().unwrap_or_default();
        let result = check_array_keywords(obj, arr, path, policy, coerce, changed, out);
        apply_composition(obj, &result, path, policy, changed, out);
        return result;
    }

    // Primitive / untyped value.
    let mut value = value;
    if let Some(type_str) = schema_type.and_then(|t| t.as_str()) {
        match type_str {
            "string" => {
                if !policy.matches(&value, "string") {
                    if coerce && (value.is_number() || value.is_boolean()) {
                        value = serde_json::Value::String(match &value {
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            _ => String::new(),
                        });
                        *changed = true;
                    } else {
                        out.push(Violation {
                            path: path.to_string(),
                            kind: ViolationKind::Type("string".into()),
                        });
                    }
                }
            }
            "number" | "integer" => {
                if !policy.matches(&value, type_str) {
                    let mut coerced = None;
                    if coerce {
                        let n = match &value {
                            serde_json::Value::String(s) => s.parse::<f64>().ok(),
                            serde_json::Value::Bool(true) => Some(1.0),
                            serde_json::Value::Bool(false) => Some(0.0),
                            _ => None,
                        };
                        if let Some(n) = n {
                            if type_str == "integer" && n.fract() != 0.0 {
                                out.push(Violation {
                                    path: path.to_string(),
                                    kind: ViolationKind::Type("integer".into()),
                                });
                            }
                            if let Some(num) = serde_json::Number::from_f64(n) {
                                coerced = Some(serde_json::Value::Number(num));
                            }
                        }
                    }
                    match coerced {
                        Some(v) => {
                            *changed = true;
                            value = v;
                        }
                        None => out.push(Violation {
                            path: path.to_string(),
                            kind: ViolationKind::Type(type_str.into()),
                        }),
                    }
                }
            }
            "boolean" => {
                if !policy.matches(&value, "boolean") {
                    let mut coerced = None;
                    if coerce {
                        match &value {
                            serde_json::Value::String(s) if s == "true" => {
                                coerced = Some(serde_json::Value::Bool(true));
                            }
                            serde_json::Value::String(s) if s == "false" => {
                                coerced = Some(serde_json::Value::Bool(false));
                            }
                            _ => {}
                        }
                    }
                    match coerced {
                        Some(v) => {
                            *changed = true;
                            value = v;
                        }
                        None => out.push(Violation {
                            path: path.to_string(),
                            kind: ViolationKind::Type("boolean".into()),
                        }),
                    }
                }
            }
            "null" => {
                if !value.is_null() {
                    out.push(Violation {
                        path: path.to_string(),
                        kind: ViolationKind::Type("null".into()),
                    });
                }
            }
            _ => {} // unknown type keyword → accept
        }
    }

    check_scalar_keywords(obj, &value, path, out);
    apply_composition(obj, &value, path, policy, changed, out);
    value
}

/// Object keywords handled by [`json_schema_check`] (used to infer `object`).
const OBJECT_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "additionalProperties",
    "propertyNames",
    "required",
    "dependentRequired",
    "dependentSchemas",
    "minProperties",
    "maxProperties",
];

fn push_kw(out: &mut Vec<Violation>, path: &str, phrase: impl Into<String>) {
    out.push(Violation {
        path: path.to_string(),
        kind: ViolationKind::Keyword(phrase.into()),
    });
}

fn json_str(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Regex match for the `pattern` / `patternProperties` keywords. Uses
/// `regex-automata`'s meta engine — the minimal core (the umbrella `regex`
/// crate is only a façade over it) — which aike already compiles via
/// `globset`/`grep-regex`. An invalid pattern is treated as "matches" (never
/// false-rejects), matching the "unknown keyword accepts" philosophy.
fn regex_is_match(pattern: &str, s: &str) -> bool {
    regex_automata::meta::Regex::new(pattern)
        .map(|re| re.is_match(s.as_bytes()))
        .unwrap_or(true)
}

/// Whether `value` satisfies `sub` (no coercion). Used by compositors.
fn subschema_matches(
    sub: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
    policy: IntegerPolicy,
    changed: &mut bool,
) -> bool {
    let mut s = Vec::new();
    json_schema_check(value.clone(), sub, path, policy, false, changed, &mut s);
    s.is_empty()
}

/// Object-shape keywords: propertyNames / properties / patternProperties /
/// additionalProperties / required / dependent* / min|maxProperties.
fn check_object_keywords(
    obj: &serde_json::Map<String, serde_json::Value>,
    map: &mut serde_json::Map<String, serde_json::Value>,
    path: &str,
    policy: IntegerPolicy,
    coerce: bool,
    changed: &mut bool,
    out: &mut Vec<Violation>,
) {
    // propertyNames — every key validates against the subschema.
    if let Some(pn) = obj.get("propertyNames") {
        for k in map.keys() {
            let mut scratch = Vec::new();
            json_schema_check(
                serde_json::Value::String(k.clone()),
                pn,
                &format!("{path}.{k}"),
                policy,
                false,
                changed,
                &mut scratch,
            );
            if !scratch.is_empty() {
                push_kw(out, path, format!("property name \"{k}\" is invalid"));
            }
        }
    }

    let declared = obj.get("properties").and_then(|p| p.as_object());
    let patterns = obj.get("patternProperties").and_then(|p| p.as_object());
    let additional = obj.get("additionalProperties");
    let keys: Vec<String> = map.keys().cloned().collect();
    for k in &keys {
        let v = map.get(k).cloned().unwrap_or(serde_json::Value::Null);
        let mut matched = false;
        if let Some(ps) = declared.and_then(|d| d.get(k)) {
            matched = true;
            let c = json_schema_check(
                v.clone(),
                ps,
                &format!("{path}.{k}"),
                policy,
                coerce,
                changed,
                out,
            );
            map.insert(k.clone(), c);
        }
        if let Some(pp) = patterns {
            for (pat, ps) in pp {
                if regex_is_match(pat, k) {
                    matched = true;
                    let c = json_schema_check(
                        v.clone(),
                        ps,
                        &format!("{path}.{k}"),
                        policy,
                        coerce,
                        changed,
                        out,
                    );
                    map.insert(k.clone(), c);
                }
            }
        }
        if !matched {
            match additional {
                Some(serde_json::Value::Bool(false)) => push_kw(
                    out,
                    &format!("{path}.{k}"),
                    "is not allowed (additionalProperties: false)",
                ),
                Some(ap) if ap.is_object() => {
                    let c = json_schema_check(
                        v,
                        ap,
                        &format!("{path}.{k}"),
                        policy,
                        coerce,
                        changed,
                        out,
                    );
                    map.insert(k.clone(), c);
                }
                _ => {}
            }
        }
    }

    if let Some(req) = obj.get("required").and_then(|r| r.as_array()) {
        for r in req {
            if let Some(name) = r.as_str()
                && !map.contains_key(name)
            {
                out.push(Violation {
                    path: format!("{path}.{name}"),
                    kind: ViolationKind::Missing,
                });
            }
        }
    }

    if let Some(dr) = obj.get("dependentRequired").and_then(|d| d.as_object()) {
        for (k, reqs) in dr {
            if map.contains_key(k)
                && let Some(arr) = reqs.as_array()
            {
                for r in arr {
                    if let Some(n) = r.as_str()
                        && !map.contains_key(n)
                    {
                        push_kw(out, &format!("{path}.{k}"), format!("requires \"{n}\""));
                    }
                }
            }
        }
    }

    if let Some(ds) = obj.get("dependentSchemas").and_then(|d| d.as_object()) {
        for (k, sub) in ds {
            if map.contains_key(k) {
                json_schema_check(
                    serde_json::Value::Object(map.clone()),
                    sub,
                    path,
                    policy,
                    coerce,
                    changed,
                    out,
                );
            }
        }
    }

    let np = map.len() as u64;
    if let Some(m) = obj.get("minProperties").and_then(|v| v.as_u64())
        && np < m
    {
        push_kw(out, path, format!("must have at least {m} properties"));
    }
    if let Some(m) = obj.get("maxProperties").and_then(|v| v.as_u64())
        && np > m
    {
        push_kw(out, path, format!("must have at most {m} properties"));
    }
}

/// Array-shape keywords: prefixItems / items / min|maxItems / uniqueItems /
/// contains (+min|maxContains).
fn check_array_keywords(
    obj: &serde_json::Map<String, serde_json::Value>,
    arr: Vec<serde_json::Value>,
    path: &str,
    policy: IntegerPolicy,
    coerce: bool,
    changed: &mut bool,
    out: &mut Vec<Violation>,
) -> serde_json::Value {
    let prefix = obj.get("prefixItems").and_then(|p| p.as_array());
    let items = obj.get("items");
    let mut coerced: Vec<serde_json::Value> = Vec::with_capacity(arr.len());
    for (i, v) in arr.into_iter().enumerate() {
        // `prefixItems` covers the leading positions; `items` the rest.
        let sub = if prefix.is_some() {
            prefix.and_then(|p| p.get(i))
        } else {
            items
        };
        match sub {
            Some(ps) => coerced.push(json_schema_check(
                v,
                ps,
                &format!("{path}[{i}]"),
                policy,
                coerce,
                changed,
                out,
            )),
            None => coerced.push(v),
        }
    }

    let n = coerced.len() as u64;
    if let Some(m) = obj.get("minItems").and_then(|v| v.as_u64())
        && n < m
    {
        push_kw(out, path, format!("must have at least {m} items"));
    }
    if let Some(m) = obj.get("maxItems").and_then(|v| v.as_u64())
        && n > m
    {
        push_kw(out, path, format!("must have at most {m} items"));
    }
    if obj.get("uniqueItems").and_then(|v| v.as_bool()) == Some(true) {
        'outer: for i in 0..coerced.len() {
            for j in (i + 1)..coerced.len() {
                if coerced[i] == coerced[j] {
                    push_kw(out, path, "items must be unique");
                    break 'outer;
                }
            }
        }
    }
    if let Some(cs) = obj.get("contains") {
        let cnt = coerced
            .iter()
            .filter(|v| {
                let mut s = Vec::new();
                json_schema_check((*v).clone(), cs, path, policy, false, changed, &mut s);
                s.is_empty()
            })
            .count() as i64;
        let min = obj.get("minContains").and_then(|v| v.as_i64()).unwrap_or(1);
        if cnt < min {
            push_kw(
                out,
                path,
                format!("must contain at least {min} matching item(s)"),
            );
        }
        if let Some(mx) = obj.get("maxContains").and_then(|v| v.as_i64())
            && cnt > mx
        {
            push_kw(
                out,
                path,
                format!("must contain at most {mx} matching item(s)"),
            );
        }
    }

    serde_json::Value::Array(coerced)
}

/// Scalar keywords: string min|maxLength / pattern and numeric multipleOf /
/// min|max|exclusiveMin|exclusiveMax. Only the ones matching the value's shape fire.
fn check_scalar_keywords(
    obj: &serde_json::Map<String, serde_json::Value>,
    value: &serde_json::Value,
    path: &str,
    out: &mut Vec<Violation>,
) {
    if let Some(s) = value.as_str() {
        let len = s.chars().count() as u64;
        if let Some(m) = obj.get("minLength").and_then(|v| v.as_u64())
            && len < m
        {
            push_kw(out, path, format!("must be at least {m} characters"));
        }
        if let Some(m) = obj.get("maxLength").and_then(|v| v.as_u64())
            && len > m
        {
            push_kw(out, path, format!("must be at most {m} characters"));
        }
        if let Some(p) = obj.get("pattern").and_then(|v| v.as_str())
            && !regex_is_match(p, s)
        {
            push_kw(out, path, format!("must match pattern {:?}", p));
        }
    }
    if let Some(n) = value.as_f64() {
        if let Some(m) = obj.get("multipleOf").and_then(|v| v.as_f64())
            && m > 0.0
        {
            let q = n / m;
            if (q - q.round()).abs() > 1e-9 {
                push_kw(out, path, format!("must be a multiple of {m}"));
            }
        }
        if let Some(m) = obj.get("minimum").and_then(|v| v.as_f64())
            && n < m
        {
            push_kw(out, path, format!("must be >= {m}"));
        }
        if let Some(m) = obj.get("exclusiveMinimum").and_then(|v| v.as_f64())
            && n <= m
        {
            push_kw(out, path, format!("must be > {m}"));
        }
        if let Some(m) = obj.get("maximum").and_then(|v| v.as_f64())
            && n > m
        {
            push_kw(out, path, format!("must be <= {m}"));
        }
        if let Some(m) = obj.get("exclusiveMaximum").and_then(|v| v.as_f64())
            && n >= m
        {
            push_kw(out, path, format!("must be < {m}"));
        }
    }
}

/// Applicator keywords: `not` / `allOf` / `anyOf` / `oneOf` / `if`-`then`-`else`.
/// Subschemas run in pure-check mode (no coercion).
fn apply_composition(
    obj: &serde_json::Map<String, serde_json::Value>,
    value: &serde_json::Value,
    path: &str,
    policy: IntegerPolicy,
    changed: &mut bool,
    out: &mut Vec<Violation>,
) {
    if let Some(sub) = obj.get("not")
        && subschema_matches(sub, value, path, policy, changed)
    {
        push_kw(out, path, "must not match the `not` schema");
    }
    if let Some(arr) = obj.get("allOf").and_then(|v| v.as_array()) {
        for sub in arr {
            json_schema_check(value.clone(), sub, path, policy, false, changed, out);
        }
    }
    if let Some(arr) = obj.get("anyOf").and_then(|v| v.as_array())
        && !arr
            .iter()
            .any(|sub| subschema_matches(sub, value, path, policy, changed))
    {
        push_kw(out, path, "must match at least one of the `anyOf` schemas");
    }
    if let Some(arr) = obj.get("oneOf").and_then(|v| v.as_array()) {
        let count = arr
            .iter()
            .filter(|sub| subschema_matches(sub, value, path, policy, changed))
            .count();
        if count != 1 {
            push_kw(
                out,
                path,
                format!("must match exactly one of the `oneOf` schemas (matched {count})"),
            );
        }
    }
    if let Some(if_s) = obj.get("if") {
        if subschema_matches(if_s, value, path, policy, changed) {
            if let Some(t) = obj.get("then") {
                json_schema_check(value.clone(), t, path, policy, false, changed, out);
            }
        } else if let Some(e) = obj.get("else") {
            json_schema_check(value.clone(), e, path, policy, false, changed, out);
        }
    }
}

/// JS `validateSchema` surface: run the shared validator (lenient integers) and
/// render each violation as `"{path}: ..."`.
pub(crate) fn json_schema_error_js(v: &Violation) -> String {
    let what = match &v.kind {
        ViolationKind::NotObject => "expected object".to_string(),
        ViolationKind::NotArray => "expected array".to_string(),
        ViolationKind::Type(t) => format!("expected {t}"),
        ViolationKind::Missing => "missing required property".to_string(),
        ViolationKind::NotInEnum { allowed, .. } => {
            format!(
                "must be one of {}",
                serde_json::to_string(allowed).unwrap_or_default()
            )
        }
        ViolationKind::Keyword(phrase) => phrase.clone(),
    };
    format!("{}: {}", v.path, what)
}

/// Run the shared validator (lenient integers) collecting JS error strings.
/// Returns the (possibly coerced) value; `changed` is set iff a coercion fired.
pub(crate) fn json_schema_coerce_or_check(
    value: serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
    coerce: bool,
    changed: &mut bool,
) -> serde_json::Value {
    let mut violations = Vec::new();
    let out = json_schema_check(
        value,
        schema,
        path,
        IntegerPolicy::Lenient,
        coerce,
        changed,
        &mut violations,
    );
    for v in &violations {
        errors.push(json_schema_error_js(v));
    }
    out
}

// ── example synthesis ──

/// Synthesize an example value from a JSON-Schema.
pub(crate) fn json_schema_example(schema: &serde_json::Value) -> serde_json::Value {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return serde_json::Value::Null,
    };

    // If schema has an explicit example, use it.
    if let Some(ex) = obj.get("example") {
        return ex.clone();
    }

    let type_str = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match type_str {
        "object" => {
            let mut map = serde_json::Map::new();
            if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
                for (k, prop_schema) in props {
                    map.insert(k.clone(), json_schema_example(prop_schema));
                }
            }
            serde_json::Value::Object(map)
        }
        "array" => {
            if let Some(items) = obj.get("items") {
                serde_json::Value::Array(vec![json_schema_example(items)])
            } else {
                serde_json::Value::Array(vec![])
            }
        }
        "string" => serde_json::Value::String("string".into()),
        "number" => serde_json::Value::Number(serde_json::Number::from(0)),
        "integer" => serde_json::Value::Number(serde_json::Number::from(0)),
        "boolean" => serde_json::Value::Bool(true),
        "null" => serde_json::Value::Null,
        _ => {
            // No type — if properties exist, treat as object.
            if obj.contains_key("properties") {
                return json_schema_example(
                    &serde_json::json!({"type": "object", "properties": obj.get("properties")}),
                );
            }
            serde_json::Value::Null
        }
    }
}

// ── MCP tool-argument checking ──
//
// `json_schema_validate_args` runs the SAME validator as the JS
// `validateSchema` (see the module docs for the implemented keyword set) under
// the strict integer contract. `json_schema_apply_defaults` fills missing
// optional args from their schema `default` before validation, so a bad
// default is rejected like a bad provided value.

/// Validate MCP tool-call `arguments` against a tool's `inputSchema` before
/// dispatch.
///
/// Covers the subset of JSON Schema that `simple_schema_from_args`
/// produces: an object with `required` and per-property `type`/`enum`
/// (including array `items`). Types string/integer/number/boolean/object/
/// null/array are enforced; extra keys are ignored; unknown types pass.
pub(crate) fn json_schema_validate_args(
    schema: &serde_json::Map<String, serde_json::Value>,
    arguments: &serde_json::Value,
) -> Result<(), String> {
    if !arguments.is_object() {
        return Err("arguments must be a JSON object".to_string());
    }

    // Run the SAME validator the JS `validateSchema` path uses — strict integer
    // contract for tool args — and report the first violation in MCP wording.
    let mut violations = Vec::new();
    let mut changed = false;
    json_schema_check(
        arguments.clone(),
        &serde_json::Value::Object(schema.clone()),
        "root",
        IntegerPolicy::Strict,
        false,
        &mut changed,
        &mut violations,
    );

    match violations.first() {
        Some(v) => Err(json_schema_error_mcp(v)),
        None => Ok(()),
    }
}

/// Fill missing optional MCP tool arguments with their schema `default` values.
///
/// A non-required property that carries a `default` is inserted into
/// `arguments` when absent, so handlers always receive the default. If
/// `arguments` is not an object (e.g. `null` when no args were sent) it is
/// replaced by a fresh object whenever at least one default exists.
pub(crate) fn json_schema_apply_defaults(
    schema: &serde_json::Map<String, serde_json::Value>,
    arguments: &mut serde_json::Value,
) {
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    // Fast path: no property carries a default.
    if !properties.values().any(|p| p.get("default").is_some()) {
        return;
    }

    let obj = match arguments.as_object_mut() {
        Some(obj) => obj,
        None => {
            *arguments = serde_json::Value::Object(serde_json::Map::new());
            arguments.as_object_mut().expect("just created object")
        }
    };
    for (name, prop_schema) in properties {
        if !obj.contains_key(name)
            && let Some(default) = prop_schema.get("default")
        {
            obj.insert(name.clone(), default.clone());
        }
    }
}

/// MCP tool-arg surface: render a [`Violation`] as the protocol error string.
///
/// Enum is checked before type; wording is part of the MCP protocol surface and
/// must not change.
fn json_schema_error_mcp(v: &Violation) -> String {
    // Array-item violations (`<parent>[<i>]`) report the parent field.
    if let Some((parent_path, _idx)) = v.path.rsplit_once('[')
        && let ViolationKind::Type(item_type) = &v.kind
    {
        let parent = parent_path.rsplit('.').next().unwrap_or(parent_path);
        return format!("items of \"{}\" must be {}", parent, item_type);
    }

    let name = v.path.rsplit('.').next().unwrap_or(&v.path);
    match &v.kind {
        ViolationKind::Missing => format!("missing required argument: \"{}\"", name),
        ViolationKind::NotInEnum { value, .. } => format!(
            "invalid value for \"{}\": {}",
            name,
            serde_json::to_string(value).unwrap_or_default()
        ),
        ViolationKind::NotObject => format!("\"{}\" must be an object", name),
        ViolationKind::NotArray => format!("\"{}\" must be an array", name),
        ViolationKind::Type(t) => {
            // Wording below is protocol surface (asserted in tests): keep exact
            // articles — "an integer"/"an object", bare "null".
            let noun = match t.as_str() {
                "integer" | "object" => format!("an {t}"),
                "null" => "null".to_string(),
                _ => format!("a {t}"),
            };
            format!("\"{}\" must be {}", name, noun)
        }
        ViolationKind::Keyword(phrase) => format!("\"{}\" {}", name, phrase),
    }
}

// ── Not yet implemented ─────────────────────────────────────────────────────
//
// Standard JSON Schema 2020-12 keywords this validator does NOT enforce.
// Unknown keywords are ignored (the validator never false-rejects), so a
// schema that uses them still validates — only the extra check is missing.
//
// Core / references — need a schema resolver (URI handling + $ref recursion):
//   $id, $schema, $vocabulary, $anchor, $dynamicAnchor, $dynamicRef,
//   $ref, $defs, $comment
//
// Applicator — need full annotation ("evaluated") tracking:
//   unevaluatedItems, unevaluatedProperties
//
// Format — `format` is a non-enforcing annotation by default; the assertion
//   vocabulary (email / date-time / uri / ipv4 / …) is not implemented.
//
// Content:
//   contentEncoding, contentMediaType, contentSchema
//
// Meta-data — non-validating annotations, intentionally ignored:
//   title, deprecated, readOnly, writeOnly, examples
