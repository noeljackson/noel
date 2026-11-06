//! TypeScript declarations for the script-visible API (upstream
//! `declarations.ts`): tools as members of `declare const tools`, globals as
//! `declare function` statements or members of a `declare const` namespace,
//! descriptions as doc comments and JSON Schemas as types.
//!
//! Lengths are counted in UTF-16 code units and property names sorted by
//! them, as upstream's strings are. Object values in `const`, `enum` and
//! `@default` render with their keys sorted (serde_json's map), where
//! upstream keeps insertion order; see the non-parity register.

use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::identifier::to_codemode_identifier;

const INDENT: &str = "  ";
/// Largest rendered input type, in characters, before it becomes `unknown`.
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;
/// Local `$ref` expansions per rendered schema.
const MAX_REF_EXPANSIONS: usize = 32;
/// JSON Schema keywords shown as `@keyword value` property tags.
const TAG_KEYWORDS: [&str; 11] = [
    "minimum",
    "exclusiveMinimum",
    "maximum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "pattern",
    "format",
    "minItems",
    "maxItems",
];
const LOWER_BOUNDS: [&str; 4] = ["minimum", "exclusiveMinimum", "minLength", "minItems"];
const UPPER_BOUNDS: [&str; 4] = ["maximum", "exclusiveMaximum", "maxLength", "maxItems"];
/// Longest property tag; longer ones are left out.
const MAX_TAG_CHARS: usize = 80;

/// A tool or global as the declarations see it (upstream `CodemodeTool`
/// without `execute`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolDeclaration {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    /// Globals only: the parameter list and return type, replacing the
    /// rendering from the schemas.
    pub signature: Option<String>,
}

impl ToolDeclaration {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

/// Upstream `renderDeclarations({tools, globals})`.
pub fn render_declarations(tools: &[ToolDeclaration], globals: &[ToolDeclaration]) -> String {
    let mut sections = Vec::new();
    if !tools.is_empty() {
        let members: Vec<String> = tools
            .iter()
            .map(|tool| {
                format!(
                    "{}{INDENT}{}",
                    doc_comment(tool.description.as_deref(), INDENT),
                    render_tool_signature(tool, None)
                )
            })
            .collect();
        sections.push(format!(
            "declare const tools: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    let mut namespaces: Vec<(String, Vec<String>)> = Vec::new();
    for global in globals {
        let Some(dot) = global.name.find('.') else {
            sections.push(render_global(
                &format!("declare function {}", global.name),
                global,
                "",
            ));
            continue;
        };
        let namespace = &global.name[..dot];
        let member = render_global(&global.name[dot + 1..], global, INDENT);
        match namespaces.iter_mut().find(|(name, _)| name == namespace) {
            Some((_, members)) => members.push(member),
            None => namespaces.push((namespace.to_string(), vec![member])),
        }
    }
    for (namespace, members) in namespaces {
        sections.push(format!(
            "declare const {namespace}: {{\n{}\n}};",
            members.join("\n")
        ));
    }
    sections.join("\n\n")
}

/// One tool as a member of `tools`: `name(args: T): Promise<R>;`. Input
/// types longer than `input_max_chars` (default
/// [`DEFAULT_INPUT_SCHEMA_MAX_CHARS`]) render as `unknown`.
pub fn render_tool_signature(tool: &ToolDeclaration, input_max_chars: Option<usize>) -> String {
    let input = match &tool.input_schema {
        None => "unknown".to_string(),
        Some(schema) => schema_to_type(
            schema,
            Some(input_max_chars.unwrap_or(DEFAULT_INPUT_SCHEMA_MAX_CHARS)),
        ),
    };
    format!(
        "{}(args: {input}): Promise<{}>;",
        to_codemode_identifier(&tool.name),
        render_tool_output_type(tool.output_schema.as_ref())
    )
}

/// The description followed by the tool's declaration, for tool listings.
pub fn render_tool_sample(tool: &ToolDeclaration, input_max_chars: Option<usize>) -> String {
    let declaration = format!(
        "declare const tools: {{ {} }};",
        render_tool_signature(tool, input_max_chars)
    );
    let description = tool.description.as_deref().map(js_trim).unwrap_or("");
    format!("{description}\n\ncodemode tool declaration:\n```ts\n{declaration}\n```")
}

/// The type a call resolves to; `unknown` without a schema.
pub fn render_tool_output_type(schema: Option<&Value>) -> String {
    schema.map_or_else(
        || "unknown".to_string(),
        |schema| schema_to_type(schema, None),
    )
}

fn render_global(head: &str, global: &ToolDeclaration, indent: &str) -> String {
    let doc = doc_comment(global.description.as_deref(), indent);
    if let Some(signature) = &global.signature {
        return format!("{doc}{indent}{head}{signature};");
    }
    let render = |schema: &Option<Value>| {
        schema.as_ref().map_or_else(
            || "unknown".to_string(),
            |schema| schema_to_type(schema, None),
        )
    };
    format!(
        "{doc}{indent}{head}(args: {}): Promise<{}>;",
        render(&global.input_schema),
        render(&global.output_schema)
    )
}

fn doc_comment(description: Option<&str>, indent: &str) -> String {
    let text = description.map(js_trim).unwrap_or("");
    if text.is_empty() {
        return String::new();
    }
    let escaped = text.replace("*/", "*\\/");
    let lines: Vec<&str> = escaped
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.len() == 1 {
        return format!("{indent}/** {} */\n", lines[0]);
    }
    let body: Vec<String> = lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                format!("{indent} *")
            } else {
                format!("{indent} * {line}")
            }
        })
        .collect();
    format!("{indent}/**\n{}\n{indent} */\n", body.join("\n"))
}

/// JavaScript's `String.prototype.trim`: Unicode white space, line
/// terminators and the byte order mark.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        json(&Value::String(name.to_string()))
    }
}

