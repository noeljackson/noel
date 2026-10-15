//! noel's built-in tools. They work in a workspace, an
//! [`ExecutionEnv`]: `read`, `write`, `edit`, `ls` and `bash` reach files
//! and run commands only through it. `grep` and `find` search with
//! ripgrep's crates, here or, in a workspace on another machine, over what
//! the workspace lists and reads.

mod mutation;
mod output;
mod paths;
mod read;
mod search;
mod truncate;
mod web;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use noel_env::{
    Context, ExecOptions, ExecutionEnv, ExecutionErrorCode, FileErrorCode, LocalEnv,
    LocalEnvOptions, OutputChunk, ShellCommand, SpillOptions,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Notify;
use tokio::time::Instant;

pub use noel_env::{kill_running_tool_groups, spawn_tool_process, ToolProcessGroup};
use output::{OutputAccumulator, OutputSnapshot};
use paths::resolve_tool_path;
use truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ToolRequest {
    Read {
        path: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    /// Without `timeout_ms` the command runs until it exits.
    Bash {
        command: String,
        timeout_ms: Option<u64>,
    },
    /// All edits apply to the original file together, or none do.
    Edit {
        path: String,
        edits: Vec<TextEdit>,
    },
    Write {
        path: String,
        content: String,
    },
    Grep {
        path: Option<String>,
        pattern: String,
        #[serde(default)]
        glob: Option<String>,
        literal: Option<bool>,
        ignore_case: Option<bool>,
        context: Option<u64>,
        limit: Option<u64>,
    },
    Find {
        pattern: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
    },
    Ls {
        path: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
    },
    WebSearch {
        query: String,
        limit: Option<u64>,
    },
    WebFetch {
        url: String,
        max_chars: Option<u64>,
    },
}

/// One exact-text replacement: `find` must occur exactly once in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub find: String,
    pub replace: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
    /// Images that go to the model with the output (`read` of an image).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
}

impl ToolResult {
    pub fn text(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(output)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    pub data_base64: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("web tool: {0}")]
    Web(String),
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("failed to access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}")]
    InvalidEdit(String),
    /// A failure whose message is the whole result, worded as upstream does.
    #[error("{0}")]
    Failed(String),
    #[error("command timed out after {0} ms")]
    Timeout(u64),
}

#[derive(Debug, Clone)]
pub struct ToolRuntimeOptions {
    /// The shell of the local workspace [`execute_tool`] makes; a workspace
    /// passed to [`execute_tool_in`] brings its own.
    pub shell_path: Option<String>,
    pub shell_command_prefix: Option<String>,
    pub offline: bool,
    /// Changes to the environment a bash command inherits: a variable to
    /// set, or with `None` one to remove.
    pub bash_env: Vec<(String, Option<String>)>,
    /// Scale images that `read` returns down to what providers accept
    /// (the `images.autoResize` setting).
    pub auto_resize_images: bool,
    /// Never return images from `read` (the `images.blockImages` setting).
    pub block_images: bool,
}

impl Default for ToolRuntimeOptions {
    fn default() -> Self {
        Self {
            shell_path: None,
            shell_command_prefix: None,
            offline: false,
            bash_env: Vec::new(),
            auto_resize_images: true,
            block_images: false,
        }
    }
}

pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    [
        "read",
        "bash",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
        "todo",
        "web_search",
        "web_fetch",
    ]
    .into_iter()
    .map(|name| ToolDefinition {
        name: name.to_string(),
        read_only: matches!(
            name,
            "read" | "grep" | "find" | "ls" | "todo" | "web_search" | "web_fetch"
        ),
    })
    .collect()
}

/// The local workspace at `cwd` that [`execute_tool`] runs tools in.
pub fn local_env(cwd: &Path, options: &ToolRuntimeOptions) -> LocalEnv {
    LocalEnv::with_options(
        cwd,
        LocalEnvOptions {
            shell_path: options.shell_path.clone(),
        },
    )
}

/// Runs a tool in the local workspace at `cwd`.
pub async fn execute_tool(
    cwd: &Path,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
) -> Result<ToolResult, ToolError> {
    execute_tool_in(&local_env(cwd, options), request, options).await
}

/// Runs a tool in the workspace `env`.
pub async fn execute_tool_in(
    env: &dyn ExecutionEnv,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
) -> Result<ToolResult, ToolError> {
    execute_tool_streaming_in(env, request, options, &mut |_| {}).await
}

/// What a running tool has printed so far, as [`execute_tool_streaming`]
/// reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputSoFar<'a> {
    /// The end of the output, within the limits of the result (the last
    /// 2000 lines or 50 KB).
    pub text: &'a str,
    /// Whether output before `text` was left out to keep within them.
    pub truncated: bool,
}

