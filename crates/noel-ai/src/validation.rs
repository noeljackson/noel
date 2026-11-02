//! Tool-argument validation with pi-schema's semantics, ported from
//! upstream's `packages/ai/src/utils/validation.ts` (the messages the
//! model sees) over a focused JSON-Schema subset (type, required,
//! properties, enum, const, items, additionalProperties, and the
//! number/string bounds), with TypeBox-superset coercion (upstream's
//! `coerce.ts`: `"42"` for a number, a single value for an array,
//! `null` for an omitted optional property).
//!
//! The error shape is upstream's, exactly:
//!
//! ```text
//! Validation failed for tool "grep":
//!   - pattern: missing
//!   - limit: expected number, got string
//! ```

use serde_json::{Map, Value};

/// One validation error: a JSON-Pointer path and pi-schema's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub path: String,
    pub message: String,
}

/// A JSON-Pointer path as the dotted path the model sees (upstream
/// `formatValidationPath`): `/edits/0/oldText` → `edits.0.oldText`.
pub fn dotted_path(pointer: &str) -> String {
    if pointer.is_empty() {
        return "root".to_string();
    }
    pointer
        .trim_start_matches('/')
        .split('/')
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

/// The validation error lines for `value` against `schema`, in
/// pi-schema's wording (upstream `explain.ts`, the subset this port
/// walks). Empty when the value conforms.
pub fn validate(schema: &Value, value: &Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    walk(schema, value, "", &mut errors);
    errors
}

/// `value` coerced per pi-schema's rules, or the errors when it cannot
/// conform: the two passes upstream's `parse()` runs (drop
/// optional-property `null`s, convert convertible types, then coerce the
/// rest: a single value into a one-element array, a number into a
/// string where a string is wanted).
pub fn parse(schema: &Value, value: Value) -> Result<Value, Vec<ValidationError>> {
    let value = drop_optional_nulls(schema, &value);
    let value = convert(schema, value);
    let errors = validate(schema, &value);
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors)
    }
}

/// The error a tool call reports (upstream `validateToolArguments`):
/// the full message the model sees, with the dotted paths, the
/// messages, and the received arguments.
pub fn tool_arguments_error(tool: &str, arguments: &Value, errors: &[ValidationError]) -> String {
    let lines = errors
        .iter()
        .map(|error| format!("  - {}: {}", dotted_path(&error.path), error.message))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Validation failed for tool \"{tool}\":\n{lines}\n\nReceived arguments:\n{}",
        serde_json::to_string_pretty(arguments).unwrap_or_default()
    )
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(wanted: &str, value: &Value) -> bool {
    match wanted {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        // An integer is a number without a fraction (pi-schema).
        "integer" => {
            value.as_i64().is_some()
                || (value.as_f64().is_some_and(|number| number.fract() == 0.0) && value.is_number())
        }
        "null" => value.is_null(),
        _ => true,
    }
}

fn wants_type(schema: &Value, kind: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(one)) => one == kind,
        Some(Value::Array(many)) => many.iter().any(|one| one.as_str() == Some(kind)),
        _ => false,
    }
}

