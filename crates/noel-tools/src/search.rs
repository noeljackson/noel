//! `grep`, `find`, and `ls`. Upstream runs ripgrep and fd; these walk the
//! tree with ripgrep's own `ignore` and `globset` crates and reproduce the
//! flags upstream passes, so the same files are searched and listed.

use std::fs;
use std::path::Path;

use globset::GlobBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::paths::resolve_to_cwd;
use crate::truncate::{
    format_size, truncate_head, truncate_line, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH,
};
use crate::{ToolError, ToolResult};

const GREP_DEFAULT_LIMIT: u64 = 100;
const FIND_DEFAULT_LIMIT: u64 = 1000;
const LS_DEFAULT_LIMIT: u64 = 500;
/// ripgrep reads a file in blocks of this size and stops at a block that
/// holds a NUL byte.
const BINARY_BLOCK_BYTES: usize = 64 * 1024;

pub(crate) struct GrepArgs<'a> {
    pub pattern: &'a str,
    pub path: Option<&'a str>,
    pub glob: Option<&'a str>,
    pub ignore_case: bool,
    pub literal: bool,
    pub context: u64,
    pub limit: Option<u64>,
}

fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}

/// Result lines under the byte limit, followed by the notices that tell the
/// model what was cut and how to get more.
fn limited_output(lines: &[String], mut notices: Vec<String>, after_bytes: &[&str]) -> ToolResult {
    let truncation = truncate_head(&lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
    }
    notices.extend(after_bytes.iter().map(ToString::to_string));
    let mut output = truncation.content;
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    ToolResult::text(output)
}

/// A path below `root` with `/` separators.
fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Like `rg --hidden [--ignore-case] [--fixed-strings] [--glob GLOB]`:
/// hidden files are searched, ignore files apply, and inside a git
/// repository so does `.gitignore`. Files are searched in path order.
pub(crate) fn grep_tool(cwd: &Path, args: GrepArgs<'_>) -> Result<ToolResult, ToolError> {
    let search_path = resolve_to_cwd(cwd, args.path.unwrap_or("."));
    let Ok(metadata) = fs::metadata(&search_path) else {
        return Err(failed(format!("Path not found: {}", search_path.display())));
    };
    let is_directory = metadata.is_dir();
    let limit = args.limit.unwrap_or(GREP_DEFAULT_LIMIT).max(1) as usize;
    let context = args.context as usize;
    if args.pattern.contains('\n') {
        return Err(failed("the literal \"\\n\" is not allowed in a regex"));
    }
    let pattern = if args.literal {
        regex::escape(args.pattern)
    } else {
        args.pattern.to_string()
    };
    let matcher = regex::bytes::RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|error| failed(error.to_string()))?;

    let mut walk = WalkBuilder::new(&search_path);
    walk.hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .sort_by_file_name(|a, b| a.cmp(b));
    if let Some(glob) = args.glob {
        // ripgrep resolves a glob with a directory against its working
        // directory, which upstream leaves at pi's.
        let mut overrides = OverrideBuilder::new(cwd);
        overrides
            .add(glob)
            .map_err(|error| failed(error.to_string()))?;
        walk.overrides(
            overrides
                .build()
                .map_err(|error| failed(error.to_string()))?,
        );
    }

    let mut output = Vec::new();
    let mut match_count = 0;
    let mut lines_truncated = false;
    let mut push_line =
        |output: &mut Vec<String>, path: &str, number: usize, separator: char, text: &str| {
            let (text, was_cut) = truncate_line(&text.replace('\r', ""));
            lines_truncated |= was_cut;
            output.push(format!("{path}{separator}{number}{separator} {text}"));
        };
    // Unreadable files and directories are skipped.
    'files: for entry in walk.build().flatten() {
        // The search path itself is searched whatever it is; below it only
        // regular files are, and symlinks are not followed.
        let explicit = entry.depth() == 0;
        let is_file = if explicit {
            entry.path().is_file()
        } else {
            entry.file_type().is_some_and(|kind| kind.is_file())
        };
        if !is_file {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let mut searched = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        if !explicit {
            if let Some(nul) = searched.iter().position(|byte| *byte == 0) {
                // A binary file: only the blocks before the NUL are searched.
                let block_start = nul - nul % BINARY_BLOCK_BYTES;
                let end = searched[..block_start]
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |newline| newline + 1);
                searched = &searched[..end];
            }
        }
        let display = if is_directory && !explicit {
            relative_display(&search_path, entry.path())
        } else {
            entry.file_name().to_string_lossy().into_owned()
        };
        // Context lines come from the file as upstream reads it: all of it,
        // with every line ending normalized.
        let mut file_lines: Option<Vec<String>> = None;
        let mut lines = searched.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        if searched.last().is_none_or(|byte| *byte == b'\n') {
            lines.pop();
        }
        for (index, line) in lines.iter().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            match_count += 1;
            let number = index + 1;
            match std::str::from_utf8(line) {
                Ok(text) if context == 0 => push_line(&mut output, &display, number, ':', text),
                _ => {
                    let file_lines = file_lines.get_or_insert_with(|| {
                        String::from_utf8_lossy(&bytes)
                            .replace("\r\n", "\n")
                            .replace('\r', "\n")
                            .split('\n')
                            .map(ToString::to_string)
                            .collect()
                    });
                    let first = number.saturating_sub(context).max(1);
                    let last = number.saturating_add(context).min(file_lines.len());
                    for current in first..=last {
                        let separator = if current == number { ':' } else { '-' };
                        push_line(
                            &mut output,
                            &display,
                            current,
                            separator,
                            &file_lines[current - 1],
                        );
                    }
                }
            }
            if match_count >= limit {
                break 'files;
            }
        }
    }
    if match_count == 0 {
        return Ok(ToolResult::text("No matches found"));
    }
    let mut notices = Vec::new();
    if match_count >= limit {
        notices.push(format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    let lines_notice = format!(
        "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
    );
    let after_bytes = if lines_truncated {
        vec![lines_notice.as_str()]
    } else {
        Vec::new()
    };
    Ok(limited_output(&output, notices, &after_bytes))
}

/// Like `fd --glob --hidden`: files and directories whose name matches the
/// glob, or whose path does when the glob has a `/`. Hidden entries are
/// listed and ignore files apply, `.gitignore` also outside a repository.
/// Results are in path order.
pub(crate) fn find_tool(
    cwd: &Path,
    pattern: &str,
    path: Option<&str>,
    limit: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let search_path = resolve_to_cwd(cwd, path.unwrap_or("."));
    if !search_path.exists() {
        return Err(failed(format!("Path not found: {}", search_path.display())));
    }
    if !search_path.is_dir() {
        return Err(failed(format!(
            "Not a directory: {}",
            search_path.display()
        )));
    }
    let limit = limit.unwrap_or(FIND_DEFAULT_LIMIT).max(1) as usize;
    // A glob with a directory is matched against the whole path, so it needs
    // a leading `**/` to match anywhere below the search path.
    let full_path = pattern.contains('/');
    let glob = if full_path && !pattern.starts_with('/') && !pattern.starts_with("**/") {
        format!("**/{pattern}")
    } else {
        pattern.to_string()
    };
    // fd's smart case: a pattern without uppercase letters ignores case.
    let matcher = GlobBuilder::new(&glob)
        .literal_separator(true)
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()
        .map_err(|error| failed(error.to_string()))?
        .compile_matcher();
    // Inside a repository `.gitignore` rules stop at nested repositories;
    // outside one they apply anyway.
    let inside_repository = search_path
        .ancestors()
        .any(|directory| directory.join(".git").exists());

    let mut results = Vec::new();
    for entry in WalkBuilder::new(&search_path)
        .hidden(false)
        .require_git(inside_repository)
        .add_custom_ignore_filename(".fdignore")
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue;
        }
        let matched = if full_path {
            matcher.is_match(entry.path())
        } else {
            matcher.is_match(entry.file_name())
        };
        if !matched {
            continue;
        }
        let mut result = relative_display(&search_path, entry.path());
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            result.push('/');
        }
        results.push(result);
        if results.len() >= limit {
            break;
        }
    }
    if results.is_empty() {
        return Ok(ToolResult::text("No files found matching pattern"));
    }
    let mut notices = Vec::new();
    if results.len() >= limit {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    Ok(limited_output(&results, notices, &[]))
}