/// [`execute_tool`] that also reports a `bash` command's output while it
/// runs. `on_output` gets everything printed so far, within the limits of
/// the result (see [`OutputSoFar`]): once, empty, when the command starts,
/// then as output arrives, at most every [`BASH_UPDATE_THROTTLE`]. Other
/// tools report nothing before their result.
pub async fn execute_tool_streaming(
    cwd: &Path,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
    on_output: &mut (dyn FnMut(OutputSoFar<'_>) + Send),
) -> Result<ToolResult, ToolError> {
    execute_tool_streaming_in(&local_env(cwd, options), request, options, on_output).await
}

/// [`execute_tool_streaming`] in the workspace `env`. Dropping the future
/// stops the tool, and kills a running command's processes.
pub async fn execute_tool_streaming_in(
    env: &dyn ExecutionEnv,
    request: ToolRequest,
    options: &ToolRuntimeOptions,
    on_output: &mut (dyn FnMut(OutputSoFar<'_>) + Send),
) -> Result<ToolResult, ToolError> {
    let cx = Context::background();
    if options.offline
        && matches!(
            &request,
            ToolRequest::WebFetch { .. } | ToolRequest::WebSearch { .. }
        )
    {
        return Err(ToolError::Web(
            "network tools are disabled in offline mode".to_string(),
        ));
    }
    match request {
        ToolRequest::Read {
            path,
            offset,
            limit,
        } => read::read_tool(env, &path, offset, limit, options, &cx).await,
        ToolRequest::Bash {
            command,
            timeout_ms,
        } => bash_tool(env, &command, timeout_ms, options, on_output, &cx).await,
        ToolRequest::Edit { path, edits } => edit_tool(env, &path, &edits, &cx).await,
        ToolRequest::Write { path, content } => write_tool(env, &path, &content, &cx).await,
        ToolRequest::Grep {
            path,
            pattern,
            glob,
            literal,
            ignore_case,
            context,
            limit,
        } => {
            search::grep_tool(
                env,
                search::GrepArgs {
                    pattern: &pattern,
                    path: path.as_deref(),
                    glob: glob.as_deref(),
                    ignore_case: ignore_case.unwrap_or(false),
                    literal: literal.unwrap_or(false),
                    context: context.unwrap_or(0),
                    limit,
                },
                &cx,
            )
            .await
        }
        ToolRequest::Find {
            pattern,
            path,
            limit,
        } => search::find_tool(env, &pattern, path.as_deref(), limit, &cx).await,
        ToolRequest::Ls { path, limit } => search::ls_tool(env, path.as_deref(), limit, &cx).await,
        ToolRequest::WebSearch { query, limit } => {
            tokio::time::timeout(Duration::from_secs(30), web::search(&query, limit))
                .await
                .map_err(|_| ToolError::Timeout(30_000))?
        }
        ToolRequest::WebFetch { url, max_chars } => {
            tokio::time::timeout(Duration::from_secs(30), web::fetch(&url, max_chars))
                .await
                .map_err(|_| ToolError::Timeout(30_000))?
        }
    }
}

/// Caps the output of a tool noel does not implement (an extension tool) like
/// the built-in ones: the first 2000 lines or 50 KB, with the whole output
/// saved to a temp file the notice names.
pub fn cap_tool_output(output: String) -> String {
    let truncation = truncate_head(&output, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    if !truncation.truncated {
        return output;
    }
    let saved = output::create_output_file("noel-tool")
        .and_then(|(path, mut file)| {
            std::io::Write::write_all(&mut file, output.as_bytes())?;
            Ok(path)
        })
        .map_or_else(
            |error| format!("Full output could not be saved: {error}"),
            |path| format!("Full output: {}", path.display()),
        );
    let cause = if truncation.by_lines {
        String::new()
    } else {
        format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
    };
    format!(
        "{}\n\n[Showing lines 1-{} of {}{cause}. {saved}]",
        truncation.content, truncation.output_lines, truncation.total_lines
    )
}

/// The shortest time between two reports of a running command's output
/// (upstream `BASH_UPDATE_THROTTLE_MS`).
pub const BASH_UPDATE_THROTTLE: Duration = Duration::from_millis(100);

/// The longest timeout a workspace accepts (a 32-bit millisecond timer).
const MAX_TIMEOUT_MS: u64 = 2_147_483_647;

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The text of a finished command: the end of its output, and when that is
/// not all of it, where the rest is.
fn bash_output_text(snapshot: &OutputSnapshot, empty: &str) -> String {
    let truncation = &snapshot.truncation;
    let mut text = if truncation.content.is_empty() {
        empty.to_string()
    } else {
        truncation.content.clone()
    };
    if !truncation.truncated {
        return text;
    }
    let path = snapshot
        .full_output_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let last = truncation.total_lines;
    let first = last + 1 - truncation.output_lines;
    let limit = format_size(DEFAULT_MAX_BYTES);
    text.push_str(&if truncation.last_line_partial {
        format!(
            "\n\n[Showing last {} of line {last} (line is {}). Full output: {path}]",
            format_size(truncation.output_bytes),
            format_size(snapshot.last_line_bytes)
        )
    } else if truncation.by_lines {
        format!("\n\n[Showing lines {first}-{last} of {last}. Full output: {path}]")
    } else {
        format!("\n\n[Showing lines {first}-{last} of {last} ({limit} limit). Full output: {path}]")
    });
    text
}

fn with_status(text: String, status: &str) -> String {
    if text.is_empty() {
        status.to_string()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// What a running command printed, and when it was last reported.
struct BashReporter<'a> {
    output: OutputAccumulator,
    on_output: &'a mut (dyn FnMut(OutputSoFar<'_>) + Send),
    /// Output not reported yet is reported once the throttle allows.
    unreported: bool,
    last_report: Option<Instant>,
    report_due: Option<Instant>,
    /// Why the output could not be kept, which stopped the command.
    error: Option<io::Error>,
}

impl BashReporter<'_> {
    fn report(&mut self) {
        (self.unreported, self.report_due, self.last_report) = (false, None, Some(Instant::now()));
        (self.on_output)(OutputSoFar {
            text: &self.output.tail(),
            truncated: self.output.truncated(),
        });
    }

    /// Keeps a chunk of output and reports it now, or returns true when the
    /// throttle moved the report to `report_due`.
    fn append(&mut self, bytes: &[u8]) -> Result<bool, String> {
        if let Err(error) = self.output.append(bytes) {
            let message = error.to_string();
            self.error = Some(error);
            return Err(message);
        }
        self.unreported = true;
        let due = self.last_report.map(|at| at + BASH_UPDATE_THROTTLE);
        match due.filter(|due| *due > Instant::now()) {
            Some(due) => {
                self.report_due = Some(due);
                Ok(true)
            }
            None => {
                self.report();
                Ok(false)
            }
        }
    }
}

fn lock<'a, 'b>(reporter: &'a Mutex<BashReporter<'b>>) -> MutexGuard<'a, BashReporter<'b>> {
    reporter.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `command` through the workspace's shell (upstream `bash.ts`).
/// stdout and stderr are one stream in the order written; the model gets
/// its last 2000 lines or 50 KB, and a longer output is saved whole to a
/// temp file. A timeout or a non-zero exit is an error result that still
/// carries the output. While the command runs, `on_output` gets its output
/// so far (see [`execute_tool_streaming`]).
async fn bash_tool(
    env: &dyn ExecutionEnv,
    command: &str,
    timeout_ms: Option<u64>,
    options: &ToolRuntimeOptions,
    on_output: &mut (dyn FnMut(OutputSoFar<'_>) + Send),
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    // Like upstream, the first report says the command is starting, even if
    // it then cannot start.
    on_output(OutputSoFar {
        text: "",
        truncated: false,
    });
    let command = match &options.shell_command_prefix {
        Some(prefix) if !prefix.trim().is_empty() => format!("{prefix}\n{command}"),
        _ => command.to_string(),
    };
    let cwd = PathBuf::from(env.cwd());
    let io_error = |source| ToolError::Io {
        path: cwd.clone(),
        source,
    };
    let reporter = Mutex::new(BashReporter {
        output: OutputAccumulator::new("noel-bash"),
        on_output,
        unreported: false,
        last_report: None,
        report_due: None,
        error: None,
    });
    // Wakes the loop below when a report was put off.
    let report_put_off = Notify::new();
    let mut on_chunk = |chunk: OutputChunk<'_>| {
        if lock(&reporter).append(chunk.raw.unwrap_or(chunk.text.as_bytes()))? {
            report_put_off.notify_one();
        }
        Ok(())
    };
    // In a workspace on another machine the complete output is kept there
    // too, as Durable's bash tool keeps it, so the path the result names is
    // one the workspace's `read` can open.
    let remote = !noel_env::is_local(env);
    let result = {
        // stdout and stderr share one pipe, so the output is in the order
        // it was written. The command runs in its own session and process
        // group: a timeout or an interrupt (dropping this future) kills the
        // whole tree, not just the shell.
        let exec = env.exec(
            ShellCommand::Script(command),
            ExecOptions {
                env: options.bash_env.clone(),
                // A zero timeout still starts the command and kills it at
                // once; a longer one than the timer holds becomes its
                // longest.
                timeout: timeout_ms
                    .map(|timeout_ms| Duration::from_millis(timeout_ms.clamp(1, MAX_TIMEOUT_MS))),
                on_output: Some(&mut on_chunk),
                merge_stderr: true,
                spill: remote.then_some(SpillOptions {
                    after_bytes: DEFAULT_MAX_BYTES as u64,
                    after_lines: DEFAULT_MAX_LINES as u64,
                }),
                ..ExecOptions::default()
            },
            cx,
        );
        tokio::pin!(exec);
        loop {
            let due = lock(&reporter).report_due;
            tokio::select! {
                biased;
                result = &mut exec => break result,
                () = report_put_off.notified() => {}
                () = sleep_until(due) => {
                    let mut reporter = lock(&reporter);
                    // A report made since, or put off again, is not due.
                    if reporter.report_due.is_some_and(|due| due <= Instant::now()) {
                        reporter.report();
                    }
                }
            }
        }
    };
    let mut reporter = reporter
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let spill_path = match &result {
        Ok(result) => result.spill_path.clone(),
        Err(error) => error.spill_path.clone(),
    };
    let exit_code = match result {
        Ok(result) => Some(result.exit_code),
        Err(error) => match error.code {
            ExecutionErrorCode::Timeout => None,
            ExecutionErrorCode::CallbackError => {
                return Err(io_error(
                    reporter
                        .error
                        .take()
                        .unwrap_or_else(|| io::Error::other(error.message)),
                ));
            }
            ExecutionErrorCode::SpawnError if error.errno.is_some() => {
                return Err(io_error(io::Error::from_raw_os_error(
                    error.errno.unwrap_or_default(),
                )));
            }
            _ => return Err(ToolError::Failed(error.message)),
        },
    };
    if reporter.unreported {
        reporter.report();
    }

    let mut snapshot = reporter.output.finish().map_err(io_error)?;
    if let (Some(spilled), Some(local)) = (spill_path, &snapshot.full_output_path) {
        // The workspace's copy replaces the one kept here.
        let _ = std::fs::remove_file(local);
        snapshot.full_output_path = Some(PathBuf::from(spilled));
    }
    let Some(code) = exit_code else {
        // Killed at the timeout; what it wrote is kept.
        let seconds = timeout_ms.unwrap_or_default() as f64 / 1000.0;
        return Ok(ToolResult::error(with_status(
            bash_output_text(&snapshot, ""),
            &format!("Command timed out after {seconds} seconds"),
        )));
    };
    let text = bash_output_text(&snapshot, "(no output)");
    Ok(match code {
        0 => ToolResult::text(text),
        code => ToolResult::error(with_status(
            text,
            &format!("Command exited with code {code}"),
        )),
    })
}

/// The error code Node reports for a failure, where upstream's tools put it
/// in their results.
pub(crate) fn node_error_code(code: FileErrorCode) -> Option<&'static str> {
    match code {
        FileErrorCode::NotFound => Some("ENOENT"),
        FileErrorCode::PermissionDenied => Some("EACCES"),
        FileErrorCode::IsDirectory => Some("EISDIR"),
        _ => None,
    }
}

async fn edit_tool(
    env: &dyn ExecutionEnv,
    input_path: &str,
    edits: &[TextEdit],
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let path = resolve_tool_path(env, input_path, cx).await?;
    // A file that cannot be read or written is reported as upstream does.
    let file_error = |error: noel_env::FileError| match node_error_code(error.code) {
        Some(code) => ToolError::Failed(format!(
            "Could not edit file: {input_path}. Error code: {code}."
        )),
        None => ToolError::Io {
            path: PathBuf::from(&path),
            source: error.to_io_error(),
        },
    };
    mutation::with_file_mutation_queue(env, &path, cx, async {
        let bytes = env.read_binary_file(&path, cx).await.map_err(file_error)?;
        // Strictly UTF-8: an edit must not rewrite bytes it could not read.
        let content = io::read_to_string(bytes.as_slice()).map_err(|source| ToolError::Io {
            path: PathBuf::from(&path),
            source,
        })?;
        let next = apply_text_edits(&content, edits, input_path)?;
        env.write_file(&path, next.as_bytes(), cx)
            .await
            .map_err(file_error)?;
        Ok(ToolResult::text(format!(
            "Successfully replaced {} block(s) in {input_path}.",
            edits.len()
        )))
    })
    .await
}

/// Applies every edit to `content`, or none (upstream
/// `applyEditsToNormalizedContent`, without fuzzy matching). Each `find` must
/// occur exactly once in the original content and the matched regions must
/// not overlap. Edits use the file's line endings (CRLF when its first line
/// ends in CRLF); the rest of the file, including a BOM, is kept byte for
/// byte.
fn apply_text_edits(content: &str, edits: &[TextEdit], path: &str) -> Result<String, ToolError> {
    if edits.is_empty() {
        return Err(ToolError::InvalidEdit(format!(
            "edits must contain at least one replacement for {path}."
        )));
    }
    let crlf = content
        .find('\n')
        .is_some_and(|newline| content[..newline].ends_with('\r'));
    let in_file_endings = |text: &str| {
        let text = text.replace("\r\n", "\n");
        if crlf {
            text.replace('\n', "\r\n")
        } else {
            text
        }
    };
    let single = edits.len() == 1;
    let mut matches = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let find = in_file_endings(&edit.find);
        if find.is_empty() {
            return Err(ToolError::InvalidEdit(if single {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{index}].oldText must not be empty in {path}.")
            }));
        }
        let mut occurrences = content.match_indices(find.as_str());
        let Some((start, _)) = occurrences.next() else {
            return Err(ToolError::InvalidEdit(if single {
                format!("Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines.")
            } else {
                format!("Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines.")
            }));
        };
        let count = 1 + occurrences.count();
        if count > 1 {
            return Err(ToolError::InvalidEdit(if single {
                format!("Found {count} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique.")
            } else {
                format!("Found {count} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique.")
            }));
        }
        matches.push((start, find.len(), in_file_endings(&edit.replace), index));
    }
    matches.sort_by_key(|(start, ..)| *start);
    for pair in matches.windows(2) {
        let ((start, len, _, first), (next_start, _, _, second)) = (&pair[0], &pair[1]);
        if start + len > *next_start {
            return Err(ToolError::InvalidEdit(format!(
                "edits[{first}] and edits[{second}] overlap in {path}. Merge them into one edit or target disjoint regions."
            )));
        }
    }
    let mut next = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, len, replace, _) in &matches {
        next.push_str(&content[cursor..*start]);
        next.push_str(replace);
        cursor = start + len;
    }
    next.push_str(&content[cursor..]);
    Ok(next)
}

async fn write_tool(
    env: &dyn ExecutionEnv,
    input_path: &str,
    content: &str,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let path = resolve_tool_path(env, input_path, cx).await?;
    mutation::with_file_mutation_queue(env, &path, cx, async {
        // Missing parent directories are created; a failure names the path
        // that failed, the file or a parent.
        env.write_file(&path, content.as_bytes(), cx)
            .await
            .map_err(|error| ToolError::Io {
                path: PathBuf::from(error.path.as_deref().unwrap_or(&path)),
                source: error.to_io_error(),
            })?;
        Ok(ToolResult::text(format!(
            "Successfully wrote to {input_path}"
        )))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    #[tokio::test]
    async fn web_tools_are_read_only_and_offline_rejects_network_before_connecting() {
        let definitions = builtin_tool_definitions();
        for name in ["web_search", "web_fetch"] {
            assert!(
                definitions
                    .iter()
                    .find(|tool| tool.name == name)
                    .unwrap()
                    .read_only
            );
        }
        let options = ToolRuntimeOptions {
            offline: true,
            ..Default::default()
        };
        for request in [
            ToolRequest::WebFetch {
                url: "http://127.0.0.1:1/".into(),
                max_chars: None,
            },
            ToolRequest::WebSearch {
                query: "rust".into(),
                limit: None,
            },
        ] {
            assert!(execute_tool(Path::new("."), request, &options)
                .await
                .unwrap_err()
                .to_string()
                .contains("offline"));
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_an_error_that_keeps_the_output() {
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command: "printf failed-output; exit 7".into(),
                timeout_ms: Some(1000),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .unwrap();
        assert!(result.is_error);
        assert_eq!(result.output, "failed-output\n\nCommand exited with code 7");
    }

    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        // A timed-out bash call must not orphan its children: the shell runs
        // in its own process group and the timeout kills the group, so a
        // long-running child script dies with the tool call.
        let marker = std::env::temp_dir().join(format!(
            "noel-bash-orphan-{}-{}",
            std::process::id(),
            new_marker_nonce()
        ));
        let marker_str = marker.to_string_lossy().to_string();
        let command = format!("sh -c 'sleep 30; touch {marker_str}' & sleep 30");
        let started = std::time::Instant::now();
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command,
                timeout_ms: Some(750),
            },
            &ToolRuntimeOptions::default(),
        )
        .await;
        let result = result.expect("a timeout is a result, not a failure to run");
        assert!(result.is_error);
        assert_eq!(result.output, "Command timed out after 0.75 seconds");
        // Give the group kill a moment to land, then verify no orphan lives on
        // to create the marker file later.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let orphaned = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("touch {marker_str}"))
            .output()
            .await
            .expect("pgrep");
        assert!(
            !orphaned.status.success(),
            "timed-out bash must kill its process group, not just the shell"
        );
        assert!(!marker.exists(), "orphaned child outlived the timeout");
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_file(&marker);
    }

    fn new_marker_nonce() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NONCE: AtomicU64 = AtomicU64::new(0);
        NONCE.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn sh_options() -> ToolRuntimeOptions {
        ToolRuntimeOptions {
            shell_path: Some("/bin/sh".into()),
            ..Default::default()
        }
    }

    /// A pid file path and a command that publishes the pid of a background
    /// `sleep 30` there before waiting on it.
    fn background_sleep_command(name: &str) -> (PathBuf, String) {
        let pid_file = std::env::temp_dir().join(format!(
            "noel-bash-{name}-{}-{}.pid",
            std::process::id(),
            new_marker_nonce()
        ));
        let path = pid_file.display();
        let command =
            format!("sleep 30 & echo $! > '{path}.tmp' && mv '{path}.tmp' '{path}'; wait");
        (pid_file, command)
    }

    async fn read_pid_file(pid_file: &Path) -> i32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(pid) = fs::read_to_string(pid_file) {
                let _ = fs::remove_file(pid_file);
                return pid.trim().parse().expect("pid file holds a pid");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "command never wrote {}",
                pid_file.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Whether `pid` still runs. A killed process stays a zombie (state Z)
    /// until it is reaped, which counts as exited.
    fn process_is_running(pid: i32) -> bool {
        if cfg!(target_os = "linux") {
            return fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
            });
        }
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
    }

    async fn assert_exits(pid: i32, context: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_running(pid) {
            if std::time::Instant::now() >= deadline {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
                panic!("background child {pid} outlived the bash tool: {context}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn bash_is_non_interactive_in_its_own_session() {
        // `cat` sees end of input at once instead of reading noel's stdin, and
        // the shell leads a new session with no controlling terminal.
        let linux = cfg!(target_os = "linux");
        let started = std::time::Instant::now();
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command: if linux {
                    "cat; echo cat-done; cut -d' ' -f1,5,6,7 /proc/$$/stat"
                } else {
                    "cat; echo cat-done"
                }
                .into(),
                timeout_ms: Some(10_000),
            },
            &sh_options(),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!result.is_error, "{}", result.output);
        let mut lines = result.output.lines();
        assert_eq!(lines.next(), Some("cat-done"), "{}", result.output);
        if linux {
            let fields = lines
                .next()
                .unwrap()
                .split(' ')
                .map(|field| field.parse::<i64>().unwrap())
                .collect::<Vec<_>>();
            let [pid, pgrp, session, tty] = fields[..] else {
                panic!("unexpected stat fields: {fields:?}");
            };
            assert_eq!((pgrp, session, tty), (pid, pid, 0));
        }
    }

    #[tokio::test]
    async fn bash_timeout_kills_background_children() {
        let (pid_file, command) = background_sleep_command("timeout");
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command,
                timeout_ms: Some(2_000),
            },
            &sh_options(),
        )
        .await;
        assert_eq!(result.unwrap().output, "Command timed out after 2 seconds");
        let pid = read_pid_file(&pid_file).await;
        assert_exits(pid, "timeout").await;
    }

    #[tokio::test]
    async fn dropping_the_bash_future_kills_background_children() {
        // An interrupt drops the in-flight tool future; that alone must take
        // the whole process group down.
        let (pid_file, command) = background_sleep_command("drop");
        let tool = tokio::spawn(async move {
            execute_tool(
                Path::new("."),
                ToolRequest::Bash {
                    command,
                    timeout_ms: None,
                },
                &sh_options(),
            )
            .await
        });
        let pid = read_pid_file(&pid_file).await;
        assert!(process_is_running(pid));
        tool.abort();
        assert!(tool.await.unwrap_err().is_cancelled());
        assert_exits(pid, "dropped future").await;
    }

    #[tokio::test]
    async fn finished_bash_leaves_deliberate_background_jobs_running() {
        let pid_file = std::env::temp_dir().join(format!(
            "noel-bash-detached-{}-{}.pid",
            std::process::id(),
            new_marker_nonce()
        ));
        let result = execute_tool(
            Path::new("."),
            ToolRequest::Bash {
                command: format!(
                    "sleep 5 >/dev/null 2>&1 & echo $! > '{}'",
                    pid_file.display()
                ),
                timeout_ms: Some(10_000),
            },
            &sh_options(),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{}", result.output);
        let pid = read_pid_file(&pid_file).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let running = process_is_running(pid);
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        assert!(running, "a normal finish must not kill background jobs");
    }

    /// A fresh directory under the system temp directory.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "noel-tools-{name}-{}-{}",
            std::process::id(),
            new_marker_nonce()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    async fn run(cwd: &Path, request: ToolRequest) -> Result<ToolResult, ToolError> {
        execute_tool(cwd, request, &ToolRuntimeOptions::default()).await
    }

    fn read(path: &str, offset: Option<u64>, limit: Option<u64>) -> ToolRequest {
        ToolRequest::Read {
            path: path.to_string(),
            offset,
            limit,
        }
    }

    fn bash(command: &str) -> ToolRequest {
        ToolRequest::Bash {
            command: command.to_string(),
            timeout_ms: Some(10_000),
        }
    }

    fn grep(pattern: &str, path: Option<&str>, glob: Option<&str>) -> ToolRequest {
        ToolRequest::Grep {
            path: path.map(ToString::to_string),
            pattern: pattern.to_string(),
            glob: glob.map(ToString::to_string),
            literal: None,
            ignore_case: None,
            context: None,
            limit: None,
        }
    }

    fn find(pattern: &str) -> ToolRequest {
        ToolRequest::Find {
            pattern: pattern.to_string(),
            path: None,
            limit: None,
        }
    }

    fn ls(path: Option<&str>) -> ToolRequest {
        ToolRequest::Ls {
            path: path.map(ToString::to_string),
            limit: None,
        }
    }

    #[tokio::test]
    async fn read_caps_lines_and_says_how_to_continue() {
        let cwd = temp_dir("read-cap");
        let content = (1..=3_000)
            .map(|line| format!("line-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(cwd.join("big.txt"), content).expect("write big file");

        let capped = run(&cwd, read("big.txt", None, None)).await.expect("read");
        assert!(capped.output.starts_with("line-1\nline-2\n"));
        assert!(capped.output.ends_with(
            "line-2000\n\n[Showing lines 1-2000 of 3000. Use offset=2001 to continue.]"
        ));

        // The offset from the notice continues exactly there.
        let resumed = run(&cwd, read("big.txt", Some(2001), Some(2)))
            .await
            .expect("resumed read");
        assert_eq!(
            resumed.output,
            "line-2001\nline-2002\n\n[998 more lines in file. Use offset=2003 to continue.]"
        );

        let past = run(&cwd, read("big.txt", Some(9_999), None)).await;
        assert_eq!(
            past.expect_err("offset past the end").to_string(),
            "Offset 9999 is beyond end of file (3000 lines total)"
        );
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn read_returns_images_as_attachments_unless_blocked() {
        use base64::Engine as _;
        let cwd = temp_dir("read-image");
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(3, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");
        // The type comes from the content, not the name.
        fs::write(cwd.join("picture.dat"), png.get_ref()).expect("write image");

        let result = run(&cwd, read("picture.dat", None, None))
            .await
            .expect("read");
        assert_eq!(result.output, "Read image file [image/png]");
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&result.images[0].data_base64)
                .expect("base64"),
            *png.get_ref()
        );

        let blocked = execute_tool(
            &cwd,
            read("picture.dat", None, None),
            &ToolRuntimeOptions {
                block_images: true,
                ..Default::default()
            },
        )
        .await
        .expect("blocked read");
        assert_eq!(
            blocked.output,
            "Read image file [image/png]\nImage reading is disabled."
        );
        assert!(blocked.images.is_empty());
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn bash_keeps_the_end_of_long_output_and_saves_all_of_it() {
        let cwd = temp_dir("bash-cap");
        let result = run(&cwd, bash("seq 1 20000; echo the-error >&2; exit 4"))
            .await
            .expect("bash");
        assert!(result.is_error);
        assert!(result.output.starts_with("18002\n18003\n"));
        let (shown, rest) = result
            .output
            .split_once("\n\n[Showing lines 18002-20001 of 20001. Full output: ")
            .expect("truncation notice");
        assert!(shown.ends_with("\n20000\nthe-error"));
        let (saved, status) = rest.split_once("]\n\n").expect("saved output path");
        assert_eq!(status, "Command exited with code 4");
        let saved = PathBuf::from(saved);
        assert_eq!(saved.parent(), Some(std::env::temp_dir().as_path()));
        let full = fs::read_to_string(&saved).expect("read saved output");
        assert_eq!(full.lines().count(), 20_001);
        assert!(full.starts_with("1\n2\n") && full.ends_with("\n20000\nthe-error\n"));
        let _ = fs::remove_file(saved);
        let _ = fs::remove_dir_all(cwd);
    }

    /// Runs `command`, collecting what it reports while it runs and when.
    async fn run_streaming(command: &str) -> (ToolResult, Vec<(String, std::time::Instant)>) {
        let mut reports = Vec::new();
        let result = execute_tool_streaming(
            Path::new("."),
            bash(command),
            &ToolRuntimeOptions::default(),
            &mut |output| reports.push((output.text.to_string(), std::time::Instant::now())),
        )
        .await
        .expect("bash");
        (result, reports)
    }

    #[tokio::test]
    async fn bash_reports_its_output_while_it_runs() {
        let (result, reports) = run_streaming("printf 'one\\n'; sleep 0.5; printf 'two\\n'").await;
        assert_eq!(result.output, "one\ntwo\n");
        let outputs = reports
            .iter()
            .map(|(output, _)| output.as_str())
            .collect::<Vec<_>>();
        // Starting, then everything printed so far each time more arrives.
        assert_eq!(outputs, ["", "one\n", "one\ntwo\n"]);
        // The first line was reported while the command still slept.
        let waited = reports[2].1.duration_since(reports[1].1);
        assert!(waited >= Duration::from_millis(400), "{waited:?}");
    }

    #[tokio::test]
    async fn bash_reports_are_throttled_and_end_with_all_the_output() {
        // 40 lines over about 400 ms: far fewer reports than lines.
        let (result, reports) =
            run_streaming("for i in $(seq 1 40); do echo line-$i; sleep 0.01; done").await;
        assert_eq!(result.output.lines().count(), 40);
        assert_eq!(reports[0].0, "");
        assert!(reports.len() < 15, "{} reports", reports.len());
        assert!(reports.len() > 2, "{} reports", reports.len());
        for pair in reports[1..].windows(2) {
            let apart = pair[1].1.duration_since(pair[0].1);
            // The last report flushes what is left when the command ends.
            if pair[1].0 != result.output {
                assert!(apart >= BASH_UPDATE_THROTTLE, "{apart:?} apart");
            }
            assert!(pair[1].0.starts_with(&pair[0].0), "a report lost output");
        }
        assert_eq!(reports.last().unwrap().0, result.output);
    }

    #[tokio::test]
    async fn bash_reports_stay_within_the_result_limits() {
        let (result, reports) = run_streaming("seq 1 20000").await;
        let (shown, rest) = result
            .output
            .split_once("\n\n[Showing lines 18001-20000 of 20000. Full output: ")
            .expect("truncation notice");
        // A report is the output's end, like the result without its notice.
        assert_eq!(reports.last().unwrap().0, shown);
        for (output, _) in &reports {
            assert!(output.lines().count() <= DEFAULT_MAX_LINES);
            assert!(output.len() <= DEFAULT_MAX_BYTES);
        }
        // A report says when it has left the beginning out.
        let mut truncated = Vec::new();
        execute_tool_streaming(
            Path::new("."),
            bash("seq 1 3; sleep 0.3; seq 1 20000"),
            &ToolRuntimeOptions::default(),
            &mut |output| truncated.push((output.text.starts_with("1\n"), output.truncated)),
        )
        .await
        .expect("bash");
        assert_eq!(truncated[..2], [(false, false), (true, false)]);
        assert_eq!(truncated.last(), Some(&(false, true)));
        let _ = fs::remove_file(rest.trim_end_matches(']'));
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_still_reports_that_it_starts() {
        let mut reports = Vec::new();
        let error = execute_tool_streaming(
            Path::new("/nonexistent/noel-tools-cwd"),
            bash("echo never"),
            &ToolRuntimeOptions::default(),
            &mut |output| reports.push(output.text.to_string()),
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("Working directory does not exist"));
        assert_eq!(reports, [""]);
        // Other tools report nothing before their result.
        let mut reports = Vec::new();
        execute_tool_streaming(
            Path::new("."),
            ToolRequest::Ls {
                path: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
            &mut |output| reports.push(output.text.to_string()),
        )
        .await
        .expect("ls");
        assert_eq!(reports, Vec::<String>::new());
    }

    #[tokio::test]
    async fn bash_returns_while_a_background_job_holds_its_output() {
        // The job inherits stdout; waiting for the pipe to close would take
        // as long as the job runs.
        let started = std::time::Instant::now();
        let result = run(Path::new("."), bash("sleep 30 & echo $!"))
            .await
            .expect("bash");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!result.is_error, "{}", result.output);
        let pid = result.output.trim().parse::<i32>().expect("job pid");
        let running = process_is_running(pid);
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        assert!(running, "a normal finish must not kill background jobs");
    }

    #[tokio::test]
    async fn bash_uses_the_configured_shell_and_command_prefix() {
        let options = |shell_path: &str| ToolRuntimeOptions {
            shell_path: Some(shell_path.to_string()),
            shell_command_prefix: Some("GREETING=from-prefix".to_string()),
            ..Default::default()
        };
        let command = "echo \"$GREETING ${BASH_VERSION:+bash}\"";
        let result = execute_tool(Path::new("."), bash(command), &options("/bin/sh"))
            .await
            .expect("bash with /bin/sh");
        // /bin/sh may be bash in sh mode or another shell; the prefix ran.
        assert!(
            result.output.starts_with("from-prefix"),
            "{}",
            result.output
        );

        let missing =
            execute_tool(Path::new("."), bash(command), &options("/nonexistent/sh")).await;
        assert_eq!(
            missing.expect_err("missing shell").to_string(),
            "Custom shell path not found: /nonexistent/sh"
        );
    }

    #[tokio::test]
    async fn bash_default_shell_is_a_plain_bash() {
        if !Path::new("/bin/bash").exists() {
            return;
        }
        let result = run(
            Path::new("."),
            bash("shopt -q login_shell && echo login || echo ${BASH_VERSION:+plain-bash}"),
        )
        .await
        .expect("bash");
        assert_eq!(result.output, "plain-bash\n");
    }

    /// A repository with ignored, hidden, binary, and linked files.
    fn search_tree(name: &str) -> PathBuf {
        let cwd = temp_dir(name);
        for directory in [".git", ".github", "src"] {
            fs::create_dir_all(cwd.join(directory)).expect("create dir");
        }
        for (path, content) in [
            (".gitignore", "ignored.txt\n".as_bytes()),
            (".rgignore", "only-rg-skips.txt\n".as_bytes()),
            (".fdignore", "only-fd-skips.txt\n".as_bytes()),
            (".github/ci.yml", b"needle workflow\n"),
            ("ignored.txt", b"needle ignored\n"),
            ("only-rg-skips.txt", b"needle rg\n"),
            ("only-fd-skips.txt", b"needle fd\n"),
            ("binary.dat", b"needle before\n\0needle after\n"),
            ("bom.txt", b"\xef\xbb\xbfneedle bom\r\n"),
            ("src/real.txt", b"needle real\n"),
        ] {
            fs::write(cwd.join(path), content).expect("write file");
        }
        std::os::unix::fs::symlink("src/real.txt", cwd.join("link.txt")).expect("symlink");
        std::os::unix::fs::symlink("missing", cwd.join("broken")).expect("symlink");
        cwd
    }

    #[tokio::test]
    async fn grep_searches_what_ripgrep_would() {
        let cwd = search_tree("grep");
        // Hidden files are searched; ignored and binary files and links are
        // not. A byte order mark does not hide the start of a line.
        let result = run(&cwd, grep("^needle", None, None)).await.expect("grep");
        assert_eq!(
            result.output,
            ".github/ci.yml:1: needle workflow\nbom.txt:1: needle bom\nonly-fd-skips.txt:1: needle fd\nsrc/real.txt:1: needle real"
        );
        // A file named as the search path is searched whatever it is.
        let binary = run(&cwd, grep("needle", Some("binary.dat"), None))
            .await
            .expect("grep binary");
        assert_eq!(
            binary.output,
            "binary.dat:1: needle before\nbinary.dat:2: \0needle after"
        );
        let linked = run(&cwd, grep("needle", Some("link.txt"), None))
            .await
            .expect("grep link");
        assert_eq!(linked.output, "link.txt:1: needle real");
        // A glob selects files, even ignored ones.
        let globbed = run(&cwd, grep("needle", None, Some("ign*.txt")))
            .await
            .expect("grep glob");
        assert_eq!(globbed.output, "ignored.txt:1: needle ignored");

        let invalid = run(&cwd, grep("needle(", None, None)).await;
        let message = invalid.expect_err("invalid regex").to_string();
        assert!(message.contains("regex parse error"), "{message}");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn find_lists_what_fd_would() {
        let cwd = search_tree("find");
        let result = run(&cwd, find("*.txt")).await.expect("find");
        assert_eq!(
            result.output,
            "bom.txt\nlink.txt\nonly-rg-skips.txt\nsrc/real.txt"
        );
        // Directories match too and end with a slash; a pattern with a
        // directory matches the whole path.
        assert_eq!(
            run(&cwd, find("SRC")).await.expect("find").output,
            "No files found matching pattern"
        );
        assert_eq!(run(&cwd, find("src")).await.expect("find").output, "src/");
        assert_eq!(
            run(&cwd, find(".github/*.yml")).await.expect("find").output,
            ".github/ci.yml"
        );
        let missing = run(
            &cwd,
            ToolRequest::Find {
                pattern: "*".to_string(),
                path: Some("nowhere".to_string()),
                limit: None,
            },
        )
        .await;
        assert_eq!(
            missing.expect_err("missing path").to_string(),
            format!("Path not found: {}", cwd.join("nowhere").display())
        );
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn ls_lists_dotfiles_and_follows_links() {
        let cwd = search_tree("ls");
        std::os::unix::fs::symlink("src", cwd.join("src-link")).expect("symlink");
        let result = run(&cwd, ls(None)).await.expect("ls");
        // A link to a directory is a directory; a broken link is left out.
        assert_eq!(
            result.output,
            ".fdignore\n.git/\n.github/\n.gitignore\n.rgignore\nbinary.dat\nbom.txt\nignored.txt\nlink.txt\nonly-fd-skips.txt\nonly-rg-skips.txt\nsrc/\nsrc-link/"
        );
        assert_eq!(
            run(&cwd, ls(Some("."))).await.expect("ls dot").output,
            result.output
        );
        let _ = fs::remove_dir_all(cwd);
    }

    #[test]
    fn oversized_extension_output_is_capped_and_saved() {
        assert_eq!(cap_tool_output("short\n".to_string()), "short\n");
        let long = (1..=2_500)
            .map(|line| format!("row {line}\n"))
            .collect::<String>();
        let capped = cap_tool_output(long.clone());
        let (shown, notice) = capped
            .split_once("\n\n[Showing lines 1-2000 of 2500. Full output: ")
            .expect("truncation notice");
        assert!(shown.starts_with("row 1\n") && shown.ends_with("\nrow 2000"));
        let saved = PathBuf::from(notice.strip_suffix(']').expect("notice end"));
        assert_eq!(fs::read_to_string(&saved).expect("saved output"), long);
        let _ = fs::remove_file(saved);
    }

    #[test]
    fn builtin_tools_cover_upstream_ts_names() {
        let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../tests/fixtures/ts-parity/local-tools.json"
        ))
        .expect("parse local tools fixture");
        let upstream_names = fixture["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_string())
            .collect::<Vec<_>>();
        let rust_tools = builtin_tool_definitions();
        let rust_names = rust_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        // Rust intentionally adds tools beyond the upstream set (see
        // docs/rust-rewrite/non-parity-register.md); upstream names must all
        // be covered, but equality is not required. What the tools do and
        // what they tell the model is compared with upstream in
        // crates/noel-core/src/tool_parity_tests.rs (tool-behavior.json).
        // Upstream's `powershell` tool only runs on Windows, so Rust omits it
        // ("Windows-Only PowerShell Tool" in non-parity-register.md).
        const WINDOWS_ONLY_UPSTREAM_TOOL: &str = "powershell";
        for name in &upstream_names {
            if name == WINDOWS_ONLY_UPSTREAM_TOOL {
                assert!(
                    !rust_names.contains(name),
                    "{name} is now ported; drop its non-parity exception"
                );
                continue;
            }
            assert!(rust_names.contains(name), "missing upstream tool {name}");
        }

        let read_only = rust_tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.read_only))
            .collect::<BTreeMap<_, _>>();
        assert!(read_only["read"]);
        assert!(read_only["grep"]);
        assert!(read_only["find"]);
        assert!(read_only["ls"]);
        assert!(!read_only["bash"]);
        assert!(!read_only["edit"]);
        assert!(!read_only["write"]);
        assert!(read_only["todo"]);
    }

    #[tokio::test]
    async fn read_write_and_edit_tool_round_trip() {
        let cwd = std::env::temp_dir().join(format!("noel-tools-test-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");

        execute_tool(
            &cwd,
            ToolRequest::Write {
                path: "a.txt".to_string(),
                content: "hello world".to_string(),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("write");
        execute_tool(
            &cwd,
            ToolRequest::Edit {
                path: "a.txt".to_string(),
                edits: vec![TextEdit {
                    find: "world".to_string(),
                    replace: "rust".to_string(),
                }],
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("edit");
        let result = execute_tool(
            &cwd,
            ToolRequest::Read {
                path: "a.txt".to_string(),
                offset: None,
                limit: None,
            },
            &ToolRuntimeOptions::default(),
        )
        .await
        .expect("read");

        assert_eq!(result.output, "hello rust");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn write_edit_and_read_follow_paths_outside_cwd() {
        // Like upstream, a path is not confined to the working directory.
        let root = temp_dir("outside");
        let cwd = root.join("project");
        fs::create_dir_all(&cwd).expect("create cwd");

        run(
            &cwd,
            ToolRequest::Write {
                path: "../sibling/note.txt".to_string(),
                content: "hello world".to_string(),
            },
        )
        .await
        .expect("write outside cwd");
        let note = root.join("sibling/note.txt");
        assert_eq!(
            fs::read_to_string(&note).expect("written file"),
            "hello world"
        );

        run(
            &cwd,
            ToolRequest::Edit {
                path: note.display().to_string(),
                edits: text_edits(&[("world", "rust")]),
            },
        )
        .await
        .expect("edit by absolute path");
        let result = run(&cwd, read("../sibling/note.txt", None, None))
            .await
            .expect("read outside cwd");
        assert_eq!(result.output, "hello rust");
        let _ = fs::remove_dir_all(root);
    }

    fn text_edits(pairs: &[(&str, &str)]) -> Vec<TextEdit> {
        pairs
            .iter()
            .map(|(find, replace)| TextEdit {
                find: find.to_string(),
                replace: replace.to_string(),
            })
            .collect()
    }

    async fn edit_file(cwd: &Path, edits: &[(&str, &str)]) -> Result<ToolResult, ToolError> {
        execute_tool(
            cwd,
            ToolRequest::Edit {
                path: "a.txt".to_string(),
                edits: text_edits(edits),
            },
            &ToolRuntimeOptions::default(),
        )
        .await
    }

    #[tokio::test]
    async fn edits_apply_all_together_or_not_at_all() {
        let cwd =
            std::env::temp_dir().join(format!("noel-tools-edit-atomic-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let file = cwd.join("a.txt");
        fs::write(&file, "alpha beta gamma\n").expect("write file");

        edit_file(&cwd, &[("gamma", "G"), ("alpha", "A")])
            .await
            .expect("edit both");
        assert_eq!(fs::read_to_string(&file).unwrap(), "A beta G\n");

        // A later edit that fails leaves the earlier ones unapplied.
        let error = edit_file(&cwd, &[("beta", "B"), ("missing", "X")])
            .await
            .expect_err("missing text");
        assert!(error.to_string().contains("edits[1]"), "{error}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "A beta G\n");
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn edits_require_unique_non_overlapping_old_text() {
        let cwd =
            std::env::temp_dir().join(format!("noel-tools-edit-unique-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let file = cwd.join("a.txt");
        fs::write(&file, "x = 1;\nx = 1;\nabcdef\n").expect("write file");

        let error = edit_file(&cwd, &[("x = 1;", "x = 2;")])
            .await
            .expect_err("duplicate text");
        assert!(error.to_string().contains("Found 2 occurrences"), "{error}");
        let error = edit_file(&cwd, &[("abc", "1"), ("cde", "2")])
            .await
            .expect_err("overlapping edits");
        assert!(
            error.to_string().contains("edits[0] and edits[1] overlap"),
            "{error}"
        );
        let error = edit_file(&cwd, &[("", "y")]).await.expect_err("empty");
        assert!(error.to_string().contains("must not be empty"), "{error}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "x = 1;\nx = 1;\nabcdef\n"
        );
        let _ = fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn edits_delete_with_empty_new_text_and_keep_bom_and_crlf() {
        let cwd = std::env::temp_dir().join(format!("noel-tools-edit-crlf-{}", std::process::id()));
        fs::create_dir_all(&cwd).expect("create temp dir");
        let file = cwd.join("a.txt");
        fs::write(&file, "\u{feff}one\r\ntwo\r\nthree\r\n").expect("write file");

        // Models send LF; the edit matches and writes CRLF.
        edit_file(&cwd, &[("one\ntwo\n", ""), ("three", "3\nthree")])
            .await
            .expect("edit crlf file");
        assert_eq!(fs::read_to_string(&file).unwrap(), "\u{feff}3\r\nthree\r\n");
        let _ = fs::remove_dir_all(cwd);
    }
}
