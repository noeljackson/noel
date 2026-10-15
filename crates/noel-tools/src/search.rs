//! `grep`, `find`, and `ls`. Upstream runs ripgrep and fd; these walk the
//! tree with ripgrep's own `ignore` and `globset` crates and reproduce the
//! flags upstream passes, so the same files are searched and listed. `ls`
//! lists through the workspace.
//!
//! In a workspace on another machine, `grep` and `find` list the tree there
//! with the workspace's `list_files` (the daemon walks it with the same
//! `ignore` crate, skipping what git ignores) and `grep` reads each file
//! through the workspace, then both match exactly as they do here.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;

use globset::{GlobBuilder, GlobMatcher};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::WalkBuilder;
use noel_env::{Context, ExecutionEnv, FileError, FileErrorCode, FileKind, ListFilesOptions};

use crate::paths::resolve_tool_path;
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
/// Files a remote `grep` reads at once.
const REMOTE_READ_DEPTH: usize = 16;

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

/// Every future's output, in order, running them all at once.
async fn join_all<'a, T>(mut futures: Vec<Pin<Box<dyn Future<Output = T> + Send + 'a>>>) -> Vec<T> {
    let mut outputs: Vec<Option<T>> = futures.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => *output = Some(value),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().flatten().collect()
}

/// `path/relative` in a workspace's own form.
fn below(path: &str, relative: &str) -> String {
    if path.ends_with('/') {
        format!("{path}{relative}")
    } else {
        format!("{path}/{relative}")
    }
}

/// Everything below `path` in a workspace on another machine, as the
/// `ignore` crate walks it there: hidden entries included, what git ignores
/// left out.
async fn remote_listing(
    env: &dyn ExecutionEnv,
    tool: &str,
    path: &str,
    cx: &Context,
) -> Result<noel_env::FileListing, ToolError> {
    let options = ListFilesOptions {
        hidden: Some(true),
        gitignore: Some(true),
        ..ListFilesOptions::default()
    };
    env.list_files(path, &options, cx)
        .await
        .map_err(|error: FileError| match error.code {
            FileErrorCode::NotSupported => failed(format!(
                "{tool} is not available in this workspace: {}",
                error.message
            )),
            _ => failed(error.message),
        })
}

/// What a remote search says when the workspace stopped listing early.
fn listing_notice(listing: &noel_env::FileListing) -> Option<String> {
    listing.truncated.then(|| {
        format!(
            "The workspace listed only the first {} entries. Search a narrower path",
            listing.entries.len()
        )
    })
}

/// A path below `root` with `/` separators.
fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// The pattern a `grep` call searches for, checked as ripgrep checks it.
fn grep_matcher(args: &GrepArgs<'_>) -> Result<regex::bytes::Regex, ToolError> {
    if args.pattern.contains('\n') {
        return Err(failed("the literal \"\\n\" is not allowed in a regex"));
    }
    let pattern = if args.literal {
        regex::escape(args.pattern)
    } else {
        args.pattern.to_string()
    };
    regex::bytes::RegexBuilder::new(&pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|error| failed(error.to_string()))
}

/// `--glob`, matched as ripgrep matches it: relative to `cwd`.
fn grep_overrides(cwd: &Path, glob: &str) -> Result<Override, ToolError> {
    // ripgrep resolves a glob with a directory against its working
    // directory, which upstream leaves at pi's.
    let mut overrides = OverrideBuilder::new(cwd);
    overrides
        .add(glob)
        .map_err(|error| failed(error.to_string()))?;
    overrides.build().map_err(|error| failed(error.to_string()))
}

/// The matches of one `grep` call so far.
struct GrepSearch {
    matcher: regex::bytes::Regex,
    context: usize,
    limit: usize,
    output: Vec<String>,
    match_count: usize,
    lines_truncated: bool,
}

impl GrepSearch {
    fn new(args: &GrepArgs<'_>, matcher: regex::bytes::Regex) -> Self {
        Self {
            matcher,
            context: args.context as usize,
            limit: args.limit.unwrap_or(GREP_DEFAULT_LIMIT).max(1) as usize,
            output: Vec::new(),
            match_count: 0,
            lines_truncated: false,
        }
    }

