//! Prompt templates, as upstream loads and expands them
//! (`core/prompt-templates.ts`): a Markdown file `name.md` is the command
//! `/name`, and its body is the prompt, with the command's arguments
//! substituted. `skills.json` holds upstream's results for the cases the
//! tests in this crate compare.

use std::fs;
use std::path::{Path, PathBuf};

use crate::frontmatter::{is_js_whitespace, js_trim, parse_frontmatter};
use crate::resources::{
    is_markdown, sorted_entries, ResourceDiagnostic, ResourcePath, SourceInfo, SourceScope,
};
use crate::{project_config_dir, ConfigPaths};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    /// The file's name without `.md`.
    pub name: String,
    /// The frontmatter's `description`, or the start of the body's first
    /// line.
    pub description: String,
    /// The frontmatter's `argument-hint`.
    pub argument_hint: Option<String>,
    /// The file without its frontmatter.
    pub content: String,
    pub source_info: SourceInfo,
    pub file_path: PathBuf,
}

/// Loads the templates of `paths`, in order: a Markdown file, or the
/// Markdown files directly in a directory. The first template of a name
/// wins and each later one is reported, after the other diagnostics. Each
/// `explicit` path that does not exist is reported last. A template loaded
/// from a path without a source takes the source `discovered` has for its
/// file, if any.
pub(crate) fn load_prompt_templates(
    paths: &[ResourcePath],
    discovered: &[ResourcePath],
    explicit: &[ResourcePath],
    config: &ConfigPaths,
) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut templates: Vec<PromptTemplate> = Vec::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();
    for resource in paths {
        let path = &resource.path;
        let files = if path.is_dir() {
            let entries = sorted_entries(path).into_iter();
            entries
                .filter(|file| file.is_file() && is_markdown(file))
                .collect()
        } else if path.is_file() && is_markdown(path) {
            vec![path.clone()]
        } else {
            continue;
        };
        for file in files {
            let source = resource.source.as_ref().or_else(|| {
                let found = discovered.iter().find(|resource| resource.path == file);
                found.and_then(|resource| resource.source.as_ref())
            });
            let source = match source {
                Some(source) => SourceInfo {
                    path: file.clone(),
                    ..source.clone()
                },
                None => explicit_source(&file, config),
            };
            let Some(template) = load_template_file(&file, source, &mut diagnostics) else {
                continue;
            };
            match templates.iter().find(|loaded| loaded.name == template.name) {
                Some(winner) => collisions.push(ResourceDiagnostic::collision(
                    &format!("/{}", template.name),
                    &winner.file_path,
                    &template.file_path,
                )),
                None => templates.push(template),
            }
        }
    }
    diagnostics.append(&mut collisions);
    for resource in explicit {
        let reported = diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == resource.path);
        if !resource.path.exists() && !reported {
            let message = "Prompt template path does not exist";
            diagnostics.push(ResourceDiagnostic::new(message, &resource.path));
        }
    }
    (templates, diagnostics)
}

/// The source of a template `file` given on the command line.
fn explicit_source(file: &Path, config: &ConfigPaths) -> SourceInfo {
    let user = config.agent_dir.join("prompts");
    let project = project_config_dir(&config.cwd).join("prompts");
    if file.starts_with(&user) {
        SourceInfo::local(file, SourceScope::User, &user)
    } else if file.starts_with(&project) {
        SourceInfo::local(file, SourceScope::Project, &project)
    } else {
        SourceInfo::local(file, SourceScope::Temporary, file.parent().unwrap_or(file))
    }
}

fn load_template_file(
    path: &Path,
    source_info: SourceInfo,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Option<PromptTemplate> {
    let content = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => {
            diagnostics.push(ResourceDiagnostic::new(error.to_string(), path));
            return None;
        }
    };
    let (frontmatter, body) = match parse_frontmatter(&content) {
        Ok(parsed) => parsed,
        Err(message) => {
            diagnostics.push(ResourceDiagnostic::new(message, path));
            return None;
        }
    };
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let name = file_name.strip_suffix(".md").unwrap_or(&file_name);
    let described = frontmatter
        .string("description")
        .filter(|text| !text.is_empty());
    let description = match described {
        Some(description) => description.to_string(),
        None => first_line_summary(&body),
    };
    let hint = frontmatter.string("argument-hint");
    Some(PromptTemplate {
        name: name.to_string(),
        description,
        argument_hint: hint.filter(|hint| !hint.is_empty()).map(str::to_string),
        content: body,
        source_info,
        file_path: path.to_path_buf(),
    })
}

