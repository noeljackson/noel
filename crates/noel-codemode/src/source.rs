//! The codemode source format (upstream `source.ts`): JavaScript, optionally
//! preceded by one options line.
//!
//! ```text
//! // @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
//! const text = await tools.read({ path: "package.json" });
//! ```

use serde_json::Value;

pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

const SUPPORTED_FIELDS: [&str; 2] = ["max_output_tokens", "timeout_ms"];
const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";
/// Largest delay `setTimeout` supports, which bounds `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;
/// `Number.MAX_SAFE_INTEGER`.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceOptions {
    /// Token budget for the script's output.
    pub max_output_tokens: Option<u64>,
    /// Hard deadline for the whole script in milliseconds, tool calls included.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodemodeSource {
    /// The script with the options line replaced by an empty line, so line
    /// numbers are unchanged.
    pub code: String,
    pub options: SourceOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CodemodeSourceError(pub String);

fn error(message: impl Into<String>) -> CodemodeSourceError {
    CodemodeSourceError(message.into())
}

/// A JSON number that is a non-negative safe integer, as `Number.isSafeInteger`
/// sees it (`1.0` is one).
fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_f64()?;
    ((0.0..=MAX_SAFE_INTEGER).contains(&number) && number.fract() == 0.0).then_some(number as u64)
}

fn parse_options(directive: &str) -> Result<SourceOptions, CodemodeSourceError> {
    let not_object = || {
        error(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        ))
    };
    if directive.is_empty() {
        return Err(not_object());
    }
    let value: Value = serde_json::from_str(directive).map_err(|parse| {
        error(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {parse}"
        ))
    })?;
    let Value::Object(fields) = value else {
        return Err(not_object());
    };
    if let Some(key) = fields
        .keys()
        .find(|key| !SUPPORTED_FIELDS.contains(&key.as_str()))
    {
        return Err(error(format!(
            "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
        )));
    }
    let mut options = SourceOptions::default();
    if let Some(value) = fields.get("max_output_tokens") {
        options.max_output_tokens = Some(safe_integer(value).ok_or_else(|| {
            error("@options field `max_output_tokens` must be a non-negative safe integer")
        })?);
    }
    if let Some(value) = fields.get("timeout_ms") {
        options.timeout_ms = Some(
            safe_integer(value)
                .filter(|ms| *ms > 0 && *ms <= MAX_TIMEOUT_MS)
                .ok_or_else(|| {
                    error(format!(
                        "@options field `timeout_ms` must be a positive integer up to {MAX_TIMEOUT_MS}"
                    ))
                })?,
        );
    }
    Ok(options)
}

/// Splits an optional first-line `// @options: {...}` from the script.
/// Refuses empty input, invalid options, and an options line that is not the
/// first line, which would otherwise be ignored without notice.
pub fn parse_codemode_source(input: &str) -> Result<ParsedCodemodeSource, CodemodeSourceError> {
    if input.trim().is_empty() {
        return Err(error(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        ));
    }
    let lines: Vec<&str> = input.split('\n').collect();
    if let Some(misplaced) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim_start().starts_with(CODEMODE_OPTIONS_PREFIX))
        .map(|(index, _)| index)
    {
        return Err(error(format!(
            "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
            misplaced + 1
        )));
    }
    let first_line = lines[0].strip_suffix('\r').unwrap_or(lines[0]);
    let trimmed = first_line.trim_start();
    let Some(directive) = trimmed.strip_prefix(CODEMODE_OPTIONS_PREFIX) else {
        return Ok(ParsedCodemodeSource {
            code: input.to_owned(),
            options: SourceOptions::default(),
        });
    };
    let code = input.find('\n').map_or("", |newline| &input[newline..]);
    if code.trim().is_empty() {
        return Err(error(
            "The @options line must be followed by JavaScript source on subsequent lines",
        ));
    }
    Ok(ParsedCodemodeSource {
        code: code.to_owned(),
        options: parse_options(directive.trim())?,
    })
}

#[cfg(test)]
mod tests {
    //! Upstream `test/source.test.ts`, case for case.
    use super::*;

    fn options(timeout_ms: Option<u64>, max_output_tokens: Option<u64>) -> SourceOptions {
        SourceOptions {
            max_output_tokens,
            timeout_ms,
        }
    }