    fn push_line(&mut self, path: &str, number: usize, separator: char, text: &str) {
        let (text, was_cut) = truncate_line(&text.replace('\r', ""));
        self.lines_truncated |= was_cut;
        self.output
            .push(format!("{path}{separator}{number}{separator} {text}"));
    }

    /// Searches one file's bytes, shown as `display`; true once the match
    /// limit is reached. Only an `explicit` file (the search path itself)
    /// is searched past a NUL byte.
    fn file(&mut self, bytes: &[u8], explicit: bool, display: &str) -> bool {
        let mut searched = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
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
        // Context lines come from the file as upstream reads it: all of it,
        // with every line ending normalized.
        let mut file_lines: Option<Vec<String>> = None;
        let mut lines = searched.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        if searched.last().is_none_or(|byte| *byte == b'\n') {
            lines.pop();
        }
        for (index, line) in lines.iter().enumerate() {
            if !self.matcher.is_match(line) {
                continue;
            }
            self.match_count += 1;
            let number = index + 1;
            match std::str::from_utf8(line) {
                Ok(text) if self.context == 0 => self.push_line(display, number, ':', text),
                _ => {
                    let file_lines = file_lines.get_or_insert_with(|| {
                        String::from_utf8_lossy(bytes)
                            .replace("\r\n", "\n")
                            .replace('\r', "\n")
                            .split('\n')
                            .map(ToString::to_string)
                            .collect()
                    });
                    let first = number.saturating_sub(self.context).max(1);
                    let last = number.saturating_add(self.context).min(file_lines.len());
                    for current in first..=last {
                        let separator = if current == number { ':' } else { '-' };
                        let text = file_lines[current - 1].clone();
                        self.push_line(display, current, separator, &text);
                    }
                }
            }
            if self.match_count >= self.limit {
                return true;
            }
        }
        false
    }

    fn finish(self, extra: Option<String>) -> ToolResult {
        if self.match_count == 0 {
            return ToolResult::text("No matches found");
        }
        let limit = self.limit;
        let mut notices = Vec::new();
        if self.match_count >= limit {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit.saturating_mul(2)
            ));
        }
        notices.extend(extra);
        let lines_notice = format!(
            "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
        );
        let after_bytes = if self.lines_truncated {
            vec![lines_notice.as_str()]
        } else {
            Vec::new()
        };
        limited_output(&self.output, notices, &after_bytes)
    }
}

/// Like `rg --hidden [--ignore-case] [--fixed-strings] [--glob GLOB]`:
/// hidden files are searched, ignore files apply, and inside a git
/// repository so does `.gitignore`. Files are searched in path order.
pub(crate) async fn grep_tool(
    env: &dyn ExecutionEnv,
    args: GrepArgs<'_>,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    if !noel_env::is_local(env) {
        return grep_remote(env, args, cx).await;
    }
    let cwd = Path::new(env.cwd());
    let search_path = PathBuf::from(resolve_tool_path(env, args.path.unwrap_or("."), cx).await?);
    let Ok(metadata) = fs::metadata(&search_path) else {
        return Err(failed(format!("Path not found: {}", search_path.display())));
    };
    let is_directory = metadata.is_dir();
    let mut search = GrepSearch::new(&args, grep_matcher(&args)?);

    let mut walk = WalkBuilder::new(&search_path);
    walk.hidden(false)
        .add_custom_ignore_filename(".rgignore")
        .sort_by_file_name(|a, b| a.cmp(b));
    if let Some(glob) = args.glob {
        walk.overrides(grep_overrides(cwd, glob)?);
    }

    // Unreadable files and directories are skipped.
    for entry in walk.build().flatten() {
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
        let display = if is_directory && !explicit {
            relative_display(&search_path, entry.path())
        } else {
            entry.file_name().to_string_lossy().into_owned()
        };
        if search.file(&bytes, explicit, &display) {
            break;
        }
    }
    Ok(search.finish(None))
}

/// Whether `--glob` leaves out the file at `relative` below `root`, or a
/// directory on its way, as ripgrep's walk would.
fn overridden_out(overrides: &Override, root: &str, relative: &str) -> bool {
    let mut directory = String::new();
    let mut parts = relative.split('/').peekable();
    while let Some(part) = parts.next() {
        if !directory.is_empty() {
            directory.push('/');
        }
        directory.push_str(part);
        let is_dir = parts.peek().is_some();
        if overrides
            .matched(below(root, &directory), is_dir)
            .is_ignore()
        {
            return true;
        }
    }
    false
}