fn json(value: &Value) -> String {
    match value {
        Value::Number(number) => js_number(number),
        Value::Array(items) => {
            format!("[{}]", items.iter().map(json).collect::<Vec<_>>().join(","))
        }
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    json(&Value::String(key.clone())),
                    json(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

/// A number as JavaScript's `String(n)` writes it.
fn js_number(number: &serde_json::Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    let value = number.as_f64().unwrap_or(f64::NAN);
    js_f64(value)
}

fn js_f64(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return "0".into();
    }
    let magnitude = value.abs();
    if (1e-6..1e21).contains(&magnitude) {
        return format!("{value}");
    }
    // Exponent form: `1e+21`, `1.5e-7`.
    let formatted = format!("{value:e}");
    match formatted.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => formatted,
    }
}

fn union(types: Vec<String>) -> String {
    let mut seen = HashSet::new();
    let unique: Vec<String> = types
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect();
    if unique.iter().any(|t| t == "unknown") {
        return "unknown".into();
    }
    if unique.is_empty() {
        "never".into()
    } else {
        unique.join(" | ")
    }
}

struct Context<'a> {
    root: &'a Value,
    resolving: Vec<String>,
    expansions: usize,
    tags: bool,
}

/// Upstream `schemaToType`. A result longer than `max_chars` renders again
/// without constraint tags, and as `unknown` if still too long.
pub fn schema_to_type(schema: &Value, max_chars: Option<usize>) -> String {
    let render = |tags: bool| {
        to_type(
            schema,
            &mut Context {
                root: schema,
                resolving: Vec::new(),
                expansions: 0,
                tags,
            },
        )
    };
    let rendered = render(true);
    let Some(max) = max_chars else {
        return rendered;
    };
    if utf16_len(&rendered) <= max {
        return rendered;
    }
    let untagged = render(false);
    if utf16_len(&untagged) <= max {
        untagged
    } else {
        "unknown".into()
    }
}

fn decode_pointer_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = segment.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(out).ok()?;
    Some(decoded.replace("~1", "/").replace("~0", "~"))
}

/// The subschema a local `$ref` (`#` or `#/...`) names in `root`; nothing
/// for a remote ref, a missing target or a malformed escape (upstream
/// `resolveSchemaRef`, 6d3e342b).
pub fn resolve_schema_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    resolve_ref(reference, root)
}

fn resolve_ref<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    if reference != "#" && !reference.starts_with("#/") {
        return None;
    }
    let mut current = root;
    for segment in reference
        .get(2..)
        .unwrap_or("")
        .split('/')
        .filter(|s| !s.is_empty())
    {
        let key = decode_pointer_segment(segment)?;
        current = current.as_object()?.get(&key)?;
    }
    matches!(current, Value::Bool(_) | Value::Object(_)).then_some(current)
}

