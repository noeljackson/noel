//! Frontmatter of skill and prompt template files, as upstream reads it
//! (`utils/frontmatter.ts`): the text between a leading `---` line and the
//! next line that starts with `---`, and the body after it.
//!
//! Upstream parses that text with a YAML library. This is a parser for the
//! part of YAML the Agent Skills specification uses: one mapping whose
//! values are plain or quoted scalars, booleans, or `|` / `>` block scalars.
//! A nested mapping, a sequence, or a flow collection is accepted as a value
//! that is neither a string nor a boolean. Anything else, such as anchors,
//! aliases, and tags, is malformed here. `skills.json` holds upstream's
//! reading of the cases the tests below compare.

/// A top-level frontmatter value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    String(String),
    Bool(bool),
    /// A null, a number, a mapping, or a sequence.
    Other,
}

/// A file's frontmatter: its top-level keys, in order. Empty when the file
/// has none or it is not a mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    fields: Vec<(String, Value)>,
}

impl Frontmatter {
    /// The string value of `key`; `None` when it has another type.
    pub fn string(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    /// Whether `key` is the boolean `true`.
    pub fn is_true(&self, key: &str) -> bool {
        self.get(key) == Some(&Value::Bool(true))
    }

    fn get(&self, key: &str) -> Option<&Value> {
        let field = self.fields.iter().find(|(name, _)| name == key)?;
        Some(&field.1)
    }
}

/// The frontmatter and body of `content`. The body is trimmed when there is
/// frontmatter and returned whole otherwise, with line endings as `\n` and
/// without a byte order mark. `Err` when the frontmatter is malformed.
pub fn parse_frontmatter(content: &str) -> Result<(Frontmatter, String), String> {
    let (yaml, body) = split_frontmatter(content);
    let fields = match yaml {
        Some(yaml) => parse_document(&yaml)?.unwrap_or_default(),
        None => Vec::new(),
    };
    Ok((Frontmatter { fields }, body))
}

/// The body of `content`: [`parse_frontmatter`] without the fields.
pub fn strip_frontmatter(content: &str) -> Result<String, String> {
    parse_frontmatter(content).map(|(_, body)| body)
}

/// The frontmatter text, if any, and the body.
fn split_frontmatter(content: &str) -> (Option<String>, String) {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return (None, normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|index| index + 3) else {
        return (None, normalized);
    };
    // Upstream takes the text from the fifth character on, whatever the
    // fourth is.
    let mut yaml = normalized[3..end].chars();
    yaml.next();
    let body = js_trim(&normalized[end + 4..]).to_string();
    (Some(yaml.as_str().to_string()), body)
}

/// Whether JavaScript's `\s` and `String.prototype.trim` treat `ch` as
/// white space. Unlike Rust's, that includes U+FEFF and excludes U+0085.
pub fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `text` as JavaScript's `trim` leaves it.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// Parses a YAML document: its top-level fields, or `None` when it is not a
/// mapping (a scalar or a sequence), which upstream reads as no fields.
fn parse_document(yaml: &str) -> Result<Option<Vec<(String, Value)>>, String> {
    let mut parser = Parser {
        lines: yaml.lines().collect(),
        at: 0,
    };
    parser.skip_blank();
    let Some(first) = parser.peek() else {
        return Ok(Some(Vec::new()));
    };
    let top = indent(first)?;
    let text = &first[top..];
    if text.starts_with('%') {
        return Err("directives are not supported".into());
    }
    if text.starts_with('{') {
        return Err("a flow mapping document is not supported".into());
    }
    if is_sequence_entry(text) || text.starts_with('[') || split_key(text)?.is_none() {
        return Ok(None);
    }

    let mut fields: Vec<(String, Value)> = Vec::new();
    while let Some(line) = parser.peek() {
        if is_blank(line) {
            parser.at += 1;
            continue;
        }
        if is_document_end(line) {
            parser.at += 1;
            parser.skip_blank();
            if parser.peek().is_some() {
                return Err("the frontmatter holds more than one document".into());
            }
            break;
        }
        if indent(line)? != top {
            return Err("all mapping items must start at the same column".into());
        }
        let Some((key, rest)) = split_key(&line[top..])? else {
            return Err("a mapping key must be followed by a value".into());
        };
        parser.at += 1;
        let value = parser.value(rest, top)?;
        if fields.iter().any(|(name, _)| *name == key) {
            return Err(format!("map keys must be unique: {key}"));
        }
        fields.push((key, value));
    }
    Ok(Some(fields))
}

struct Parser<'a> {
    lines: Vec<&'a str>,
    /// The next line to read.
    at: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.at).copied()
    }

    /// Skips blank lines and comments.
    fn skip_blank(&mut self) {
        while self.peek().is_some_and(is_blank) {
            self.at += 1;
        }
    }

    /// The value of a key at column `parent` whose line continues with
    /// `rest`.
    fn value(&mut self, rest: &'a str, parent: usize) -> Result<Value, String> {
        let text = rest.trim_start_matches([' ', '\t']);
        if !text.is_empty() && !text.starts_with('#') {
            return self.scalar(text, parent);
        }
        // Nothing on the key's line: the value starts on a later line, or
        // is null.
        let mut next = self.at;
        while self.lines.get(next).copied().is_some_and(is_blank) {
            next += 1;
        }
        let Some(line) = self.lines.get(next).copied() else {
            return Ok(Value::Other);
        };
        if is_document_end(line) {
            return Ok(Value::Other);
        }
        let column = indent(line)?;
        let text = &line[column..];
        if column == parent && is_sequence_entry(text) {
            // A sequence may sit at its key's column.
            self.at = next;
            while let Some(line) = self.peek().filter(|line| !is_document_end(line)) {
                if !is_blank(line) {
                    let column = indent(line)?;
                    let entry = column == parent && is_sequence_entry(&line[column..]);
                    if column <= parent && !entry {
                        break;
                    }
                }
                self.at += 1;
            }
            return Ok(Value::Other);
        }
        if column <= parent {
            return Ok(Value::Other);
        }
        if is_sequence_entry(text) || split_key(text)?.is_some() {
            self.at = next;
            self.skip_nested(parent)?;
            return Ok(Value::Other);
        }
        self.at = next + 1;
        self.scalar(text, parent)
    }

    /// Skips a nested mapping or sequence: every line indented past
    /// `parent`.
    fn skip_nested(&mut self, parent: usize) -> Result<(), String> {
        while let Some(line) = self.peek() {
            if !is_blank(line) {
                if line.starts_with('\t') {
                    return Err("tabs are not allowed as indentation".into());
                }
                if leading_spaces(line) <= parent {
                    break;
                }
            }
            self.at += 1;
        }
        Ok(())
    }

    /// A value that starts with `text`, the rest of the current line.
    fn scalar(&mut self, text: &'a str, parent: usize) -> Result<Value, String> {
        let mut chars = text.chars();
        let first = chars.next().unwrap_or(' ');
        let second = chars.next();
        let before_space = second.is_none_or(|ch| ch == ' ' || ch == '\t');
        match first {
            '|' | '>' => self.block_scalar(text, parent).map(Value::String),
            '"' | '\'' => self.quoted(text).map(Value::String),
            '[' | '{' => self.flow(text).map(|()| Value::Other),
            '&' | '*' | '!' => Err("anchors, aliases, and tags are not supported".into()),
            '@' | '`' | '%' | ',' | ']' | '}' => {
                Err(format!("a plain value cannot start with {first}"))
            }
            '-' | '?' | ':' if before_space => {
                Err(format!("unexpected {first} in a mapping value"))
            }
            _ => self.plain(text, parent),
        }
    }

    /// A plain scalar: `text`, and the more indented lines that continue it.
    fn plain(&mut self, text: &'a str, parent: usize) -> Result<Value, String> {
        let (mut value, mut commented) = plain_line(text)?;
        loop {
            let mut next = self.at;
            let mut blank_lines = 0;
            while self
                .lines
                .get(next)
                .is_some_and(|line| line.trim().is_empty())
            {
                blank_lines += 1;
                next += 1;
            }
            let Some(line) = self.lines.get(next).copied() else {
                break;
            };
            let column = indent(line)?;
            let text = &line[column..];
            // A comment line ends the scalar; text after it is an error the
            // caller reports.
            if column <= parent || text.starts_with('#') {
                break;
            }
            if commented {
                return Err("unexpected text after a comment".into());
            }
            let (line, comment) = plain_line(text)?;
            if blank_lines == 0 {
                value.push(' ');
            } else {
                value.push_str(&"\n".repeat(blank_lines));
            }
            value.push_str(&line);
            commented = comment;
            self.at = next + 1;
        }
        Ok(resolve_plain(value))
    }

    /// A quoted scalar that opens at the start of `text`, which may close on
    /// a later line.
    fn quoted(&mut self, text: &'a str) -> Result<String, String> {
        let quote = text.chars().next().unwrap_or('"');
        let mut value = String::new();
        let mut line = &text[1..];
        loop {
            // Where the line's text ends without its trailing white space.
            let mut kept = value.len();
            let mut chars = line.char_indices();
            let mut continued = false;
            while let Some((index, ch)) = chars.next() {
                if ch == quote {
                    if quote == '\'' && line[index + 1..].starts_with('\'') {
                        chars.next();
                        value.push('\'');
                        kept = value.len();
                        continue;
                    }
                    let rest = &line[index + 1..];
                    let trimmed = rest.trim_start_matches([' ', '\t']);
                    let comment = trimmed.starts_with('#') && trimmed.len() < rest.len();
                    if !trimmed.is_empty() && !comment {
                        return Err("unexpected text after a quoted value".into());
                    }
                    return Ok(value);
                }
                if quote == '"' && ch == '\\' {
                    match chars.next() {
                        Some((_, escape)) => value.push(unescape(escape, &mut chars)?),
                        // An escaped line break joins the next line as is.
                        None => continued = true,
                    }
                    kept = value.len();
                    continue;
                }
                value.push(ch);
                if ch != ' ' && ch != '\t' {
                    kept = value.len();
                }
            }
            // The value continues: line breaks fold as in a plain scalar.
            if !continued {
                value.truncate(kept);
            }
            let mut blank_lines = 0;
            line = loop {
                let Some(next) = self.peek() else {
                    return Err(format!("missing closing {quote} quote"));
                };
                self.at += 1;
                if !next.trim().is_empty() {
                    break next.trim_start_matches([' ', '\t']);
                }
                blank_lines += 1;
            };
            if blank_lines > 0 {
                value.push_str(&"\n".repeat(blank_lines));
            } else if !continued {
                value.push(' ');
            }
        }
    }

    /// A `|` or `>` block scalar whose header is `text`.
    fn block_scalar(&mut self, text: &str, parent: usize) -> Result<String, String> {
        let folded = text.starts_with('>');
        let mut chomp = None;
        let mut explicit = None;
        let mut header = text[1..].chars().peekable();
        while let Some(&ch) = header.peek() {
            match ch {
                '+' | '-' if chomp.is_none() => chomp = Some(ch),
                '1'..='9' if explicit.is_none() => explicit = ch.to_digit(10),
                _ => break,
            }
            header.next();
        }
        let rest = header.collect::<String>();
        let comment = rest.trim_start_matches([' ', '\t']);
        if !comment.is_empty() && !(comment.starts_with('#') && comment.len() < rest.len()) {
            return Err("a block scalar header includes extra characters".into());
        }

        let first_text = self.lines[self.at..]
            .iter()
            .find(|line| !line.trim().is_empty());
        let block = match explicit {
            Some(digit) => parent + digit as usize,
            None => first_text.map_or(0, |line| leading_spaces(line)),
        };
        let mut value = String::new();
        let mut read_text = false;
        let mut blank_lines = 0;
        let mut more_indented = false;
        while let Some(line) = self.peek() {
            if line.trim().is_empty() {
                blank_lines += 1;
                self.at += 1;
                continue;
            }
            if block <= parent || leading_spaces(line) < block {
                break;
            }
            let line = &line[block..];
            let breaks = if !folded {
                usize::from(read_text) + blank_lines
            } else if line.starts_with([' ', '\t']) {
                more_indented = true;
                usize::from(read_text) + blank_lines
            } else if std::mem::take(&mut more_indented) {
                blank_lines + 1
            } else {
                blank_lines
            };
            if folded && breaks == 0 && read_text {
                value.push(' ');
            }
            value.push_str(&"\n".repeat(breaks));
            value.push_str(line);
            read_text = true;
            blank_lines = 0;
            self.at += 1;
        }
        match chomp {
            Some('-') => {}
            Some(_) => value.push_str(&"\n".repeat(usize::from(read_text) + blank_lines)),
            None if read_text => value.push('\n'),
            None => {}
        }
        Ok(value)
    }

    /// Skips a `[...]` or `{...}` collection that opens at the start of
    /// `text`.
    fn flow(&mut self, text: &'a str) -> Result<(), String> {
        let mut depth = 0usize;
        let mut quote = None;
        let mut line = text;
        loop {
            let mut chars = line.char_indices();
            while let Some((index, ch)) = chars.next() {
                match quote {
                    Some('"') if ch == '\\' => {
                        chars.next();
                    }
                    Some(open) if ch == open => quote = None,
                    Some(_) => {}
                    None => match ch {
                        '"' | '\'' => quote = Some(ch),
                        '[' | '{' => depth += 1,
                        ']' | '}' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                let rest = line[index + 1..].trim_start_matches([' ', '\t']);
                                if !rest.is_empty() && !rest.starts_with('#') {
                                    return Err("unexpected text after a flow collection".into());
                                }
                                return Ok(());
                            }
                        }
                        _ => {}
                    },
                }
            }
            line = self.peek().ok_or("a flow collection is not closed")?;
            self.at += 1;
        }
    }
}