/// [`grep_tool`] in a workspace on another machine: the files its
/// `list_files` names, read through it.
async fn grep_remote(
    env: &dyn ExecutionEnv,
    args: GrepArgs<'_>,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let search_path = resolve_tool_path(env, args.path.unwrap_or("."), cx).await?;
    let found = follow(env, &search_path, cx).await?;
    let is_file = matches!(found, Found::File);
    let is_directory = match found {
        Found::Missing => return Err(failed(format!("Path not found: {search_path}"))),
        Found::Directory => true,
        Found::File | Found::Other => false,
    };
    let mut search = GrepSearch::new(&args, grep_matcher(&args)?);
    if !is_directory {
        // The search path itself is searched whatever it holds, if it is a
        // file.
        if is_file {
            if let Ok(bytes) = env.read_binary_file(&search_path, cx).await {
                search.file(&bytes, true, noel_env_basename(&search_path));
            }
        }
        return Ok(search.finish(None));
    }
    let overrides = args
        .glob
        .map(|glob| grep_overrides(Path::new(env.cwd()), glob))
        .transpose()?;
    let listing = remote_listing(env, "grep", &search_path, cx).await?;
    // Below the search path only regular files are searched, and symlinks
    // are not followed.
    let files = listing
        .entries
        .iter()
        .filter(|entry| entry.kind == FileKind::File)
        .filter(|entry| {
            overrides
                .as_ref()
                .is_none_or(|overrides| !overridden_out(overrides, &search_path, &entry.path))
        })
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>();
    'files: for batch in files.chunks(REMOTE_READ_DEPTH) {
        let reads = join_all(
            batch
                .iter()
                .map(|relative| {
                    let path = below(&search_path, relative);
                    Box::pin(async move { env.read_binary_file(&path, cx).await })
                        as Pin<Box<dyn Future<Output = noel_env::FileResult<Vec<u8>>> + Send + '_>>
                })
                .collect(),
        )
        .await;
        for (relative, read) in batch.iter().zip(reads) {
            // Unreadable files are skipped.
            let Ok(bytes) = read else {
                continue;
            };
            if search.file(&bytes, false, relative) {
                break 'files;
            }
        }
    }
    Ok(search.finish(listing_notice(&listing)))
}

/// The last component of a path in a workspace's own form.
fn noel_env_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

/// The glob a `find` call matches, and whether it matches whole paths.
fn find_matcher(pattern: &str) -> Result<(GlobMatcher, bool), ToolError> {
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
    Ok((matcher, full_path))
}