/// The first 60 characters of the body's first line that is not blank,
/// counted as JavaScript counts them, with `...` when the line is longer.
fn first_line_summary(body: &str) -> String {
    let line = body.split('\n').find(|line| !js_trim(line).is_empty());
    let units = line.unwrap_or_default().encode_utf16().collect::<Vec<_>>();
    match units.get(..60) {
        Some(start) if units.len() > 60 => format!("{}...", String::from_utf16_lossy(start)),
        _ => String::from_utf16_lossy(&units),
    }
}

/// Splits a command's arguments at white space; upstream's
/// `parseCommandArgs` (upstream 96667f34):
///
/// - a single or double quote opens a quoted argument only at the start of
///   an argument, so an apostrophe inside a word (`doesn't`) is text;
/// - inside one, a backslash before its quote character is that quote;
///   any other backslash is kept;
/// - a quote that never closes is text, and the rest is read again;
/// - a quoted empty string (`""` or `''`) is an argument.
pub fn parse_command_args(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    // Where the open quote started, so an unclosed one is read again as text.
    let mut quote_start = 0;
    // Whether a token has started (upstream `hasToken`).
    let mut has_token = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        match quote {
            Some(open) if ch == '\\' && chars.get(index + 1) == Some(&open) => {
                current.push(open);
                index += 1;
            }
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if (ch == '"' || ch == '\'') && !has_token => {
                quote = Some(ch);
                quote_start = index;
                has_token = true;
            }
            None if is_js_whitespace(ch) => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(ch);
                has_token = true;
            }
        }
        if quote.is_some() && index == chars.len() - 1 {
            // Unclosed: the quote is text, and what follows it is parsed
            // again (`current` was empty when the quote opened).
            quote = None;
            current = chars[quote_start].to_string();
            index = quote_start;
        }
        index += 1;
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Replaces the argument placeholders of a template's `content`: upstream's
/// `substituteArgs`.
///
/// - `$1`, `$2`, ...: one argument, or nothing.
/// - `$@`, `$ARGUMENTS`: all arguments, joined by spaces.
/// - `${1:-default}`, `${@:-default}`, `${ARGUMENTS:-default}`: the
///   argument or all arguments, or `default` when that is empty.
/// - `${@:N}`: the arguments from the Nth on; `${@:N:L}`: L of them.
///
/// An argument's own text is never substituted again.
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut output = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(dollar) = rest.find('$') {
        output.push_str(&rest[..dollar]);
        rest = &rest[dollar..];
        match placeholder(rest, args) {
            Some((length, value)) => {
                output.push_str(&value);
                rest = &rest[length..];
            }
            None => {
                output.push('$');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

/// The placeholder `text` starts with, as its length and its value.
fn placeholder(text: &str, args: &[String]) -> Option<(usize, String)> {
    let all = || args.join(" ");
    let argument = |number: &str| {
        position(number)
            .checked_sub(1)
            .and_then(|index| args.get(index))
    };

    if let Some(braced) = text.strip_prefix("${") {
        // ${N:-default}, ${@:-default}, ${ARGUMENTS:-default}
        let all_name = ["@", "ARGUMENTS"]
            .into_iter()
            .find(|name| braced.starts_with(name));
        let target = all_name
            .map(|name| braced.split_at(name.len()))
            .or_else(|| digits(braced));
        let with_default = target.and_then(|(target, rest)| {
            let (default, _) = rest.strip_prefix(":-")?.split_once('}')?;
            let value = match target {
                "@" | "ARGUMENTS" => all(),
                number => argument(number).cloned().unwrap_or_default(),
            };
            let value = if value.is_empty() {
                default.to_string()
            } else {
                value
            };
            Some((
                text.len() - rest.len() + ":-".len() + default.len() + 1,
                value,
            ))
        });
        if with_default.is_some() {
            return with_default;
        }
        // ${@:N} and ${@:N:L}
        let (start, rest) = digits(braced.strip_prefix("@:")?)?;
        let (count, rest) = match rest.strip_prefix(':').and_then(digits) {
            Some((count, rest)) => (position(count), rest),
            None => (usize::MAX, rest),
        };
        let rest = rest.strip_prefix('}')?;
        let slice = args
            .iter()
            .skip(position(start).saturating_sub(1))
            .take(count);
        let value = slice.map(String::as_str).collect::<Vec<_>>().join(" ");
        return Some((text.len() - rest.len(), value));
    }

    let name = &text[1..];
    if let Some(all_name) = ["ARGUMENTS", "@"]
        .into_iter()
        .find(|all| name.starts_with(all))
    {
        return Some((1 + all_name.len(), all()));
    }
    let (number, _) = digits(name)?;
    Some((
        1 + number.len(),
        argument(number).cloned().unwrap_or_default(),
    ))
}

/// The ASCII digits `text` starts with, and the rest; `None` without one.
fn digits(text: &str) -> Option<(&str, &str)> {
    let end = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// A 1-based argument position; a number too large to hold is past every
/// argument.
fn position(digits: &str) -> usize {
    digits.parse().unwrap_or(usize::MAX)
}

/// Expands `/name arguments` when a template is called `name`: upstream's
/// `expandPromptTemplate`. Any other text is returned as it is.
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    let Some(command) = text.strip_prefix('/') else {
        return text.to_string();
    };
    let name_end = command.find(is_js_whitespace).unwrap_or(command.len());
    let (name, arguments) = command.split_at(name_end);
    match templates.iter().find(|template| template.name == name) {
        Some(template) if !name.is_empty() => {
            let arguments = arguments.trim_start_matches(is_js_whitespace);
            substitute_args(&template.content, &parse_command_args(arguments))
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/ts-parity/skills.json"
        ))
        .expect("parse TS parity fixture")
    }

    /// Upstream's cases for apostrophes, unclosed quotes and escaped
    /// quotes (96667f34, `prompt-templates.test.ts`).
    #[test]
    fn apostrophes_unclosed_and_escaped_quotes_split_as_upstream() {
        let cases: [(&str, &[&str]); 9] = [
            (
                "why doesn't the user's cache work",
                &["why", "doesn't", "the", "user's", "cache", "work"],
            ),
            ("'tis the season", &["'tis", "the", "season"]),
            (r#""quoted \"text\"""#, &[r#"quoted "text""#]),
            (r"'it\'s here'", &["it's here"]),
            (r#""C:\dir\file""#, &[r"C:\dir\file"]),
            (r"'\\server\share'", &[r"\\server\share"]),
            (r#""a b" 'c d' "" ''"#, &["a b", "c d", "", ""]),
            (r#"one "two"#, &["one", "\"two"]),
            (r#""C:\dir\""#, &[r#""C:\dir\""#]),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_command_args(input), expected, "{input}");
        }
    }

    /// Every argument string of `skills.json` splits as upstream splits it,
    /// and every template content substitutes to upstream's text.
    #[test]
    fn template_arguments_match_ts() {
        let fixture = fixture();
        let parsing = fixture["templateArguments"]["parsing"].as_array().unwrap();
        assert!(parsing.len() > 20);
        for case in parsing {
            let input = case["input"].as_str().unwrap();
            let args = parse_command_args(input);
            assert_eq!(serde_json::json!(args), case["args"], "{input:?}");
        }
        let substitution = fixture["templateArguments"]["substitution"]
            .as_array()
            .unwrap();
        assert!(substitution.len() > 40);
        for case in substitution {
            let content = case["content"].as_str().unwrap();
            for output in case["outputs"].as_array().unwrap() {
                let input = output["input"].as_str().unwrap();
                assert_eq!(
                    substitute_args(content, &parse_command_args(input)),
                    output["output"].as_str().unwrap(),
                    "{content:?} with {input:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_first_line_is_cut_by_javascript_length() {
        assert_eq!(first_line_summary("\n \nshort\nnext"), "short");
        assert_eq!(first_line_summary(""), "");
        let line = "x".repeat(61);
        assert_eq!(first_line_summary(&line), format!("{}...", "x".repeat(60)));
        // An astral character counts as two.
        let line = format!("{}\u{10348}tail", "x".repeat(58));
        let summary = first_line_summary(&line);
        assert_eq!(summary, format!("{}\u{10348}...", "x".repeat(58)));
    }
}