fn walk(schema: &Value, value: &Value, path: &str, out: &mut Vec<ValidationError>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if object.get("type") == Some(&Value::Bool(false)) {
        return;
    }
    // type
    if let Some(types) = object.get("type") {
        let matches = match types {
            Value::String(one) => type_matches(one, value),
            Value::Array(many) => many
                .iter()
                .filter_map(Value::as_str)
                .any(|one| type_matches(one, value)),
            _ => true,
        };
        if !matches {
            let wanted = match types {
                Value::String(one) => one.clone(),
                Value::Array(many) => many
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" or "),
                _ => String::new(),
            };
            out.push(ValidationError {
                path: path.to_string(),
                message: format!("expected {wanted}, got {}", type_name(value)),
            });
            return;
        }
    }
    // enum / const
    if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            out.push(ValidationError {
                path: path.to_string(),
                message: format!(
                    "expected one of {}",
                    serde_json::to_string(allowed).unwrap_or_default()
                ),
            });
        }
    }
    if let Some(constant) = object.get("const") {
        if constant != value {
            out.push(ValidationError {
                path: path.to_string(),
                message: format!(
                    "expected {}",
                    serde_json::to_string(constant).unwrap_or_default()
                ),
            });
        }
    }
    // Number bounds.
    if let Some(number) = value.as_f64() {
        if let Some(minimum) = object.get("minimum").and_then(Value::as_f64) {
            if number < minimum {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected >= {minimum}"),
                });
            }
        }
        if let Some(maximum) = object.get("maximum").and_then(Value::as_f64) {
            if number > maximum {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected <= {maximum}"),
                });
            }
        }
    }
    // String bounds.
    if let Some(text) = value.as_str() {
        if let Some(min_length) = object.get("minLength").and_then(Value::as_u64) {
            if (text.chars().count() as u64) < min_length {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected at least {min_length} characters"),
                });
            }
        }
        if let Some(max_length) = object.get("maxLength").and_then(Value::as_u64) {
            if (text.chars().count() as u64) > max_length {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected at most {max_length} characters"),
                });
            }
        }
    }
    // Objects: required and properties.
    if let (Some(properties), Some(map)) = (
        object.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) {
        for key in object
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !map.contains_key(key) || map.get(key) == Some(&Value::Null) {
                out.push(ValidationError {
                    path: format!("{path}/{key}"),
                    message: "missing".to_string(),
                });
            }
        }
        for (key, sub) in properties {
            if let Some(child) = map.get(key) {
                if child != &Value::Null {
                    walk(sub, child, &format!("{path}/{key}"), out);
                }
            }
        }
        // additionalProperties: false reports unknown fields.
        if object.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in map.keys() {
                if !properties.contains_key(key) {
                    out.push(ValidationError {
                        path: format!("{path}/{key}"),
                        message: "unknown field".to_string(),
                    });
                }
            }
        }
    }
    // Arrays: items and bounds.
    if let (Some(items), Some(list)) = (object.get("items"), value.as_array()) {
        if let Some(min_items) = object.get("minItems").and_then(Value::as_u64) {
            if (list.len() as u64) < min_items {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected at least {min_items} items"),
                });
            }
        }
        if let Some(max_items) = object.get("maxItems").and_then(Value::as_u64) {
            if (list.len() as u64) > max_items {
                out.push(ValidationError {
                    path: path.to_string(),
                    message: format!("expected at most {max_items} items"),
                });
            }
        }
        for (index, item) in list.iter().enumerate() {
            walk(items, item, &format!("{path}/{index}"), out);
        }
    }
}

/// Drops `null` values of optional properties (upstream's
/// `dropOptionalNulls`): a model that sends `"limit": null` for an
/// omitted optional gets it omitted.
fn drop_optional_nulls(schema: &Value, value: &Value) -> Value {
    let (Some(properties), Some(map)) = (
        schema.get("properties").and_then(Value::as_object),
        value.as_object(),
    ) else {
        return value.clone();
    };
    let mut next = Map::new();
    let mut changed = false;
    for (key, child) in map {
        if child == &Value::Null
            && properties
                .get(key)
                .and_then(|sub| sub.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "null")
        {
            changed = true;
            continue;
        }
        // Recurse into object properties.
        if let Some(sub) = properties.get(key) {
            let recursed = drop_optional_nulls(sub, child);
            if recursed != *child {
                changed = true;
                next.insert(key.clone(), recursed);
                continue;
            }
        }
        next.insert(key.clone(), child.clone());
    }
    if changed {
        Value::Object(next)
    } else {
        value.clone()
    }
}

