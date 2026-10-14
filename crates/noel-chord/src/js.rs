//! JavaScript semantics that upstream's TypeScript gets from the language:
//! number formatting, `JSON.stringify` string escaping, `String(value)`,
//! UTF-16 lengths and comparisons, and JavaScript property order.

use std::cmp::Ordering;
use std::fmt::Write as _;

use serde_json::{Number, Value};

/// A JSON number as JavaScript holds it: every number is an `f64`.
pub(crate) fn number_value(number: &Number) -> f64 {
    // Without `arbitrary_precision` every serde_json number has an f64 view;
    // an integer is rounded to the nearest double, as JavaScript parses it.
    number.as_f64().unwrap_or(f64::NAN)
}

/// `Number.prototype.toString()`, which is also how `JSON.stringify` writes a
/// finite number (ECMA-262 `Number::toString`).
pub(crate) fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == 0.0 {
        // Both zeros print as "0".
        return "0".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    // Rust's `{:e}` prints the shortest digit string that round-trips, as
    // JavaScript requires; only the layout differs.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp output has an exponent");
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent + 1;
    let mut out = String::new();
    if value < 0.0 {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{}", if n > 0 { '+' } else { '-' }, (n - 1).abs());
    }
    out
}

/// `JSON.stringify(text)`: quotes, backslashes and control characters are
/// escaped; everything else, including U+2028, U+2029 and DEL, is written as
/// it is.
pub(crate) fn quote_into(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `String(value)` for a JSON value, as error messages print it.
pub(crate) fn display(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_string(number_value(n)),
        Value::String(s) => s.clone(),
        // `Array.prototype.join`: null items print as empty strings.
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                item => display(item),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `text.length`: the number of UTF-16 code units.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The default `Array.prototype.sort` order for strings: by UTF-16 code
/// units, which differs from byte order above U+FFFF.
pub(crate) fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `Number.isInteger(value) && value >= 0` (so `-0` passes), as a count or
/// index. Huge integers saturate; every caller clamps them anyway.
pub(crate) fn non_negative_integer(value: &Value) -> Option<u64> {
    let Value::Number(number) = value else {
        return None;
    };
    let value = number_value(number);
    (value.is_finite() && value.fract() == 0.0 && value >= 0.0).then_some(value as u64)
}

/// A canonical array index: the property keys JavaScript orders first.
pub(crate) fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|index| *index < u64::from(u32::MAX))
        .map(|index| index as u32)
}

/// Where a new key goes in JavaScript property order: array indices first,
/// ascending, then other keys in insertion order.
pub(crate) fn property_insert_at<'a>(
    keys: impl Iterator<Item = &'a str>,
    key: &str,
) -> Option<usize> {
    let index = array_index(key)?;
    let mut position = 0;
    for existing in keys {
        match array_index(existing) {
            Some(other) if other < index => position += 1,
            _ => break,
        }
    }
    Some(position)
}

/// Properties every plain object inherits from `Object.prototype`, which
/// JavaScript's `in` operator finds on any object.
pub(crate) const OBJECT_PROTOTYPE_KEYS: [&str; 12] = [
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
    "__proto__",
    "constructor",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_as_javascript_prints_them() {
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (2.5e-5, "0.000025"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(number_to_string(value), expected, "{value:e}");
        }
    }

    #[test]
    fn strings_quote_as_json_stringify_does() {
        let mut out = String::new();
        quote_into("a\"\\/\u{0}\u{1f}\u{7f}\u{2028}\t😀", &mut out);
        assert_eq!(out, "\"a\\\"\\\\/\\u0000\\u001f\u{7f}\u{2028}\\t😀\"");
    }

    #[test]
    fn utf16_order_puts_astral_characters_before_the_top_of_the_bmp() {
        assert_eq!(utf16_cmp("😀", "\u{ff61}"), Ordering::Less);
        assert_eq!("😀".cmp("\u{ff61}"), Ordering::Greater);
    }

    #[test]
    fn array_indices_are_canonical_u32_strings() {
        assert_eq!(array_index("0"), Some(0));
        assert_eq!(array_index("4294967294"), Some(4294967294));
        assert_eq!(array_index("4294967295"), None);
        assert_eq!(array_index("01"), None);
        assert_eq!(array_index("-1"), None);
        assert_eq!(array_index("1.0"), None);
    }
}