/// A blank line or a comment.
fn is_blank(line: &str) -> bool {
    let text = line.trim_start_matches([' ', '\t']);
    text.is_empty() || text.starts_with('#')
}

/// A `...` line, which ends the document.
fn is_document_end(line: &str) -> bool {
    line.strip_prefix("...").is_some_and(is_blank)
}

fn is_sequence_entry(text: &str) -> bool {
    text == "-" || text.starts_with("- ") || text.starts_with("-\t")
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// The column a line's text starts at.
fn indent(line: &str) -> Result<usize, String> {
    let spaces = leading_spaces(line);
    if line[spaces..].starts_with('\t') {
        return Err("tabs are not allowed as indentation".into());
    }
    Ok(spaces)
}

/// Splits `text` at its mapping key: the key, and the text after its colon.
/// `None` when the line has no key.
fn split_key(text: &str) -> Result<Option<(String, &str)>, String> {
    if let Some(quote) = text.chars().next().filter(|ch| matches!(*ch, '"' | '\'')) {
        // A quoted key closes on its own line.
        let mut key = String::new();
        let mut chars = text[1..].char_indices();
        while let Some((index, ch)) = chars.next() {
            if ch == quote {
                let rest = text[index + 2..].trim_start_matches([' ', '\t']);
                return Ok(rest
                    .strip_prefix(':')
                    .filter(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
                    .map(|rest| (key, rest)));
            }
            if quote == '"' && ch == '\\' {
                let escape = chars.next().ok_or("missing closing \" quote")?.1;
                key.push(unescape(escape, &mut chars)?);
            } else {
                key.push(ch);
            }
        }
        return Err(format!("missing closing {quote} quote"));
    }
    if text == "?" || text.starts_with("? ") {
        return Err("complex mapping keys are not supported".into());
    }
    let mut previous = None;
    for (index, ch) in text.char_indices() {
        let rest = &text[index + ch.len_utf8()..];
        if ch == '#' && matches!(previous, Some(' ' | '\t')) {
            return Ok(None);
        }
        if ch == ':' && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
            let key = text[..index].trim_end_matches([' ', '\t']);
            if key.is_empty() || is_sequence_entry(key) {
                return Ok(None);
            }
            if key.starts_with([
                '&', '*', '!', '|', '>', '@', '`', '%', '{', '[', '}', ']', ',',
            ]) {
                return Err(format!("unsupported mapping key: {key}"));
            }
            return Ok(Some((key.to_string(), rest)));
        }
        previous = Some(ch);
    }
    Ok(None)
}

/// One line of a plain scalar without its comment, and whether it had one.
fn plain_line(text: &str) -> Result<(String, bool), String> {
    let comment = text
        .match_indices('#')
        .map(|(index, _)| index)
        .find(|&index| text[..index].ends_with([' ', '\t']));
    let line = text[..comment.unwrap_or(text.len())].trim_end_matches([' ', '\t']);
    if line.ends_with(':') || line.contains(": ") || line.contains(":\t") {
        return Err("nested mappings are not allowed in compact mappings".into());
    }
    Ok((line.to_string(), comment.is_some()))
}

/// The character a double-quoted escape stands for. `chars` follows the
/// escape character and supplies the digits of a numeric escape.
fn unescape(escape: char, chars: &mut std::str::CharIndices<'_>) -> Result<char, String> {
    let mut code = |digits: usize| {
        let hex = chars.by_ref().take(digits).map(|(_, ch)| ch);
        let hex = hex.collect::<String>();
        let code = u32::from_str_radix(&hex, 16)
            .ok()
            .filter(|_| hex.len() == digits);
        code.and_then(char::from_u32)
            .ok_or_else(|| format!("invalid escape sequence \\{escape}{hex}"))
    };
    Ok(match escape {
        '0' => '\0',
        'a' => '\u{7}',
        'b' => '\u{8}',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\u{b}',
        'f' => '\u{c}',
        'r' => '\r',
        'e' => '\u{1b}',
        ' ' | '"' | '/' | '\\' => escape,
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        'x' => code(2)?,
        'u' => code(4)?,
        'U' => code(8)?,
        _ => return Err(format!("invalid escape sequence \\{escape}")),
    })
}

/// A plain scalar as YAML's core schema types it.
fn resolve_plain(text: String) -> Value {
    match text.as_str() {
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        "" | "~" | "null" | "Null" | "NULL" => return Value::Other,
        _ => {}
    }
    if is_number(&text) {
        Value::Other
    } else {
        Value::String(text)
    }
}

fn is_number(text: &str) -> bool {
    let digits =
        |text: &str, radix: u32| !text.is_empty() && text.chars().all(|ch| ch.is_digit(radix));
    if let Some(octal) = text.strip_prefix("0o") {
        return digits(octal, 8);
    }
    if let Some(hex) = text.strip_prefix("0x") {
        return digits(hex, 16);
    }
    if matches!(text, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    if matches!(unsigned, ".inf" | ".Inf" | ".INF") {
        return true;
    }
    // [-+]? ( . digits | digits ( . digits? )? ) ( [eE] [-+]? digits )?
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (&unsigned[..index], Some(&unsigned[index + 1..])),
        None => (unsigned, None),
    };
    let exponent_ok = exponent
        .is_none_or(|exponent| digits(exponent.strip_prefix(['-', '+']).unwrap_or(exponent), 10));
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", fraction)) => digits(fraction, 10),
        Some((whole, fraction)) => {
            digits(whole, 10) && (fraction.is_empty() || digits(fraction, 10))
        }
        None => digits(mantissa, 10),
    };
    mantissa_ok && exponent_ok
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value as Json};

    use super::*;

    fn fixture() -> Json {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/skills.json"
        ))
        .expect("parse TS parity fixture")
    }

    /// For every frontmatter case of `skills.json`: the same fields with the
    /// same types, the same body, and the same files rejected.
    #[test]
    fn frontmatter_matches_ts_parser() {
        let fixture = fixture();
        let cases = fixture["frontmatter"].as_array().unwrap();
        assert!(cases.len() > 60);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let content = case["content"]
                .as_str()
                .unwrap()
                .replace("<bom>", "\u{feff}");
            let (yaml, body) = split_frontmatter(&content);
            let document = yaml.as_deref().map_or(Ok(Some(Vec::new())), parse_document);
            if case["error"] == true {
                assert!(document.is_err(), "{name}: upstream rejects this");
                assert!(parse_frontmatter(&content).is_err(), "{name}");
                continue;
            }
            let fields = document.unwrap_or_else(|error| panic!("{name}: {error}"));
            let fields = fields.map(|fields| {
                let fields = fields.into_iter().map(|(key, value)| match value {
                    Value::String(value) => json!({"key": key, "type": "string", "value": value}),
                    Value::Bool(value) => json!({"key": key, "type": "boolean", "value": value}),
                    Value::Other => json!({"key": key, "type": "other"}),
                });
                Json::Array(fields.collect())
            });
            assert_eq!(
                fields.unwrap_or(Json::Null),
                case["fields"],
                "{name}: fields"
            );
            assert_eq!(body, case["body"].as_str().unwrap(), "{name}: body");
            assert_eq!(strip_frontmatter(&content).unwrap(), body, "{name}");
        }
    }

    #[test]
    fn yaml_outside_the_subset_is_malformed() {
        for yaml in [
            "name: &anchor x",
            "name: *alias",
            "name: !tag x",
            "? complex\n: key",
            "{name: x}",
            "%YAML 1.2",
            "name: \"\\ud83d\"",
        ] {
            assert!(parse_document(yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn fields_are_read_by_type() {
        let (frontmatter, body) =
            parse_frontmatter("---\nname: x\nflag: true\ncount: 3\n---\n body \n").unwrap();
        assert_eq!(frontmatter.string("name"), Some("x"));
        assert_eq!(frontmatter.string("flag"), None);
        assert_eq!(frontmatter.string("missing"), None);
        assert!(frontmatter.is_true("flag"));
        assert!(!frontmatter.is_true("name"));
        assert_eq!(frontmatter.fields.len(), 3);
        assert_eq!(body, "body");
    }

    #[test]
    fn trim_follows_javascript() {
        assert_eq!(js_trim("\u{feff}\u{a0} a \u{2028}\n"), "a");
        assert_eq!(js_trim("\u{85}a\u{85}"), "\u{85}a\u{85}");
    }
}
