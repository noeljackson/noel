//! Canonical JSON, `hash64` and the schema subtyping check `fits`, as in
//! upstream's `chord/src/schema.ts`.

use serde_json::{Map, Value};

use crate::js;

/// Canonical JSON: object keys sorted as JavaScript's default sort orders
/// strings (by UTF-16 code unit), everything else as `JSON.stringify` writes
/// it.
///
/// Upstream also drops `undefined` members; a [`Value`] has none.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let n = js::number_value(n);
            // JSON.stringify writes non-finite numbers as null.
            if n.is_finite() {
                out.push_str(&js::number_to_string(n));
            } else {
                out.push_str("null");
            }
        }
        Value::String(s) => js::quote_into(s, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| js::utf16_cmp(a, b));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                js::quote_into(key, out);
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

/// Upstream's non-cryptographic 64-bit hash: two FNV-1a-style lanes over the
/// text's UTF-16 code units, printed as 16 lowercase hex digits.
pub fn hash64(text: &str) -> String {
    let mut a: u32 = 0x811c_9dc5;
    let mut b: u32 = 0x0100_0193 ^ 0x5bd1_e995;
    for code in text.encode_utf16() {
        let code = u32::from(code);
        a = (a ^ code).wrapping_mul(0x0100_0193);
        b = (b ^ code).wrapping_mul(0x5bd1_e995) ^ (b >> 13);
    }
    format!("{a:08x}{b:08x}")
}

/// Upstream's `deepEqual`: equal canonical JSON. An absent value compares as
/// `null`, because `canonicalJson(undefined)` is `"null"`.
pub(crate) fn deep_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    canonical_json(a.unwrap_or(&Value::Null)) == canonical_json(b.unwrap_or(&Value::Null))
}

type Schema = Map<String, Value>;

const CONSTRAINTS: [&str; 14] = [
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

/// Keywords `fits` reasons about, with [`CONSTRAINTS`]; any other keyword in
/// the wider schema forces exact equality.
const KNOWN: [&str; 12] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "patternProperties",
    "items",
    "additionalItems",
    "const",
    "enum",
    "anyOf",
    "parameters",
    "returnType",
];

/// Annotations: no effect on validity.
const ANNOTATIONS: [&str; 10] = [
    "title",
    "description",
    "default",
    "examples",
    "$id",
    "$schema",
    "$comment",
    "readOnly",
    "writeOnly",
    "deprecated",
];

fn known(key: &str) -> bool {
    KNOWN.contains(&key) || CONSTRAINTS.contains(&key)
}

/// Upstream reaches a schema it cannot evaluate (a malformed keyword, or a
/// pattern this crate cannot compile) and throws; [`fits`] answers `false`.
struct Unsupported;

type Fits = Result<bool, Unsupported>;

/// Conservative structural subtyping, as upstream's `fits`: true only if every
/// value valid under `sub` is valid under `sup`.
///
/// Where upstream throws on a malformed schema, or on a `patternProperties`
/// pattern Rust's `regex` cannot compile, this answers `false`.
pub fn fits(sub: &Value, sup: &Value) -> bool {
    fits_inner(Some(sub), Some(sup)).unwrap_or(false)
}

/// [`fits`] for members that may be absent (`undefined` upstream).
pub(crate) fn fits_optional(sub: Option<&Value>, sup: Option<&Value>) -> bool {
    fits_inner(sub, sup).unwrap_or(false)
}

/// `asSchema`: objects as they are, `false` as `{not: {}}`, anything else as
/// `{}`. An array is an object to JavaScript; its entries are its indices.
fn as_schema(schema: Option<&Value>) -> Schema {
    match schema {
        Some(Value::Object(map)) => map.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(index, item)| (index.to_string(), item.clone()))
            .collect(),
        Some(Value::Bool(false)) => {
            let mut not = Schema::new();
            not.insert("not".to_owned(), Value::Object(Map::new()));
            not
        }
        _ => Schema::new(),
    }
}

fn without_annotations(mut schema: Schema) -> Schema {
    schema.retain(|key, _| !ANNOTATIONS.contains(&key.as_str()));
    schema
}

/// JavaScript's `key in object` on a plain object: own keys and the keys of
/// `Object.prototype`.
fn has(schema: &Schema, key: &str) -> bool {
    schema.contains_key(key) || js::OBJECT_PROTOTYPE_KEYS.contains(&key)
}