fn to_type(schema: &Value, context: &mut Context<'_>) -> String {
    let object = match schema {
        Value::Bool(true) => return "unknown".into(),
        Value::Bool(false) => return "never".into(),
        Value::Object(object) => object,
        _ => return "unknown".into(),
    };
    if let Some(Value::String(reference)) = object.get("$ref") {
        if context.resolving.contains(reference) || context.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".into();
        }
        let Some(target) = resolve_ref(reference, context.root) else {
            return "unknown".into();
        };
        context.expansions += 1;
        context.resolving.push(reference.clone());
        let rendered = to_type(target, context);
        context.resolving.pop();
        return rendered;
    }
    if let Some(constant) = object.get("const") {
        return json(constant);
    }
    if let Some(Value::Array(values)) = object.get("enum") {
        return union(values.iter().map(json).collect());
    }
    let variants = match (object.get("anyOf"), object.get("oneOf")) {
        (Some(Value::Array(variants)), _) | (_, Some(Value::Array(variants))) => Some(variants),
        _ => None,
    };
    if let Some(variants) = variants {
        return union(
            variants
                .iter()
                .map(|variant| to_type(variant, context))
                .collect(),
        );
    }
    if let Some(Value::Array(parts)) = object.get("allOf") {
        let parts: Vec<String> = parts
            .iter()
            .map(|part| to_type(part, context))
            .filter(|part| part != "unknown")
            .collect();
        if parts.is_empty() {
            return "unknown".into();
        }
        return parts
            .into_iter()
            .map(|part| {
                if part.contains(" | ") {
                    format!("({part})")
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(" & ");
    }
    match object.get("type") {
        Some(Value::Array(types)) => union(
            types
                .iter()
                .map(|entry| {
                    // Upstream recurses with the entry as `type`; an array
                    // entry would recurse without end there.
                    if entry.is_array() {
                        return "unknown".to_string();
                    }
                    let mut variant = object.clone();
                    variant.insert("type".into(), entry.clone());
                    to_type(&Value::Object(variant), context)
                })
                .collect(),
        ),
        Some(Value::String(kind)) => match kind.as_str() {
            "string" => "string".into(),
            "number" | "integer" => "number".into(),
            "boolean" => "boolean".into(),
            "null" => "null".into(),
            "array" => array_type(object, context),
            "object" => object_type(object, context),
            _ => "unknown".into(),
        },
        Some(_) => "unknown".into(),
        None => {
            if ["properties", "additionalProperties", "required"]
                .iter()
                .any(|key| object.contains_key(*key))
            {
                object_type(object, context)
            } else if object.contains_key("items") || object.contains_key("prefixItems") {
                array_type(object, context)
            } else {
                "unknown".into()
            }
        }
    }
}

fn array_type(schema: &Map<String, Value>, context: &mut Context<'_>) -> String {
    match schema.get("items") {
        Some(items) if !items.is_array() => return format!("Array<{}>", to_type(items, context)),
        _ => {}
    }
    let tuple = match (schema.get("prefixItems"), schema.get("items")) {
        (Some(Value::Array(items)), _) | (_, Some(Value::Array(items))) => items.as_slice(),
        _ => &[],
    };
    if tuple.is_empty() {
        return "unknown[]".into();
    }
    format!(
        "[{}]",
        tuple
            .iter()
            .map(|item| to_type(item, context))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn description_of(property: &Value) -> &str {
    match property.get("description") {
        Some(Value::String(description)) => js_trim(description),
        _ => "",
    }
}

fn number_of(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|_| value.is_some_and(Value::is_number))
}

/// Constraints the TypeScript type cannot express, as
/// `@integer @minimum 1 @default 20`; empty when there are none.
fn constraint_tags(schema: &Value) -> String {
    let Some(property) = schema.as_object() else {
        return String::new();
    };
    // Optional fields such as Pydantic's `int | None` keep their constraints
    // in the non-null branch; both are read.
    let variants = match property.get("anyOf") {
        Some(Value::Array(variants)) => Some(variants),
        _ => match property.get("oneOf") {
            Some(Value::Array(variants)) => Some(variants),
            _ => None,
        },
    };
    let non_null: Vec<&Value> = variants
        .map(|variants| {
            variants
                .iter()
                .filter(|variant| variant.get("type") != Some(&Value::String("null".into())))
                .collect()
        })
        .unwrap_or_default();
    let mut branches: Vec<&Map<String, Value>> = vec![property];
    if let [branch] = non_null.as_slice() {
        if let Some(branch) = branch.as_object() {
            branches.push(branch);
        }
    }
    // Draft-04's boolean `exclusiveMinimum`/`exclusiveMaximum`.
    let schemas: Vec<Map<String, Value>> = branches
        .into_iter()
        .map(|schema| {
            let mut schema = schema.clone();
            for (bound, exclusive) in [
                ("minimum", "exclusiveMinimum"),
                ("maximum", "exclusiveMaximum"),
            ] {
                if schema.get(exclusive) == Some(&Value::Bool(true)) {
                    match schema.remove(bound) {
                        Some(value) => schema.insert(exclusive.into(), value),
                        None => schema.remove(exclusive),
                    };
                }
            }
            schema
        })
        .collect();

    let mut tags = Vec::new();
    let is_integer = |kind: Option<&Value>| match kind {
        Some(Value::String(kind)) => kind == "integer",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "integer"),
        _ => false,
    };
    if schemas.iter().any(|schema| is_integer(schema.get("type"))) {
        tags.push("@integer".to_string());
    }
    for keyword in TAG_KEYWORDS {
        let lower = LOWER_BOUNDS.contains(&keyword);
        if lower || UPPER_BOUNDS.contains(&keyword) {
            let numbers: Vec<f64> = schemas
                .iter()
                .filter_map(|schema| number_of(schema.get(keyword)))
                .collect();
            let bound = if lower {
                numbers.into_iter().reduce(f64::max)
            } else {
                numbers.into_iter().reduce(f64::min)
            };
            if let Some(bound) = bound {
                tags.push(format!("@{keyword} {}", js_f64(bound)));
            }
            continue;
        }
        let mut seen: Vec<&Value> = Vec::new();
        for value in schemas.iter().filter_map(|schema| schema.get(keyword)) {
            let duplicate = seen.iter().any(|other| match (other, value) {
                (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
                (a, b) => *a == b,
            });
            if duplicate {
                continue;
            }
            seen.push(value);
            match value {
                Value::Number(number) => tags.push(format!("@{keyword} {}", js_number(number))),
                Value::String(text) => tags.push(format!("@{keyword} {text}")),
                _ => {}
            }
        }
    }
    if schemas
        .iter()
        .any(|schema| schema.get("uniqueItems") == Some(&Value::Bool(true)))
    {
        tags.push("@uniqueItems".into());
    }
    // Annotations come from the property itself.
    if let Some(default) = property.get("default").filter(|value| !value.is_null()) {
        tags.push(format!("@default {}", json(default)));
    }
    if property.get("deprecated") == Some(&Value::Bool(true)) {
        tags.push("@deprecated".into());
    }
    tags.into_iter()
        .filter(|tag| utf16_len(tag) <= MAX_TAG_CHARS)
        .map(|tag| {
            let mut escaped = String::with_capacity(tag.len());
            for c in tag.chars() {
                if is_line_terminator(c) {
                    escaped.push_str(&format!("\\u{:04x}", c as u32));
                } else {
                    escaped.push(c);
                }
            }
            escaped
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn object_type(schema: &Map<String, Value>, context: &mut Context<'_>) -> String {
    let empty = Map::new();
    let properties = match schema.get("properties") {
        Some(Value::Object(properties)) => properties,
        _ => &empty,
    };
    let required: Vec<&Value> = match schema.get("required") {
        Some(Value::Array(required)) => required.iter().collect(),
        _ => Vec::new(),
    };
    let mut names: Vec<&String> = properties.keys().collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut members: Vec<String> = names
        .iter()
        .map(|name| {
            let optional = if required.iter().any(|r| r.as_str() == Some(name.as_str())) {
                ""
            } else {
                "?"
            };
            format!(
                "{}{optional}: {};",
                property_key(name),
                to_type(&properties[name.as_str()], context)
            )
        })
        .collect();
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(additional) => {
            let rendered = if additional == &Value::Bool(true) {
                "unknown".to_string()
            } else {
                to_type(additional, context)
            };
            members.push(format!("[key: string]: {rendered};"));
        }
        None if names.is_empty() => members.push("[key: string]: unknown;".into()),
        None => {}
    }
    if members.is_empty() {
        return "{}".into();
    }
    let comments: Vec<Vec<String>> = names
        .iter()
        .map(|name| {
            let property = &properties[name.as_str()];
            let mut lines: Vec<String> = description_of(property)
                .split(is_line_terminator)
                .map(js_trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            if context.tags {
                let tags = constraint_tags(property);
                if !tags.is_empty() {
                    lines.push(tags);
                }
            }
            lines
        })
        .collect();
    if comments.iter().all(Vec::is_empty) {
        return format!("{{ {} }}", members.join(" "));
    }
    let mut lines = vec!["{".to_string()];
    for (index, comment) in comments.iter().enumerate() {
        for line in comment {
            lines.push(format!("{INDENT}// {line}"));
        }
        lines.push(format!(
            "{INDENT}{}",
            members[index].replace('\n', &format!("\n{INDENT}"))
        ));
    }
    for member in &members[names.len()..] {
        lines.push(format!("{INDENT}{member}"));
    }
    lines.push("}".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(value: &Value) -> ToolDeclaration {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        ToolDeclaration {
            name: text("name").unwrap_or_default(),
            description: text("description"),
            input_schema: value.get("inputSchema").cloned(),
            output_schema: value.get("outputSchema").cloned(),
            signature: text("signature"),
        }
    }

    fn declarations(value: Option<&Value>) -> Vec<ToolDeclaration> {
        value
            .and_then(Value::as_array)
            .map(|items| items.iter().map(declaration).collect())
            .unwrap_or_default()
    }

    fn max_chars(options: Option<&Value>, key: &str) -> Option<usize> {
        options
            .and_then(|options| options.get(key))
            .and_then(Value::as_u64)
            .map(|n| n as usize)
    }

    /// Every call upstream's `declarations.test.ts` makes, and noel's own
    /// cases, rendered by upstream (`codemode-declarations-fixture.ts`).
    #[test]
    fn matches_upstream_goldens() {
        let golden: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/codemode-declarations.json"
        ))
        .unwrap();
        assert_eq!(
            golden["defaultInputSchemaMaxChars"],
            DEFAULT_INPUT_SCHEMA_MAX_CHARS
        );
        let calls = golden["calls"].as_array().unwrap();
        assert!(calls.len() > 50);
        let mut failures = Vec::new();
        for call in calls {
            let args = call["args"].as_array().unwrap();
            let rendered = match call["fn"].as_str().unwrap() {
                // A schema, or nothing (upstream's `undefined`, absent here).
                "resolveSchemaRef" => {
                    let found = resolve_schema_ref(args[0].as_str().unwrap(), &args[1]);
                    if found != call.get("result") {
                        failures.push(format!(
                            "resolveSchemaRef {}\n  rust:     {found:?}\n  upstream: {:?}",
                            call["args"],
                            call.get("result")
                        ));
                    }
                    continue;
                }
                "schemaToType" => schema_to_type(&args[0], max_chars(args.get(1), "maxChars")),
                "renderToolSignature" => render_tool_signature(
                    &declaration(&args[0]),
                    max_chars(args.get(1), "inputMaxChars"),
                ),
                "renderToolSample" => render_tool_sample(
                    &declaration(&args[0]),
                    max_chars(args.get(1), "inputMaxChars"),
                ),
                "renderDeclarations" => render_declarations(
                    &declarations(args[0].get("tools")),
                    &declarations(args[0].get("globals")),
                ),
                other => panic!("unknown call {other}"),
            };
            if rendered != call["result"].as_str().unwrap() {
                failures.push(format!(
                    "{} {}\n  rust:     {rendered:?}\n  upstream: {:?}",
                    call["fn"], call["args"], call["result"]
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn numbers_render_as_javascript_writes_them() {
        assert_eq!(js_f64(1000.0), "1000");
        assert_eq!(js_f64(0.5), "0.5");
        assert_eq!(js_f64(1e21), "1e+21");
        assert_eq!(js_f64(1.5e-7), "1.5e-7");
    }
}