/// Entries of one directory, dotfiles included, directories marked with `/`.
pub(crate) fn ls_tool(
    cwd: &Path,
    path: Option<&str>,
    limit: Option<u64>,
) -> Result<ToolResult, ToolError> {
    let directory = resolve_to_cwd(cwd, path.unwrap_or("."));
    if !directory.exists() {
        return Err(failed(format!("Path not found: {}", directory.display())));
    }
    if !directory.is_dir() {
        return Err(failed(format!("Not a directory: {}", directory.display())));
    }
    let limit = limit.unwrap_or(LS_DEFAULT_LIMIT) as usize;
    let mut names = fs::read_dir(&directory)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| failed(format!("Cannot read directory: {error}")))?;
    names.sort_by_cached_key(|name| (name_sort_key(name), name.clone()));

    let mut results = Vec::new();
    let mut limit_reached = false;
    for name in names {
        if results.len() >= limit {
            limit_reached = true;
            break;
        }
        // Entries that cannot be inspected, such as broken links, are left out.
        let Ok(metadata) = fs::metadata(directory.join(&name)) else {
            continue;
        };
        results.push(if metadata.is_dir() {
            format!("{name}/")
        } else {
            name
        });
    }
    if results.is_empty() {
        return Ok(ToolResult::text("(empty directory)"));
    }
    let mut notices = Vec::new();
    if limit_reached {
        notices.push(format!(
            "{limit} entries limit reached. Use limit={} for more",
            limit.saturating_mul(2)
        ));
    }
    Ok(limited_output(&results, notices, &[]))
}

/// Upstream sorts names with `localeCompare` after lowercasing them. ASCII
/// follows the Unicode collation order (space, punctuation, digits, letters);
/// other characters sort after ASCII by code point.
fn name_sort_key(name: &str) -> Vec<u32> {
    const ASCII_ORDER: &str =
        " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789abcdefghijklmnopqrstuvwxyz";
    name.to_lowercase()
        .chars()
        .map(|character| match ASCII_ORDER.find(character) {
            Some(rank) => rank as u32,
            None if character.is_ascii() => 0,
            None => ASCII_ORDER.len() as u32 + character as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sort_in_collation_order_ignoring_case() {
        let mut names = vec![
            "Zeta.txt",
            "alpha.txt",
            "_under.txt",
            ".dot",
            "10.txt",
            "9.txt",
            "beta-file.txt",
            "betafile.txt",
            "sub",
            "Alpha.md",
            "\u{e9}t\u{e9}",
        ];
        names.sort_by_cached_key(|name| (name_sort_key(name), *name));
        assert_eq!(
            names,
            [
                "_under.txt",
                ".dot",
                "10.txt",
                "9.txt",
                "Alpha.md",
                "alpha.txt",
                "beta-file.txt",
                "betafile.txt",
                "sub",
                "Zeta.txt",
                "\u{e9}t\u{e9}",
            ]
        );
    }
}
