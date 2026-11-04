//! Values against JSON Schemas: the subset of a JSON Schema check that
//! contract schemas use, for params, arguments and state.
//!
//! Keywords checked: `type` (one or a list), `const`, `enum`, `anyOf`,
//! `oneOf`, `allOf`, `not`, `properties`, `required`, `additionalProperties`,
//! `patternProperties`, `minProperties`, `maxProperties`, `items` (one schema
//! or a tuple), `prefixItems`, `minItems`, `maxItems`, `uniqueItems`,
//! `minLength` and `maxLength` (in UTF-16 code units), `pattern`, `minimum`,
//! `maximum`, `exclusiveMinimum`, `exclusiveMaximum` and `multipleOf`. Other
//! keywords (`format`, `$ref`, TypeBox's function types) are not checked.

use regex::Regex;
use serde_json::{Map, Value};

use crate::js;
use crate::schema::{canonical_json, deep_equal};

/// The first problem with `value`, as `path message`, or `Ok`.
pub fn check(schema: &Value, value: &Value) -> Result<(), String> {
    let mut path = String::new();
    check_at(schema, value, &mut path)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_type(value: &Value, wanted: &str) -> bool {
    match wanted {
        "integer" => match value {
            Value::Number(number) => {
                let n = js::number_value(number);
                n.is_finite() && n.fract() == 0.0
            }
            _ => false,
        },
        "number" => value.is_number(),
        other => type_name(value) == other,
    }
}

fn fail(path: &str, message: impl AsRef<str>) -> Result<(), String> {
    let path = if path.is_empty() { "/" } else { path };
    Err(format!("{path} {}", message.as_ref()))
}

fn number(value: &Value) -> Option<f64> {
    value.as_number().map(js::number_value)
}

fn check_at(schema: &Value, value: &Value, path: &mut String) -> Result<(), String> {
    let schema = match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return fail(path, "must not exist"),
        Value::Object(schema) => schema,
        _ => return Ok(()),
    };
    if let Some(wanted) = schema.get("type") {
        let types: Vec<&str> = match wanted {
            Value::String(one) => vec![one.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|wanted| is_type(value, wanted)) {
            return fail(path, format!("must be {}", types.join(" or ")));
        }
    }
    if let Some(constant) = schema.get("const") {
        if !deep_equal(Some(constant), Some(value)) {
            return fail(
                path,
                format!("must be equal to constant {}", canonical_json(constant)),
            );
        }
    }
    if let Some(Value::Array(options)) = schema.get("enum") {
        if !options
            .iter()
            .any(|option| deep_equal(Some(option), Some(value)))
        {
            return fail(path, "must be equal to one of the allowed values");
        }
    }
    if let Some(Value::Array(branches)) = schema.get("anyOf") {
        if !branches.iter().any(|branch| check(branch, value).is_ok()) {
            return fail(path, "must match a schema in anyOf");
        }
    }
    if let Some(Value::Array(branches)) = schema.get("oneOf") {
        if branches
            .iter()
            .filter(|branch| check(branch, value).is_ok())
            .count()
            != 1
        {
            return fail(path, "must match exactly one schema in oneOf");
        }
    }
    if let Some(Value::Array(branches)) = schema.get("allOf") {
        for branch in branches {
            check_at(branch, value, path)?;
        }
    }
    if let Some(not) = schema.get("not") {
        if check(not, value).is_ok() {
            return fail(path, "must not match the schema in not");
        }
    }
    match value {
        Value::Number(_) => check_number(schema, value, path)?,
        Value::String(text) => check_string(schema, text, path)?,
        Value::Array(items) => check_array(schema, items, path)?,
        Value::Object(map) => check_object(schema, map, path)?,
        _ => {}
    }
    Ok(())
}

fn check_number(schema: &Map<String, Value>, value: &Value, path: &str) -> Result<(), String> {
    let n = number(value).unwrap_or(f64::NAN);
    if let Some(minimum) = schema.get("minimum").and_then(number) {
        if n < minimum {
            return fail(
                path,
                format!("must be >= {}", js::number_to_string(minimum)),
            );
        }
    }
    if let Some(maximum) = schema.get("maximum").and_then(number) {
        if n > maximum {
            return fail(
                path,
                format!("must be <= {}", js::number_to_string(maximum)),
            );
        }
    }
    if let Some(minimum) = schema.get("exclusiveMinimum").and_then(number) {
        if n <= minimum {
            return fail(path, format!("must be > {}", js::number_to_string(minimum)));
        }
    }
    if let Some(maximum) = schema.get("exclusiveMaximum").and_then(number) {
        if n >= maximum {
            return fail(path, format!("must be < {}", js::number_to_string(maximum)));
        }
    }
    if let Some(divisor) = schema.get("multipleOf").and_then(number) {
        if divisor != 0.0 && (n / divisor).fract() != 0.0 {
            return fail(
                path,
                format!("must be a multiple of {}", js::number_to_string(divisor)),
            );
        }
    }
    Ok(())
}

fn check_string(schema: &Map<String, Value>, text: &str, path: &str) -> Result<(), String> {
    let length = js::utf16_len(text) as f64;
    if let Some(minimum) = schema.get("minLength").and_then(number) {
        if length < minimum {
            return fail(
                path,
                format!(
                    "must not have fewer than {} characters",
                    js::number_to_string(minimum)
                ),
            );
        }
    }
    if let Some(maximum) = schema.get("maxLength").and_then(number) {
        if length > maximum {
            return fail(
                path,
                format!(
                    "must not have more than {} characters",
                    js::number_to_string(maximum)
                ),
            );
        }
    }
    if let Some(Value::String(pattern)) = schema.get("pattern") {
        // A pattern Rust's regex cannot compile is not checked.
        if let Ok(regex) = Regex::new(pattern) {
            if !regex.is_match(text) {
                return fail(path, format!("must match pattern \"{pattern}\""));
            }
        }
    }
    Ok(())
}

fn check_array(
    schema: &Map<String, Value>,
    items: &[Value],
    path: &mut String,
) -> Result<(), String> {
    let length = items.len() as f64;
    if let Some(minimum) = schema.get("minItems").and_then(number) {
        if length < minimum {
            return fail(
                path,
                format!(
                    "must not have fewer than {} items",
                    js::number_to_string(minimum)
                ),
            );
        }
    }
    if let Some(maximum) = schema.get("maxItems").and_then(number) {
        if length > maximum {
            return fail(
                path,
                format!(
                    "must not have more than {} items",
                    js::number_to_string(maximum)
                ),
            );
        }
    }
    let tuple = match (schema.get("prefixItems"), schema.get("items")) {
        (Some(Value::Array(tuple)), _) | (None, Some(Value::Array(tuple))) => Some(tuple),
        _ => None,
    };
    let rest = match (tuple, schema.get("items"), schema.get("additionalItems")) {
        (None, Some(items), _) => Some(items),
        (Some(_), Some(items), _) if !items.is_array() => Some(items),
        (Some(_), _, Some(additional)) => Some(additional),
        _ => None,
    };
    for (index, item) in items.iter().enumerate() {
        let item_schema = match tuple.and_then(|tuple| tuple.get(index)) {
            Some(schema) => Some(schema),
            None => rest,
        };
        if let Some(item_schema) = item_schema {
            let length = path.len();
            path.push('/');
            path.push_str(&index.to_string());
            let result = check_at(item_schema, item, path);
            path.truncate(length);
            result?;
        }
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
        let mut seen = std::collections::HashSet::new();
        for item in items {
            if !seen.insert(canonical_json(item)) {
                return fail(path, "must not have duplicate items");
            }
        }
    }
    Ok(())
}

fn check_object(
    schema: &Map<String, Value>,
    map: &Map<String, Value>,
    path: &mut String,
) -> Result<(), String> {
    let count = map.len() as f64;
    if let Some(minimum) = schema.get("minProperties").and_then(number) {
        if count < minimum {
            return fail(
                path,
                format!(
                    "must not have fewer than {} properties",
                    js::number_to_string(minimum)
                ),
            );
        }
    }
    if let Some(maximum) = schema.get("maxProperties").and_then(number) {
        if count > maximum {
            return fail(
                path,
                format!(
                    "must not have more than {} properties",
                    js::number_to_string(maximum)
                ),
            );
        }
    }
    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required.iter().filter_map(Value::as_str) {
            if !map.contains_key(key) {
                return fail(path, format!("must have required property '{key}'"));
            }
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    let patterns: Vec<(Regex, &Value)> = schema
        .get("patternProperties")
        .and_then(Value::as_object)
        .map(|patterns| {
            patterns
                .iter()
                .filter_map(|(pattern, schema)| {
                    Regex::new(pattern).ok().map(|regex| (regex, schema))
                })
                .collect()
        })
        .unwrap_or_default();
    let additional = schema.get("additionalProperties");
    for (key, item) in map {
        let length = path.len();
        path.push('/');
        path.push_str(&key.replace('~', "~0").replace('/', "~1"));
        let mut known = false;
        let mut result = Ok(());
        if let Some(property) = properties.and_then(|properties| properties.get(key)) {
            known = true;
            result = check_at(property, item, path);
        }
        for (regex, pattern_schema) in &patterns {
            if result.is_ok() && regex.is_match(key) {
                known = true;
                result = check_at(pattern_schema, item, path);
            }
        }
        if result.is_ok() && !known {
            match additional {
                Some(Value::Bool(false)) => {
                    result = fail(path, "must not be an additional property")
                }
                Some(additional @ Value::Object(_)) => result = check_at(additional, item, path),
                _ => {}
            }
        }
        path.truncate(length);
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn checks_the_keywords_contracts_use() {
        let count =
            json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}});
        assert!(check(&count, &json!({"n": 1})).is_ok());
        assert!(check(&count, &json!({"n": 1.0})).is_ok());
        assert_eq!(
            check(&count, &json!({"n": 1.5})),
            Err("/n must be integer".to_owned())
        );
        assert!(check(&count, &json!({})).is_err());
        assert!(check(&json!({"type": "integer"}), &json!("x")).is_err());
        let log = json!({"type": "array", "items": {"type": "string"}});
        assert!(check(&log, &json!(["a", "b"])).is_ok());
        assert_eq!(
            check(&log, &json!(["a", 2])),
            Err("/1 must be string".to_owned())
        );
        assert!(check(&json!({}), &json!(null)).is_ok());
        let union = json!({"anyOf": [{"const": "a"}, {"const": "b"}]});
        assert!(check(&union, &json!("b")).is_ok());
        assert!(check(&union, &json!("c")).is_err());
        let strict = json!({"type": "object", "properties": {}, "additionalProperties": false});
        assert!(check(&strict, &json!({"x": 1})).is_err());
        assert!(check(&json!({"type": "string", "minLength": 2}), &json!("😀")).is_ok());
    }
}