/// Converts convertible values (upstream's `convert`, the TypeBox
/// `Value.Convert` rules): `"42"` → `42` for a number or integer
/// property, `42` → `"42"` for a string one, a single value → a
/// one-element array where an array is wanted.
fn convert(schema: &Value, value: Value) -> Value {
    let Some(object) = schema.as_object() else {
        return value;
    };
    // Arrays: wrap a single value, convert each item.
    if wants_type(schema, "array") {
        let list = match value {
            Value::Array(list) => list,
            Value::Null => return value,
            other => vec![other],
        };
        let items = object.get("items");
        return Value::Array(
            list.into_iter()
                .map(|item| match items {
                    Some(items) => convert(items, item),
                    None => item,
                })
                .collect(),
        );
    }
    // Numbers from strings.
    if wants_type(schema, "number") || wants_type(schema, "integer") {
        if let Some(text) = value.as_str() {
            let trimmed = text.trim();
            if let Ok(number) = trimmed.parse::<i64>() {
                return Value::Number(number.into());
            }
            if let Ok(number) = trimmed.parse::<f64>() {
                if let Some(number) = serde_json::Number::from_f64(number) {
                    return Value::Number(number);
                }
            }
        }
    }
    // Strings from numbers and booleans.
    if wants_type(schema, "string") {
        match &value {
            Value::Number(number) => {
                return Value::String(number.to_string());
            }
            Value::Bool(flag) => {
                return Value::String(flag.to_string());
            }
            _ => {}
        }
    }
    // Booleans from strings.
    if wants_type(schema, "boolean") {
        if let Some(text) = value.as_str() {
            return match text.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value,
            };
        }
    }
    // Objects: convert each property against its schema.
    if let (Some(properties), Value::Object(map)) =
        (object.get("properties").and_then(Value::as_object), &value)
    {
        let mut next = Map::new();
        let mut changed = false;
        for (key, child) in map {
            if let Some(sub) = properties.get(key) {
                let converted = convert(sub, child.clone());
                if converted != *child {
                    changed = true;
                    next.insert(key.clone(), converted);
                    continue;
                }
            }
            next.insert(key.clone(), child.clone());
        }
        if changed {
            return Value::Object(next);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn missing_and_type_errors_read_like_upstreams() {
        let errors = validate(&schema(), &json!({"limit": "many"}));
        let messages = errors
            .iter()
            .map(|error| format!("{}: {}", dotted_path(&error.path), error.message))
            .collect::<Vec<_>>();
        assert!(
            messages.contains(&"path: missing".to_string()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"limit: expected integer, got string".to_string()),
            "{messages:?}"
        );
    }

    #[test]
    fn nested_paths_are_dotted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"oldText": {"type": "string"}},
                        "required": ["oldText"]
                    }
                }
            }
        });
        let errors = validate(&schema, &json!({"edits": [{}]}));
        assert_eq!(
            errors[0],
            ValidationError {
                path: "/edits/0/oldText".into(),
                message: "missing".into()
            }
        );
        assert_eq!(dotted_path(&errors[0].path), "edits.0.oldText");
    }

    #[test]
    fn coercion_converts_like_pi_schema() {
        // "42" for an integer.
        let value = parse(&schema(), json!({"path": "a", "limit": "42"})).unwrap();
        assert_eq!(value["limit"], json!(42));
        // A single value for an array.
        let value = parse(&schema(), json!({"path": "a", "tags": "x"})).unwrap();
        assert_eq!(value["tags"], json!(["x"]));
        // null for an omitted optional.
        let value = parse(&schema(), json!({"path": "a", "limit": null})).unwrap();
        assert!(value.get("limit").is_none());
        // A number for a string.
        let schema = json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
        let value = parse(&schema, json!({"path": 7})).unwrap();
        assert_eq!(value["path"], json!("7"));
    }

    #[test]
    fn inconvertible_values_are_errors_not_guesses() {
        let errors = parse(&schema(), json!({"limit": "many"})).unwrap_err();
        assert!(errors
            .iter()
            .any(|error| error.message.contains("expected integer, got string")));
    }

    #[test]
    fn bounds_and_unknown_fields() {
        let value = parse(&schema(), json!({"path": "a", "limit": 0})).unwrap_err();
        assert!(value.iter().any(|error| error.message == "expected >= 1"));
        let strict = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        });
        let errors = validate(&strict, &json!({"path": "a", "extra": 1}));
        assert!(errors.iter().any(|error| error.message == "unknown field"));
    }

    #[test]
    fn the_tool_error_message_is_upstreams_shape() {
        let (_, errors) = match parse(&schema(), json!({})) {
            Ok(_) => panic!("empty cannot conform"),
            Err(errors) => ((), errors),
        };
        let message = tool_arguments_error("grep", &json!({}), &errors);
        assert!(
            message.starts_with("Validation failed for tool \"grep\":\n"),
            "{message}"
        );
        assert!(message.contains("- path: missing"), "{message}");
        assert!(message.contains("Received arguments:"), "{message}");
    }
}
