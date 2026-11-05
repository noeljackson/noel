//! Canonical JSON, as upstream's `canonicalJson`.

use serde_json::Value;

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

/// Upstream's `deepEqual`: equal canonical JSON. An absent value compares as
/// `null`, because `canonicalJson(undefined)` is `"null"`.
pub(crate) fn deep_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    canonical_json(a.unwrap_or(&Value::Null)) == canonical_json(b.unwrap_or(&Value::Null))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_json_sorts_keys_by_utf16() {
        let value = json!({"😀": 1, "\u{ff61}": 2, "a": [1.0, -0.0, 1e21], "": null});
        assert_eq!(
            canonical_json(&value),
            "{\"\":null,\"a\":[1,0,1e+21],\"😀\":1,\"\u{ff61}\":2}"
        );
    }
}