/// JavaScript's `object[key]` for a key found by [`has`]. An inherited
/// property is a function or `Object.prototype`, which `asSchema` turns into
/// `{}`.
fn get(schema: &Schema, key: &str) -> Option<Value> {
    match schema.get(key) {
        Some(value) => Some(value.clone()),
        None if js::OBJECT_PROTOTYPE_KEYS.contains(&key) => Some(Value::Object(Map::new())),
        None => None,
    }
}

/// `Array.prototype.includes` on primitives (SameValueZero); containers are
/// never the same object.
fn same_value_zero(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => js::number_value(a) == js::number_value(b),
        (Value::String(a), Value::String(b)) => a == b,
        _ => false,
    }
}

fn includes(values: &[Value], wanted: &str) -> bool {
    values.iter().any(|value| value.as_str() == Some(wanted))
}

/// `typesOf`: a string type as a one-item list, an array as it is.
fn types_of(schema: &Schema) -> Option<Vec<Value>> {
    match schema.get("type") {
        Some(Value::String(t)) => Some(vec![Value::String(t.clone())]),
        Some(Value::Array(types)) => Some(types.clone()),
        _ => None,
    }
}

/// `for (const x of value ?? [])`: arrays yield items and strings yield code
/// points; anything else is not iterable, which upstream throws on.
fn iterate(value: Option<&Value>) -> Result<Vec<Value>, Unsupported> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(Value::String(text)) => {
            Ok(text.chars().map(|c| Value::String(c.to_string())).collect())
        }
        Some(_) => Err(Unsupported),
    }
}

fn fits_inner(sub: Option<&Value>, sup: Option<&Value>) -> Fits {
    let sub = without_annotations(as_schema(sub));
    let sup = without_annotations(as_schema(sup));
    fits_schemas(&sub, &sup)
}