    #[test]
    fn returns_plain_code_unchanged() {
        let parsed = parse_codemode_source("text('hi')").unwrap();
        assert_eq!(parsed.code, "text('hi')");
        assert_eq!(parsed.options, SourceOptions::default());
        let parsed = parse_codemode_source("// just a comment\nreturn 1").unwrap();
        assert_eq!(parsed.code, "// just a comment\nreturn 1");
    }

    #[test]
    fn parses_the_options_line_and_keeps_line_numbers() {
        let parsed =
            parse_codemode_source("// @options: {\"timeout_ms\": 10}\nconst a = 1;\ntext(a)")
                .unwrap();
        assert_eq!(parsed.code, "\nconst a = 1;\ntext(a)");
        assert_eq!(parsed.options, options(Some(10), None));
        let parsed = parse_codemode_source(
            "  // @options:{\"max_output_tokens\":0,\"timeout_ms\":1500}\r\ntext(1)",
        )
        .unwrap();
        assert_eq!(parsed.options, options(Some(1500), Some(0)));
        let parsed = parse_codemode_source("// @options: {}\ntext(1)").unwrap();
        assert_eq!(parsed.code, "\ntext(1)");
        assert_eq!(parsed.options, SourceOptions::default());
    }

    #[test]
    fn only_treats_the_first_line_as_an_options_line() {
        assert_eq!(
            parse_codemode_source("// @optionsx {}\ntext(1)")
                .unwrap()
                .options,
            SourceOptions::default()
        );
        assert_eq!(
            parse_codemode_source("text(\"// @options: {}\")")
                .unwrap()
                .options,
            SourceOptions::default()
        );
    }

    #[test]
    fn rejects_an_options_line_that_is_not_the_first_line() {
        let cases = [
            (
                "// count things\n// @options: {\"timeout_ms\": 1000}\nwhile (true) {}",
                2,
            ),
            (
                "\n  // @options: {\"timeout_ms\": 1000}\nwhile (true) {}",
                2,
            ),
            ("text(1)\r\n// @options: {\"timeout_ms\": 1}", 2),
            (
                "// @options: {\"timeout_ms\": 1}\ntext(1)\n// @options: {\"timeout_ms\": 2}",
                3,
            ),
        ];
        for (input, line) in cases {
            let message = parse_codemode_source(input).unwrap_err().0;
            assert_eq!(
                message,
                format!("The // @options: line must be the first line of the input; found one on line {line}"),
                "{input:?}"
            );
        }
    }

    #[test]
    fn rejects_empty_input_and_invalid_options() {
        let cases = [
            ("", "Expected JavaScript source text (non-empty)"),
            ("  \n", "Expected JavaScript source text (non-empty)"),
            (
                "// @options:\ntext(1)",
                "@options must be a JSON object with supported fields",
            ),
            (
                "// @options: {timeout_ms: 1}\ntext(1)",
                "@options must be valid JSON with supported fields",
            ),
            (
                "// @options: [1]\ntext(1)",
                "@options must be a JSON object with supported fields",
            ),
            (
                "// @options: {\"yield\": 1}\ntext(1)",
                "@options only supports `max_output_tokens` and `timeout_ms`; got `yield`",
            ),
            (
                "// @options: {\"max_output_tokens\": 1.5}\ntext(1)",
                "@options field `max_output_tokens` must be a non-negative safe integer",
            ),
            (
                "// @options: {\"timeout_ms\": 0}\ntext(1)",
                "@options field `timeout_ms` must be a positive integer",
            ),
            (
                "// @options: {\"timeout_ms\": 1}",
                "The @options line must be followed by JavaScript source on subsequent lines",
            ),
            (
                "// @options: {\"timeout_ms\": 1}\n  \n",
                "The @options line must be followed by JavaScript source on subsequent lines",
            ),
        ];
        for (input, message) in cases {
            let error = parse_codemode_source(input).unwrap_err().0;
            assert!(error.contains(message), "{input:?}: {error}");
        }
    }

    #[test]
    fn a_whole_number_written_as_a_float_is_an_integer() {
        let parsed = parse_codemode_source("// @options: {\"timeout_ms\": 5.0}\ntext(1)").unwrap();
        assert_eq!(parsed.options, options(Some(5), None));
    }
}
