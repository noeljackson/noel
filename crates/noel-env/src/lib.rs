//! noel's workspaces: where tools read, write and run commands.
//!
//! [`ExecutionEnv`] mirrors upstream Durable's `ExecutionEnv`
//! (`packages/durable/src/env/index.ts`): a [`FileSystem`] plus a [`Shell`].
//! Operations return `Result` and never panic on expected failures; the error
//! codes ([`FileErrorCode`], [`ExecutionErrorCode`]) are upstream's. Every
//! operation takes a [`Context`] whose abort signal it checks where
//! upstream's `NodeExecutionEnv` does.
//!
//! [`LocalEnv`] runs on this machine and behaves as `NodeExecutionEnv`
//! (`packages/env/docs/semantics.md`). Remote workspaces speak the `pi-env`
//! protocol, whose frames are in [`frame`]; the daemon is the
//! `noel-env-daemon` crate (`noel-env serve`), and the client comes later in
//! this crate.

mod context;
mod decode;
mod error;
mod exec;
pub mod frame;
mod line_scan;
mod local;
mod path;
mod process;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use context::{AbortController, AbortSignal, Context};
pub use decode::{RangeDecoder, StreamDecoder};
pub use error::{ExecutionError, ExecutionErrorCode, FileError, FileErrorCode};
pub use line_scan::LineScanner;
pub use local::{LocalEnv, LocalEnvOptions, LOCAL_ENV_ID};
pub use path::resolve_local_path;
pub use process::{kill_running_tool_groups, spawn_tool_process, ToolProcessGroup};

/// The largest integer JavaScript represents exactly. Offsets, lengths and
/// counts above it are `invalid`, as upstream's `Number.isSafeInteger`
/// checks make them.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

pub type FileResult<T> = Result<T, FileError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    /// Whether a newline ended the line; only the last line can lack one.
    pub terminated: bool,
}

#[async_trait]
pub trait TextLineReader: Send {
    /// The next line, `None` at the end.
    async fn read_line(&mut self, cx: &Context) -> FileResult<Option<TextLine>>;
    async fn close(&mut self, cx: &Context);
}

/// Lines `[start_line, end_line)`, 0-based, where line `k` starts after the
/// `k`-th newline byte; `end_line` absent: to the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineRange {
    pub start_line: u64,
    pub end_line: Option<u64>,
}

/// Where lines of a file are, as [`BinaryReader::scan_lines`] found them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineScan {
    /// Newline bytes in the whole file; it has `newlines + 1` lines.
    pub newlines: u64,
    /// Byte range of the selected lines, without the newline that ends the
    /// last. A selection past the last line is empty at the end of the file.
    pub start: u64,
    pub end: u64,
    /// Where the first selected line ends: its newline, or the end.
    pub first_line_end: u64,
    /// Where the last selected line starts.
    pub last_line_start: u64,
    /// UTF-8 byte length of the decoded selection and of its first line.
    pub selected_bytes: u64,
    pub first_line_bytes: u64,
}

/// Positional reads from one opened regular file; all calls see the same
/// file even if its path is renamed.
#[async_trait]
pub trait BinaryReader: Send {
    /// Metadata of the opened file, not of whatever its path names now.
    async fn info(&mut self, cx: &Context) -> FileResult<FileInfo>;
    /// Up to `length` bytes at `offset`; fewer only at the end of the file.
    async fn read(&mut self, offset: u64, length: u64, cx: &Context) -> FileResult<Vec<u8>>;
    /// One pass over the file that locates `range`. Decoded sizes are those
    /// of decoding the whole file, so a leading byte-order mark is not
    /// counted.
    async fn scan_lines(&mut self, range: LineRange, cx: &Context) -> FileResult<LineScan>;
    async fn close(&mut self, cx: &Context);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirPage {
    pub entries: Vec<FileInfo>,
    pub done: bool,
}

/// Pages of one directory's entries.
#[async_trait]
pub trait DirReader: Send {
    /// Up to `max_entries` entries in the order the file system returns
    /// them, continuing where the last call stopped. `done` marks the end.
    /// Entries removed meanwhile and entries of other kinds are skipped.
    async fn next(&mut self, max_entries: u64, cx: &Context) -> FileResult<DirPage>;
    async fn close(&mut self, cx: &Context);
}

/// Entries below a watched directory that are neither watched nor reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchExclude {
    /// Names starting with `.`.
    pub hidden: bool,
    pub names: Vec<String>,
}

/// A file or directory to watch. It may be missing; creating it is a change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchTarget {
    pub path: String,
    /// Everything below a directory, not only its entries.
    pub recursive: bool,
    pub exclude: Option<WatchExclude>,
}