fn fits_schemas(sub: &Schema, sup: &Schema) -> Fits {
    if sup.is_empty() {
        return Ok(true);
    }
    let sub_value = Value::Object(sub.clone());
    let sup_value = Value::Object(sup.clone());
    if canonical_json(&sub_value) == canonical_json(&sup_value) {
        return Ok(true);
    }
    if sup.keys().any(|key| !known(key)) {
        return Ok(false);
    }
    if sup
        .get("additionalProperties")
        .is_some_and(|value| !value.is_boolean())
    {
        return Ok(false);
    }
    if let Some(Value::Array(branches)) = sub.get("anyOf") {
        for branch in branches {
            if !fits_inner(Some(branch), Some(&sup_value))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(Value::Array(branches)) = sup.get("anyOf") {
        for branch in branches {
            if fits_inner(Some(&sub_value), Some(branch))? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if sup.contains_key("const") {
        return Ok(sub.contains_key("const") && deep_equal(sub.get("const"), sup.get("const")));
    }
    if let Some(Value::Array(allowed)) = sup.get("enum") {
        let values: Option<Vec<&Value>> = if sub.contains_key("const") {
            Some(vec![sub.get("const").unwrap_or(&Value::Null)])
        } else if let Some(Value::Array(values)) = sub.get("enum") {
            Some(values.iter().collect())
        } else {
            None
        };
        return Ok(values.is_some_and(|values| {
            values
                .iter()
                .all(|item| allowed.iter().any(|ok| deep_equal(Some(ok), Some(item))))
        }));
    }
    for key in CONSTRAINTS {
        if sup.contains_key(key) && !deep_equal(sub.get(key), sup.get(key)) {
            return Ok(false);
        }
    }
    let (Some(sup_types), Some(sub_types)) = (types_of(sup), types_of(sub)) else {
        return Ok(false);
    };
    let covered = sub_types.iter().all(|t| {
        sup_types.iter().any(|s| same_value_zero(s, t))
            || (t.as_str() == Some("integer") && includes(&sup_types, "number"))
    });
    if !covered {
        return Ok(false);
    }
    if sub.contains_key("const") || matches!(sub.get("enum"), Some(Value::Array(_))) {
        return Ok(true);
    }
    if includes(&sub_types, "object") && !object_fits(sub, sup)? {
        return Ok(false);
    }
    if includes(&sub_types, "array") && !array_fits(sub, sup)? {
        return Ok(false);
    }
    if includes(&sub_types, "function") {
        return function_fits(sub, sup);
    }
    Ok(true)
}

/// `typeof value === "object"`: objects, arrays and null.
fn is_js_object(value: Option<&Value>) -> bool {
    matches!(
        value,
        Some(Value::Object(_) | Value::Array(_) | Value::Null)
    )
}

fn object_fits(sub: &Schema, sup: &Schema) -> Fits {
    let sub_properties = as_schema(sub.get("properties"));
    let sup_properties = as_schema(sup.get("properties"));
    let sub_required = iterate(sub.get("required"))?;
    for key in iterate(sup.get("required"))? {
        if !sub_required.iter().any(|have| same_value_zero(have, &key)) {
            return Ok(false);
        }
    }
    for (key, property) in &sup_properties {
        if has(&sub_properties, key) {
            if !fits_inner(get(&sub_properties, key).as_ref(), Some(property))? {
                return Ok(false);
            }
        } else if is_js_object(sub.get("additionalProperties"))
            && !fits_inner(sub.get("additionalProperties"), Some(property))?
        {
            // The consumer may send this name with any value its
            // `additionalProperties` allows.
            return Ok(false);
        }
    }
    let closed = Some(&Value::Bool(false));
    if sup.get("additionalProperties") == closed {
        if sub.get("additionalProperties") != closed {
            return Ok(false);
        }
        if sub_properties.keys().any(|key| !has(&sup_properties, key)) {
            return Ok(false);
        }
    }
    let sup_patterns = as_schema(sup.get("patternProperties"));
    if !sup_patterns.is_empty() {
        let sub_patterns = as_schema(sub.get("patternProperties"));
        if sub.get("additionalProperties") != closed && sub_patterns.is_empty() {
            return Ok(false);
        }
        for (pattern, property) in &sup_patterns {
            let matcher = regex::Regex::new(pattern).map_err(|_| Unsupported)?;
            for (key, sub_property) in &sub_properties {
                if !has(&sup_properties, key)
                    && matcher.is_match(key)
                    && !fits_inner(Some(sub_property), Some(property))?
                {
                    return Ok(false);
                }
            }
            if !sub_patterns.is_empty()
                && (!has(&sub_patterns, pattern)
                    || !fits_inner(get(&sub_patterns, pattern).as_ref(), Some(property))?)
            {
                return Ok(false);
            }
        }
        if sub_patterns
            .keys()
            .any(|pattern| !has(&sup_patterns, pattern))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn array_fits(sub: &Schema, sup: &Schema) -> Fits {
    if let Some(Value::Array(sup_items)) = sup.get("items") {
        let Some(Value::Array(sub_items)) = sub.get("items") else {
            return Ok(false);
        };
        if sub_items.len() != sup_items.len() {
            return Ok(false);
        }
        for (item, wider) in sub_items.iter().zip(sup_items) {
            if !fits_inner(Some(item), Some(wider))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(Value::Array(sub_items)) = sub.get("items") {
        for item in sub_items {
            if !fits_inner(Some(item), sup.get("items"))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    fits_inner(sub.get("items"), sup.get("items"))
}

fn function_fits(sub: &Schema, sup: &Schema) -> Fits {
    let parameters = |schema: &Schema| match schema.get("parameters") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(Unsupported),
    };
    let sub_parameters = parameters(sub)?;
    let sup_parameters = parameters(sup)?;
    // Fewer parameters are fine; each shared one is contravariant.
    if sub_parameters.len() > sup_parameters.len() {
        return Ok(false);
    }
    for (parameter, wider) in sub_parameters.iter().zip(&sup_parameters) {
        if !fits_inner(Some(wider), Some(parameter))? {
            return Ok(false);
        }
    }
    fits_inner(sub.get("returnType"), sup.get("returnType"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hash64_matches_upstream_on_known_values() {
        // Values printed by upstream's hash64 at the pinned commit.
        assert_eq!(hash64(""), "811c9dc55ad1e806");
        assert_eq!(hash64("a"), "e40c292c41cbd47c");
        assert_eq!(hash64("😀"), "cb31c4b823fca820");
    }

    #[test]
    fn canonical_json_sorts_keys_by_utf16() {
        let value = json!({"😀": 1, "\u{ff61}": 2, "a": [1.0, -0.0, 1e21], "": null});
        assert_eq!(
            canonical_json(&value),
            "{\"\":null,\"a\":[1,0,1e+21],\"😀\":1,\"\u{ff61}\":2}"
        );
    }

    #[test]
    fn an_uncompilable_pattern_does_not_fit() {
        let sub = json!({"type": "object", "properties": {"a": {"type": "number"}}, "additionalProperties": false});
        let sup = json!({"type": "object", "patternProperties": {"(?=a)": {"type": "number"}}});
        assert!(!fits(&sub, &sup));
    }

    #[test]
    fn a_malformed_required_list_does_not_fit() {
        let sub = json!({"type": "object", "required": 5});
        let sup = json!({"type": "object", "required": ["a"]});
        assert!(!fits(&sub, &sup));
    }
}