/// The output of a `find` call that listed `results`.
fn find_output(results: &[String], limit: usize, extra: Option<String>) -> ToolResult {
    if results.is_empty() {
        return ToolResult::text("No files found matching pattern");
    }
    let mut notices = Vec::new();
    if results.len() >= limit {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    notices.extend(extra);
    limited_output(results, notices, &[])
}

/// Like `fd --glob --hidden`: files and directories whose name matches the
/// glob, or whose path does when the glob has a `/`. Hidden entries are
/// listed and ignore files apply, `.gitignore` also outside a repository.
/// Results are in path order.
pub(crate) async fn find_tool(
    env: &dyn ExecutionEnv,
    pattern: &str,
    path: Option<&str>,
    limit: Option<u64>,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    if !noel_env::is_local(env) {
        return find_remote(env, pattern, path, limit, cx).await;
    }
    let search_path = PathBuf::from(resolve_tool_path(env, path.unwrap_or("."), cx).await?);
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
    let (matcher, full_path) = find_matcher(pattern)?;
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
    Ok(find_output(&results, limit, None))
}

/// [`find_tool`] in a workspace on another machine: what its `list_files`
/// names.
async fn find_remote(
    env: &dyn ExecutionEnv,
    pattern: &str,
    path: Option<&str>,
    limit: Option<u64>,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let search_path = resolve_tool_path(env, path.unwrap_or("."), cx).await?;
    match follow(env, &search_path, cx).await? {
        Found::Missing => return Err(failed(format!("Path not found: {search_path}"))),
        Found::File | Found::Other => {
            return Err(failed(format!("Not a directory: {search_path}")))
        }
        Found::Directory => {}
    }
    let limit = limit.unwrap_or(FIND_DEFAULT_LIMIT).max(1) as usize;
    let (matcher, full_path) = find_matcher(pattern)?;
    let listing = remote_listing(env, "find", &search_path, cx).await?;
    let mut results = Vec::new();
    for entry in &listing.entries {
        let matched = if full_path {
            matcher.is_match(below(&search_path, &entry.path))
        } else {
            matcher.is_match(noel_env_basename(&entry.path))
        };
        if !matched {
            continue;
        }
        let mut result = entry.path.clone();
        if entry.kind == FileKind::Directory {
            result.push('/');
        }
        results.push(result);
        if results.len() >= limit {
            break;
        }
    }
    Ok(find_output(&results, limit, listing_notice(&listing)))
}

/// What a path names once links are followed, as Node's `existsSync` and
/// `statSync` see it.
enum Found {
    Missing,
    Directory,
    File,
    /// A socket, FIFO or device.
    Other,
}

/// A workspace on another machine that cannot be reached fails the call
/// with why, rather than calling the path missing.
async fn follow(env: &dyn ExecutionEnv, path: &str, cx: &Context) -> Result<Found, ToolError> {
    let unreachable = |error: &FileError| {
        (error.code == FileErrorCode::Unknown && !noel_env::is_local(env))
            .then(|| failed(error.message.clone()))
    };
    let info = match env.file_info(path, cx).await {
        Ok(info) => info,
        // A socket, FIFO or device, which the workspace does not describe.
        Err(error) if error.code == FileErrorCode::Invalid => return Ok(Found::Other),
        Err(error) => return unreachable(&error).map_or(Ok(Found::Missing), Err),
    };
    let kind = if info.kind == FileKind::Symlink {
        let target = match env.canonical_path(path, cx).await {
            Ok(target) => target,
            Err(error) => return unreachable(&error).map_or(Ok(Found::Missing), Err),
        };
        match env.file_info(&target, cx).await {
            Ok(info) => info.kind,
            Err(error) if error.code == FileErrorCode::Invalid => return Ok(Found::Other),
            Err(error) => return unreachable(&error).map_or(Ok(Found::Missing), Err),
        }
    } else {
        info.kind
    };
    Ok(match kind {
        FileKind::Directory => Found::Directory,
        FileKind::File => Found::File,
        FileKind::Symlink => Found::Other,
    })
}

/// Entries of one directory, dotfiles included, directories marked with `/`.
pub(crate) async fn ls_tool(
    env: &dyn ExecutionEnv,
    path: Option<&str>,
    limit: Option<u64>,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let directory = resolve_tool_path(env, path.unwrap_or("."), cx).await?;
    match follow(env, &directory, cx).await? {
        Found::Missing => return Err(failed(format!("Path not found: {directory}"))),
        Found::File | Found::Other => return Err(failed(format!("Not a directory: {directory}"))),
        Found::Directory => {}
    }
    let limit = limit.unwrap_or(LS_DEFAULT_LIMIT) as usize;
    let cannot_read = |error: noel_env::FileError| {
        failed(format!("Cannot read directory: {}", error.to_io_error()))
    };
    let mut reader = env
        .open_dir_reader(&directory, cx)
        .await
        .map_err(cannot_read)?;
    let mut entries = Vec::new();
    let listed = loop {
        match reader.next(1000, cx).await {
            Ok(page) => {
                entries.extend(page.entries);
                if page.done {
                    break Ok(());
                }
            }
            Err(error) => break Err(cannot_read(error)),
        }
    };
    reader.close(cx).await;
    listed?;
    entries.sort_by_cached_key(|entry| (name_sort_key(&entry.name), entry.name.clone()));

    let mut results = Vec::new();
    let mut limit_reached = false;
    for entry in entries {
        if results.len() >= limit {
            limit_reached = true;
            break;
        }
        // Links are followed; broken ones are left out.
        let directory = match entry.kind {
            FileKind::Directory => true,
            FileKind::File => false,
            FileKind::Symlink => match follow(env, &entry.path, cx).await? {
                Found::Missing => continue,
                Found::Directory => true,
                Found::File | Found::Other => false,
            },
        };
        results.push(if directory {
            format!("{}/", entry.name)
        } else {
            entry.name
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