/// What changed (upstream `WatchChange`).
#[derive(Debug, Clone, PartialEq)]
pub enum WatchChange {
    /// Something at or below each path may have changed.
    Paths(Vec<String>),
    /// Coverage was uncertain for a while; rescan everything watched.
    Overflow,
    /// The watcher stopped; no calls follow.
    Error(FileError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Native,
    Polling,
}

#[async_trait]
pub trait FileWatcher: Send {
    fn mode(&self) -> WatchMode;
    /// Stops watching; no change is reported after this. Idempotent.
    async fn close(&mut self, cx: &Context);
}

pub type WatchCallback = Arc<dyn Fn(WatchChange) + Send + Sync>;

/// What [`FileSystem::list_files`] lists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListFilesOptions {
    /// Include names starting with `.`; default true.
    pub hidden: Option<bool>,
    /// Inside a git work tree, skip what git ignores; default true.
    pub gitignore: Option<bool>,
    /// Names of entries to skip with everything below them.
    pub exclude: Vec<String>,
    /// Most entries to list; default 50 000.
    pub max_entries: Option<u64>,
    /// Most bytes of the listing, each entry's path plus two; default 4 MiB.
    pub max_bytes: Option<u64>,
    /// Time after which the listing stops; default 10 s.
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListEntry {
    /// Relative to the listed directory, with `/` separators.
    pub path: String,
    pub kind: FileKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListing {
    pub entries: Vec<FileListEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BinaryReaderOptions {
    /// Refuse a symbolic link as the final path component with `invalid`.
    pub no_follow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateDirOptions {
    /// Create missing parents, and accept an existing directory; default
    /// true.
    pub recursive: bool,
}

impl Default for CreateDirOptions {
    fn default() -> Self {
        Self { recursive: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemoveOptions {
    pub recursive: bool,
    /// A missing path is not a failure.
    pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TempFileOptions {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

/// A portable file system (upstream `FileSystem`). Paths are strings in the
/// environment's own form; relative ones are resolved against
/// [`FileSystem::cwd`].
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// The file namespace: equal ids see the same files at the same paths,
    /// whatever their `cwd`. Every [`LocalEnv`] has [`LOCAL_ENV_ID`].
    fn id(&self) -> &str;
    fn cwd(&self) -> &str;
    async fn absolute_path(&self, path: &str, cx: &Context) -> FileResult<String>;
    async fn join_path(&self, parts: &[&str], cx: &Context) -> FileResult<String>;
    async fn read_text_file(&self, path: &str, cx: &Context) -> FileResult<String>;
    async fn open_text_line_reader(
        &self,
        path: &str,
        cx: &Context,
    ) -> FileResult<Box<dyn TextLineReader>>;
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<u64>,
        cx: &Context,
    ) -> FileResult<Vec<String>>;
    async fn read_binary_file(&self, path: &str, cx: &Context) -> FileResult<Vec<u8>>;
    /// Opens a regular file for bounded positional reads. A directory fails
    /// with `is_directory`, other non-regular files with `invalid`.
    async fn open_binary_reader(
        &self,
        path: &str,
        options: BinaryReaderOptions,
        cx: &Context,
    ) -> FileResult<Box<dyn BinaryReader>>;
    /// Writes `content`, creating missing parent directories.
    async fn write_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()>;
    async fn append_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()>;
    /// Truncates or extends a file to exactly `size` bytes.
    async fn truncate_file(&self, path: &str, size: u64, cx: &Context) -> FileResult<()>;
    /// Flushes a file's contents and the metadata needed to read them.
    async fn flush_file(&self, path: &str, cx: &Context) -> FileResult<()>;
    async fn rename_file(&self, source: &str, destination: &str, cx: &Context) -> FileResult<()>;
    /// Metadata of `path` itself; a symbolic link is not followed.
    async fn file_info(&self, path: &str, cx: &Context) -> FileResult<FileInfo>;
    async fn list_dir(&self, path: &str, cx: &Context) -> FileResult<Vec<FileInfo>>;
    /// Optional: the files, directories and links below a directory, at any
    /// depth. Fails with `not_supported` where the environment cannot list.
    async fn list_files(
        &self,
        path: &str,
        options: &ListFilesOptions,
        cx: &Context,
    ) -> FileResult<FileListing> {
        let _ = (options, cx);
        Err(FileError::new(
            FileErrorCode::NotSupported,
            "listFiles is not supported by this environment",
            Some(path),
        ))
    }
    async fn open_dir_reader(&self, path: &str, cx: &Context) -> FileResult<Box<dyn DirReader>>;
    /// Reports changes to files and directories.
    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        cx: &Context,
    ) -> FileResult<Box<dyn FileWatcher>>;
    async fn canonical_path(&self, path: &str, cx: &Context) -> FileResult<String>;
    async fn exists(&self, path: &str, cx: &Context) -> FileResult<bool>;
    async fn create_dir(
        &self,
        path: &str,
        options: CreateDirOptions,
        cx: &Context,
    ) -> FileResult<()>;
    async fn remove(&self, path: &str, options: RemoveOptions, cx: &Context) -> FileResult<()>;
    async fn create_temp_dir(&self, prefix: Option<&str>, cx: &Context) -> FileResult<String>;
    async fn create_temp_file(&self, options: &TempFileOptions, cx: &Context)
        -> FileResult<String>;
}

/// A command: a script run through the environment's shell, or a program
/// and its arguments run directly, without a shell, so they reach it
/// unparsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Script(String),
    Argv(Vec<String>),
}

impl From<&str> for ShellCommand {
    fn from(script: &str) -> Self {
        Self::Script(script.to_string())
    }
}

impl From<String> for ShellCommand {
    fn from(script: String) -> Self {
        Self::Script(script)
    }
}

impl From<Vec<String>> for ShellCommand {
    fn from(argv: Vec<String>) -> Self {
        Self::Argv(argv)
    }
}

impl From<&[&str]> for ShellCommand {
    fn from(argv: &[&str]) -> Self {
        Self::Argv(argv.iter().map(|arg| arg.to_string()).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Output an environment omitted under [`ExecOptions::window`], measured on
/// the decoded text: every U+FFFD counts three bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSkip {
    pub bytes: u64,
    pub newlines: u64,
    pub ends_with_newline: bool,
}

/// One chunk of a command's output, in arrival order.
#[derive(Debug, Clone, Copy)]
pub struct OutputChunk<'a> {
    /// The decoded text: each stream is decoded on its own, so a character
    /// split across chunks survives. It is empty when the chunk's bytes end
    /// inside a character.
    pub text: &'a str,
    pub stream: OutputStream,
    /// Output omitted right before this chunk, only with a window.
    pub skipped: Option<OutputSkip>,
    /// The bytes the chunk came from, where the environment has them
    /// (`LocalEnv` does). A noel addition, for callers that keep raw output.
    pub raw: Option<&'a [u8]>,
}

/// Spill the complete output to a temporary file once it exceeds either
/// threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpillOptions {
    pub after_bytes: u64,
    /// Complete or partial lines.
    pub after_lines: u64,
}

/// The tail of the combined output a caller keeps, and how often it samples
/// it. An environment may omit output outside it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputWindow {
    pub max_bytes: u64,
    pub max_lines: u64,
    pub min_interval: Duration,
    pub bytes_per_second: f64,
}

/// Receives every chunk; an `Err` stops the command, which then fails with
/// `callback_error` and the message.
pub type OutputCallback<'a> = &'a mut (dyn FnMut(OutputChunk<'_>) -> Result<(), String> + Send);

pub struct ExecOptions<'a> {
    /// Relative to the environment's `cwd`; default that `cwd`.
    pub cwd: Option<String>,
    /// Variables on top of the inherited environment. A noel addition:
    /// `None` removes an inherited variable.
    pub env: Vec<(String, Option<String>)>,
    /// Start from the environment's own variables; default true. Without
    /// it the command gets only `env`.
    pub inherit_env: bool,
    /// Kill the command's processes after this long; must be above zero.
    pub timeout: Option<Duration>,
    pub on_output: Option<OutputCallback<'a>>,
    pub spill: Option<SpillOptions>,
    pub window: Option<OutputWindow>,
    /// A noel addition: stderr goes into stdout's pipe, so output keeps the
    /// order the command wrote it in; every chunk is `Stdout`. An
    /// environment that cannot share a pipe reports both streams in
    /// arrival order.
    pub merge_stderr: bool,
}

impl Default for ExecOptions<'_> {
    fn default() -> Self {
        Self {
            cwd: None,
            env: Vec::new(),
            inherit_env: true,
            timeout: None,
            on_output: None,
            spill: None,
            window: None,
            merge_stderr: false,
        }
    }
}

impl std::fmt::Debug for ExecOptions<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecOptions")
            .field("cwd", &self.cwd)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("timeout", &self.timeout)
            .field("on_output", &self.on_output.is_some())
            .field("spill", &self.spill)
            .field("window", &self.window)
            .field("merge_stderr", &self.merge_stderr)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// The exit code; `128 + signal` for a process killed by a signal.
    pub exit_code: i32,
    /// Temporary file holding the complete raw output, when it spilled.
    pub spill_path: Option<String>,
}

/// Runs commands (upstream `Shell`).
#[async_trait]
pub trait Shell: Send + Sync {
    /// Runs `command`. Aborting the context, a timeout, or dropping the
    /// returned future kills only this command's processes.
    async fn exec(
        &self,
        command: ShellCommand,
        options: ExecOptions<'_>,
        cx: &Context,
    ) -> Result<ExecResult, ExecutionError>;
    /// Kills every command this environment still runs; for its owner's
    /// shutdown, never for one request. Upstream's `FileSystem` and `Shell`
    /// both declare `cleanup`, which `ExecutionEnv` merges into this one.
    async fn cleanup(&self, cx: &Context);
}

/// A workspace: a file system and a shell that see the same files.
pub trait ExecutionEnv: FileSystem + Shell {}

impl<T: FileSystem + Shell + ?Sized> ExecutionEnv for T {}

/// Whether `env` sees this process's own files, so a tool may also reach
/// them through local libraries.
pub fn is_local(env: &dyn ExecutionEnv) -> bool {
    env.id() == LOCAL_ENV_ID
}
