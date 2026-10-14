//! `RemoteEnv`: an [`ExecutionEnv`](crate::ExecutionEnv) on another machine, reached through a pi-env daemon over a
//! [`Connection`] (upstream `packages/env/src/remote-env.ts`, `list-files.ts` and `errors.ts`). Results match
//! upstream's `NodeExecutionEnv` running on that machine, and so `LocalEnv`: the daemon performs the system calls,
//! and this client applies Node's path, error and result rules (`packages/env/docs/semantics.md`). Only error
//! messages may differ.
//!
//! Ported from upstream pi's `packages/env/src/remote-env.ts`, `list-files.ts` and `errors.ts`
//! (<https://github.com/earendil-works/pi>, commit `1692bba`), Copyright (c) 2025 Mario Zechner, MIT License (see
//! LICENSE at the repository root).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::connection::{
    node_platform, Connection, Json, PendingRequest, RemoteError, RemoteInfo, RequestOptions,
};
use crate::path::{basename, join, resolve, resolve_local_path};
use crate::remote_watch::{RemoteWatchOptions, RemoteWatcher};
use crate::{
    BinaryReader, BinaryReaderOptions, Context, CreateDirOptions, DirPage, DirReader, ExecOptions,
    ExecResult, ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileKind,
    FileListEntry, FileListing, FileResult, FileSystem, FileWatcher, LineRange, LineScan,
    ListFilesOptions, OutputChunk, OutputSkip, OutputStream, RemoveOptions, Shell, ShellCommand,
    StreamDecoder, TempFileOptions, TextLine, TextLineReader, WatchCallback, WatchTarget,
    MAX_SAFE_INTEGER,
};

/// Largest timeout, a 32-bit millisecond timer as Node's.
const MAX_TIMEOUT: Duration = Duration::from_millis(2_147_483_647);
/// Bytes per `pread` request of a binary reader; several are in flight at once.
const READ_CHUNK: u64 = 256 * 1024;
const READ_DEPTH: usize = 8;
/// Node's `writeFile` writes at most this much per call and checks for an abort before each.
const WRITE_CHUNK: usize = 512 * 1024;
const WRITE_DEPTH: usize = 8;
/// Node's `readFile`: chunk of known-size reads, chunk of unknown-size reads, and the largest file it reads.
const READ_FILE_CHUNK: u64 = 512 * 1024;
const READ_FILE_UNKNOWN_CHUNK: u64 = 64 * 1024;
const READ_FILE_MAX: u64 = (1 << 31) - 1;
/// `NodeTextLineReader`'s read size.
const LINE_CHUNK: u64 = 64 * 1024;

/// A failure on its way to becoming a [`FileError`]: one the daemon reported, or one decided here.
enum Failure {
    Remote(RemoteError),
    File(FileError),
}

impl From<RemoteError> for Failure {
    fn from(error: RemoteError) -> Self {
        Self::Remote(error)
    }
}

impl From<FileError> for Failure {
    fn from(error: FileError) -> Self {
        Self::File(error)
    }
}

/// `NodeExecutionEnv`'s mapping of Node error codes to file error codes (upstream `toFileError`).
fn to_file_error(failure: Failure, fallback_path: Option<&str>) -> FileError {
    match failure {
        Failure::File(error) => error,
        Failure::Remote(error) => remote_file_error(error, fallback_path),
    }
}

/// A daemon's error as a [`FileError`], coded as `NodeExecutionEnv` codes Node's errors.
pub(crate) fn remote_file_error(error: RemoteError, fallback_path: Option<&str>) -> FileError {
    let code = match error.code.as_str() {
        "aborted" => FileErrorCode::Aborted,
        "ENOENT" => FileErrorCode::NotFound,
        "EACCES" | "EPERM" => FileErrorCode::PermissionDenied,
        "ENOTDIR" => FileErrorCode::NotDirectory,
        "EISDIR" => FileErrorCode::IsDirectory,
        "EINVAL" | "SYMLINK" | "NOT_REGULAR" => FileErrorCode::Invalid,
        _ => FileErrorCode::Unknown,
    };
    let path = error.path.clone().or(fallback_path.map(str::to_string));
    FileError::new(code, error.message, path.as_deref())
}

fn check_abort(cx: &Context, path: Option<&str>) -> FileResult<()> {
    if cx.is_aborted() {
        Err(FileError::aborted(path))
    } else {
        Ok(())
    }
}

fn operation_aborted(path: &str) -> Failure {
    Failure::File(FileError::new(
        FileErrorCode::Aborted,
        "The operation was aborted",
        Some(path),
    ))
}

fn malformed(what: &str) -> Failure {
    Failure::Remote(RemoteError::with(
        "unknown",
        &format!("Malformed reply from pi-env: {what}"),
        false,
    ))
}

fn object(value: Value) -> Json {
    match value {
        Value::Object(object) => object,
        _ => Json::new(),
    }
}

/// A number as JavaScript sends it: integral values without a fraction.
fn js_number(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER as f64 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

fn millis(duration: Duration) -> Value {
    if duration.subsec_nanos().is_multiple_of(1_000_000) {
        Value::from(duration.as_millis() as u64)
    } else {
        js_number(duration.as_secs_f64() * 1000.0)
    }
}

/// An info record of the daemon as a [`FileInfo`] (upstream `toInfo`): other kinds of files are `invalid`.
fn to_info(path: &str, remote: &Value) -> FileResult<FileInfo> {
    let kind = match remote["kind"].as_str() {
        Some("file") => FileKind::File,
        Some("directory") => FileKind::Directory,
        Some("symlink") => FileKind::Symlink,
        _ => {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Unsupported file type",
                Some(path),
            ))
        }
    };
    let number = |field: &str| remote[field].as_f64().unwrap_or(0.0);
    Ok(FileInfo {
        name: basename(path).to_string(),
        path: path.to_string(),
        kind,
        size: remote["size"].as_u64().unwrap_or(0),
        mtime_ms: number("mtimeSec") * 1000.0 + number("mtimeNsec") / 1e6,
    })
}

/// A daemon handle: valid only in the session that opened it.
struct Handle {
    connection: Connection,
    id: u64,
    session: u32,
    path: String,
}

impl Handle {
    fn from_reply(
        connection: &Connection,
        json: &Json,
        session: u32,
        path: &str,
    ) -> Result<Self, Failure> {
        let id = json
            .get("handle")
            .and_then(Value::as_u64)
            .ok_or_else(|| malformed("no handle"))?;
        Ok(Self {
            connection: connection.clone(),
            id,
            session,
            path: path.to_string(),
        })
    }

    fn json(&self, mut json: Json) -> Json {
        json.insert("handle".into(), Value::from(self.id));
        json
    }

    async fn request(
        &self,
        op: &str,
        json: Json,
        cx: Option<&Context>,
    ) -> Result<crate::connection::Reply, RemoteError> {
        self.connection
            .request(
                op,
                self.json(json),
                RequestOptions {
                    cx,
                    session: Some(self.session),
                    cancel_on_drop: cx.is_some(),
                    ..RequestOptions::default()
                },
            )
            .await
    }

    /// Sends a request without waiting, so several are in flight.
    fn start(&self, op: &str, json: Json, payload: &[u8]) -> Result<PendingRequest, RemoteError> {
        self.connection
            .start_request(self.session, op, self.json(json), payload)
    }

    fn start_pread(&self, offset: Option<u64>, length: u64) -> Result<PendingRequest, RemoteError> {
        let mut json = Json::new();
        if let Some(offset) = offset {
            json.insert("offset".into(), Value::from(offset));
        }
        json.insert("length".into(), Value::from(length));
        self.start("pread", json, &[])
    }

    async fn pread(&self, offset: Option<u64>, length: u64) -> Result<Vec<u8>, RemoteError> {
        Ok(self
            .start_pread(offset, length)?
            .wait(None, None)
            .await?
            .payload)
    }

    async fn close(&self) {
        let _ = self.request("close", Json::new(), None).await;
    }
}

/// Reads `length` bytes from `offset` as consecutive reads of at most `chunk` bytes, several in flight. Ends early
/// at the end of the file; a short read continues from where it ended, as sequential reads would. `aborted` is
/// checked after each read; `None` means aborted.
async fn read_range(
    handle: &Handle,
    offset: u64,
    length: u64,
    chunk: u64,
    aborted: impl Fn() -> bool,
) -> Result<Option<Vec<u8>>, RemoteError> {
    let end = offset + length;
    let mut bytes = Vec::new();
    let mut next = offset;
    let mut in_flight: VecDeque<(u64, PendingRequest)> = VecDeque::new();
    while (bytes.len() as u64) < length {
        while in_flight.len() < READ_DEPTH && next < end {
            let size = (end - next).min(chunk);
            in_flight.push_back((size, handle.start_pread(Some(next), size)?));
            next += size;
        }
        let Some((size, head)) = in_flight.pop_front() else {
            break;
        };
        let read = head.wait(None, None).await?.payload;
        if aborted() {
            return Ok(None);
        }
        if read.is_empty() {
            break;
        }
        let short = (read.len() as u64) < size;
        bytes.extend_from_slice(&read);
        if short {
            // Reads already in flight assumed a full read; continue right after this one.
            in_flight.clear();
            next = offset + bytes.len() as u64;
        }
    }
    Ok(Some(bytes))
}

fn closed(what: &str, path: &str) -> FileError {
    FileError::new(
        FileErrorCode::Invalid,
        format!("{what} is closed"),
        Some(path),
    )
}

struct RemoteBinaryReader {
    handle: Handle,
    closed: bool,
}

impl RemoteBinaryReader {
    fn usable(&self, cx: &Context) -> FileResult<()> {
        check_abort(cx, Some(&self.handle.path))?;
        if self.closed {
            return Err(closed("Binary reader", &self.handle.path));
        }
        Ok(())
    }
}

#[async_trait]
impl BinaryReader for RemoteBinaryReader {
    async fn info(&mut self, cx: &Context) -> FileResult<FileInfo> {
        self.usable(cx)?;
        let path = &self.handle.path;
        match self.handle.request("fstat", Json::new(), None).await {
            Ok(reply) => to_info(path, &Value::Object(reply.json)),
            Err(error) => Err(to_file_error(error.into(), Some(path))),
        }
    }

    async fn read(&mut self, offset: u64, length: u64, cx: &Context) -> FileResult<Vec<u8>> {
        self.usable(cx)?;
        let path = &self.handle.path;
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Offset and length must be non-negative safe integers",
                Some(path),
            ));
        }
        match read_range(&self.handle, offset, length, READ_CHUNK, || cx.is_aborted()).await {
            Ok(Some(bytes)) => Ok(bytes),
            Ok(None) => Err(FileError::aborted(Some(path))),
            Err(error) => Err(to_file_error(error.into(), Some(path))),
        }
    }

    async fn scan_lines(&mut self, range: LineRange, cx: &Context) -> FileResult<LineScan> {
        self.usable(cx)?;
        let path = &self.handle.path;
        let LineRange {
            start_line,
            end_line,
        } = range;
        if start_line > MAX_SAFE_INTEGER
            || end_line.is_some_and(|end| end > MAX_SAFE_INTEGER || end <= start_line)
        {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Invalid line range",
                Some(path),
            ));
        }
        let mut json = Json::new();
        json.insert("startLine".into(), Value::from(start_line));
        if let Some(end_line) = end_line {
            json.insert("endLine".into(), Value::from(end_line));
        }
        let scanned = self.handle.request("scanLines", json, Some(cx)).await;
        match scanned {
            Ok(reply) => serde_json::from_value(Value::Object(reply.json))
                .map_err(|_| to_file_error(malformed("scanLines"), Some(path))),
            Err(error) => Err(to_file_error(error.into(), Some(path))),
        }
    }

    async fn close(&mut self, _cx: &Context) {
        if !self.closed {
            self.closed = true;
            self.handle.close().await;
        }
    }
}

struct RemoteDirReader {
    handle: Handle,
    done: bool,
    closed: bool,
}

#[async_trait]
impl DirReader for RemoteDirReader {
    async fn next(&mut self, max_entries: u64, cx: &Context) -> FileResult<DirPage> {
        let path = self.handle.path.clone();
        check_abort(cx, Some(&path))?;
        if self.closed {
            return Err(closed("Directory reader", &path));
        }
        if max_entries == 0 || max_entries > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                Some(&path),
            ));
        }
        let mut entries = Vec::new();
        // Like `NodeDirReader`, count only entries that are part of the listing.
        while !self.done && (entries.len() as u64) < max_entries {
            let max = max_entries - entries.len() as u64;
            let reply = self
                .handle
                .request("readdir", object(json!({ "max": max })), None)
                .await
                .map_err(|error| to_file_error(error.into(), Some(&path)))?;
            check_abort(cx, Some(&path))?;
            let page = reply.json.get("entries").and_then(Value::as_array);
            for entry in page.into_iter().flatten() {
                let name = entry["name"].as_str().unwrap_or_default();
                let entry_path = resolve(&[path.as_str(), name]);
                if let Some(error) = entry.get("error") {
                    // Removed between enumeration and lstat: not part of the listing any more.
                    if error["code"] == "ENOENT" {
                        continue;
                    }
                    let error = RemoteError::new(object(error.clone()));
                    return Err(to_file_error(error.into(), Some(&entry_path)));
                }
                if let Ok(info) = to_info(&entry_path, &entry["info"]) {
                    entries.push(info);
                }
            }
            self.done = reply.json.get("done") == Some(&Value::Bool(true));
        }
        Ok(DirPage {
            entries,
            done: self.done,
        })
    }

    async fn close(&mut self, _cx: &Context) {
        if !self.closed {
            self.closed = true;
            self.handle.close().await;
        }
    }
}

/// `NodeTextLineReader`: positional reads of a file opened like `open(path, "r")`.
struct RemoteTextLineReader {
    handle: Handle,
    decoder: StreamDecoder,
    offset: u64,
    buffered: String,
    /// How much of `buffered` was returned already.
    consumed: usize,
    ended: bool,
    closed: bool,
}

#[async_trait]
impl TextLineReader for RemoteTextLineReader {
    async fn read_line(&mut self, cx: &Context) -> FileResult<Option<TextLine>> {
        let path = self.handle.path.clone();
        check_abort(cx, Some(&path))?;
        if self.closed {
            return Err(closed("Text line reader", &path));
        }
        loop {
            if let Some(newline) = self.buffered[self.consumed..].find('\n') {
                let end = self.consumed + newline;
                let text = self.buffered[self.consumed..end].to_string();
                self.consumed = end + 1;
                if self.consumed > LINE_CHUNK as usize {
                    self.buffered.drain(..self.consumed);
                    self.consumed = 0;
                }
                return Ok(Some(TextLine {
                    text,
                    terminated: true,
                }));
            }
            if self.ended {
                if self.consumed == self.buffered.len() {
                    return Ok(None);
                }
                let text = self.buffered[self.consumed..].to_string();
                self.buffered.clear();
                self.consumed = 0;
                return Ok(Some(TextLine {
                    text,
                    terminated: false,
                }));
            }
            let bytes = self
                .handle
                .pread(Some(self.offset), LINE_CHUNK)
                .await
                .map_err(|error| to_file_error(error.into(), Some(&path)))?;
            check_abort(cx, Some(&path))?;
            self.offset += bytes.len() as u64;
            if bytes.is_empty() {
                let rest = self.decoder.finish();
                self.buffered.push_str(&rest);
                self.ended = true;
            } else {
                let text = self.decoder.decode(&bytes);
                self.buffered.push_str(&text);
            }
        }
    }

    async fn close(&mut self, _cx: &Context) {
        if !self.closed {
            self.closed = true;
            self.buffered.clear();
            self.consumed = 0;
            self.handle.close().await;
        }
    }
}

/// How [`RemoteEnv`] is set up (upstream `RemoteExecutionEnvOptions`).
#[derive(Debug, Clone)]
pub struct RemoteEnvOptions {
    pub connection: Connection,
    /// The file namespace: equal ids see the same files, e.g. `pi-env:<env name>`.
    pub id: String,
    /// An absolute path on the remote machine.
    pub cwd: String,
    /// The shell for string commands on the remote machine.
    pub shell_path: Option<String>,
    /// Added to the remote environment of every command that inherits it.
    pub shell_env: Vec<(String, String)>,
    /// How the daemon watches.
    pub watch: RemoteWatchOptions,
}

impl RemoteEnvOptions {
    pub fn new(connection: Connection, id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            connection,
            id: id.into(),
            cwd: cwd.into(),
            shell_path: None,
            shell_env: Vec::new(),
            watch: RemoteWatchOptions::default(),
        }
    }
}

/// An `ExecutionEnv` on another machine, reached through a pi-env daemon (upstream `RemoteExecutionEnv`). Results
/// match `NodeExecutionEnv` running on that machine; only error messages may differ. Remote systems use POSIX paths:
/// a Windows daemon is refused with `not_supported`.
#[derive(Debug)]
pub struct RemoteEnv {
    id: String,
    cwd: String,
    connection: Connection,
    shell_path: Option<String>,
    shell_env: Vec<(String, String)>,
    watch_options: RemoteWatchOptions,
    /// Running commands this environment started (request id to session), for `cleanup`.
    running: Arc<Mutex<BTreeMap<u32, u32>>>,
}

impl RemoteEnv {
    pub fn new(options: RemoteEnvOptions) -> Self {
        Self {
            id: options.id,
            cwd: options.cwd,
            connection: options.connection,
            shell_path: options.shell_path,
            shell_env: options.shell_env,
            watch_options: options.watch,
            running: Arc::default(),
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    async fn info(&self) -> Result<Arc<RemoteInfo>, Failure> {
        let info = self.connection.info().await?;
        if node_platform(&info.os, &info.arch).0 == "win32" {
            return Err(Failure::File(FileError::new(
                FileErrorCode::NotSupported,
                "noel-env does not support Windows remotes",
                None,
            )));
        }
        Ok(info)
    }

    /// Node's `resolvePath` on the remote system, with its home directory.
    async fn resolve(&self, path: &str) -> Result<String, Failure> {
        let info = self.info().await?;
        Ok(resolve_local_path(&self.cwd, path, Some(&info.home)))
    }

    /// Upstream's `#fileOp` up to the call: the resolved path, once the context is checked.
    async fn begin(&self, path: &str, cx: &Context) -> FileResult<String> {
        let resolved = self
            .resolve(path)
            .await
            .map_err(|failure| to_file_error(failure, Some(path)))?;
        check_abort(cx, Some(&resolved))?;
        Ok(resolved)
    }

    async fn request(
        &self,
        op: &str,
        json: Value,
    ) -> Result<crate::connection::Reply, RemoteError> {
        self.connection
            .request(op, object(json), RequestOptions::default())
            .await
    }

    async fn open(&self, resolved: &str, json: Value) -> Result<(Handle, Json), Failure> {
        let mut json = object(json);
        json.insert("path".into(), Value::from(resolved));
        let reply = self
            .connection
            .request("open", json, RequestOptions::default())
            .await?;
        let handle = Handle::from_reply(&self.connection, &reply.json, reply.session, resolved)?;
        Ok((handle, reply.json))
    }

    async fn open_dir(&self, resolved: &str) -> Result<Handle, Failure> {
        let reply = self.request("opendir", json!({ "path": resolved })).await?;
        Handle::from_reply(&self.connection, &reply.json, reply.session, resolved)
    }

    /// Node's `readFile`, with or without UTF-8 decoding, over a file opened like `open(path, "r")`.
    async fn read_file(&self, path: &str, decode: bool, cx: &Context) -> FileResult<Vec<u8>> {
        let resolved = self.begin(path, cx).await?;
        let read = async {
            let (handle, json) = self.open(&resolved, json!({ "mode": "read" })).await?;
            let read = self
                .read_open_file(&handle, &json, &resolved, decode, cx)
                .await;
            handle.close().await;
            read
        };
        read.await
            .map_err(|failure| to_file_error(failure, Some(&resolved)))
    }

    async fn read_open_file(
        &self,
        handle: &Handle,
        json: &Json,
        resolved: &str,
        decode: bool,
        cx: &Context,
    ) -> Result<Vec<u8>, Failure> {
        if let Some(error) = json.get("statError") {
            return Err(RemoteError::new(object(error.clone())).into());
        }
        if cx.is_aborted() {
            return Err(operation_aborted(resolved));
        }
        let stat = json.get("stat").cloned().unwrap_or(Value::Null);
        let size = if stat["kind"] == "file" {
            stat["size"].as_u64().unwrap_or(0)
        } else {
            0
        };
        if size > READ_FILE_MAX {
            return Err(FileError::new(
                FileErrorCode::Unknown,
                format!("File size ({size}) is greater than 2 GiB"),
                Some(resolved),
            )
            .into());
        }
        if size == 0 {
            // Unknown size: sequential reads until the end, as for FIFOs and devices.
            let mut bytes = Vec::new();
            loop {
                if cx.is_aborted() {
                    return Err(operation_aborted(resolved));
                }
                let read = handle.pread(None, READ_FILE_UNKNOWN_CHUNK).await?;
                if read.is_empty() {
                    return Ok(bytes);
                }
                bytes.extend_from_slice(&read);
            }
        }
        if !decode {
            // A single read of the whole size, or reads of 512 KiB until the size or the end of the file.
            if size <= READ_FILE_CHUNK {
                return Ok(handle.pread(Some(0), size).await?);
            }
            return match read_range(handle, 0, size, READ_FILE_CHUNK, || cx.is_aborted()).await? {
                Some(bytes) => Ok(bytes),
                None => Err(operation_aborted(resolved)),
            };
        }
        // Decoding: reads of min(size, 512 KiB) until the size, a short read, or the end of the file.
        let length = size.min(READ_FILE_CHUNK);
        let mut bytes = Vec::new();
        let mut next = 0;
        let mut in_flight: VecDeque<PendingRequest> = VecDeque::new();
        loop {
            while in_flight.len() < READ_DEPTH {
                in_flight.push_back(handle.start_pread(Some(next), length)?);
                next += length;
            }
            let Some(head) = in_flight.pop_front() else {
                return Ok(bytes);
            };
            let read = head.wait(None, None).await?.payload;
            if cx.is_aborted() {
                return Err(operation_aborted(resolved));
            }
            bytes.extend_from_slice(&read);
            if read.is_empty() || bytes.len() as u64 == size || read.len() as u64 != length {
                return Ok(bytes);
            }
        }
    }

    /// Node's `writeFile` (abort checked before each 512 KiB write) or `appendFile` (no checks, one after): the
    /// first request creates parents, opens, and writes the first chunk; later chunks go to the same open file.
    async fn write(
        &self,
        path: &str,
        content: &[u8],
        append: bool,
        cx: &Context,
    ) -> FileResult<()> {
        let resolved = self.begin(path, cx).await?;
        let written = self.write_chunks(&resolved, content, append, cx).await;
        written.map_err(|failure| to_file_error(failure, Some(&resolved)))?;
        if append {
            check_abort(cx, Some(&resolved))?;
        }
        Ok(())
    }

    async fn write_chunks(
        &self,
        resolved: &str,
        content: &[u8],
        append: bool,
        cx: &Context,
    ) -> Result<(), Failure> {
        let first = &content[..content.len().min(WRITE_CHUNK)];
        let keep = content.len() > WRITE_CHUNK;
        let reply = self
            .connection
            .request(
                "write",
                object(json!({ "path": resolved, "append": append, "keep": keep })),
                RequestOptions {
                    payload: first,
                    ..RequestOptions::default()
                },
            )
            .await?;
        if !keep {
            return Ok(());
        }
        let handle = Handle::from_reply(&self.connection, &reply.json, reply.session, resolved)?;
        let mut in_flight: VecDeque<PendingRequest> = VecDeque::new();
        let outcome = async {
            for offset in (WRITE_CHUNK..content.len()).step_by(WRITE_CHUNK) {
                if !append && cx.is_aborted() {
                    // A chunk that failed before this check fails the write, as it would have in sequence.
                    let mut failure = None;
                    while let Some(written) = in_flight.pop_front() {
                        if let Err(error) = written.wait(None, None).await {
                            failure.get_or_insert(error);
                        }
                    }
                    return Err(failure.map_or_else(|| operation_aborted(resolved), Failure::from));
                }
                let chunk = &content[offset..content.len().min(offset + WRITE_CHUNK)];
                in_flight.push_back(handle.start("writeChunk", Json::new(), chunk)?);
                if in_flight.len() >= WRITE_DEPTH {
                    if let Some(written) = in_flight.pop_front() {
                        written.wait(None, None).await?;
                    }
                }
            }
            // The first failure; later chunks are refused by the daemon, so the file has no gaps.
            while let Some(written) = in_flight.pop_front() {
                written.wait(None, None).await?;
            }
            Ok(())
        }
        .await;
        while let Some(written) = in_flight.pop_front() {
            let _ = written.wait(None, None).await;
        }
        handle.close().await;
        outcome
    }

    /// One request about one path, as upstream's `#fileOp`, checking the context before (and after, with
    /// `check_after`).
    async fn simple(
        &self,
        path: &str,
        cx: &Context,
        check_after: bool,
        op: &str,
        fields: Value,
    ) -> FileResult<Json> {
        let resolved = self.begin(path, cx).await?;
        let mut json = object(fields);
        json.insert("path".into(), Value::from(resolved.as_str()));
        let reply = self
            .connection
            .request(op, json, RequestOptions::default())
            .await
            .map_err(|error| to_file_error(error.into(), Some(&resolved)))?;
        if check_after {
            check_abort(cx, Some(&resolved))?;
        }
        Ok(reply.json)
    }
}

/// `listFiles` of an absolute path through the daemon (upstream `listFilesOver`): fails with `not_supported` if the
/// daemon does not list files (a release before `listFiles`), so callers fall back to their own walk.
pub async fn list_files_over(
    connection: &Connection,
    path: &str,
    options: &ListFilesOptions,
    cx: &Context,
) -> FileResult<FileListing> {
    let invalid = |key: &str| {
        Err(FileError::new(
            FileErrorCode::Invalid,
            format!("{key} must be a non-negative safe integer"),
            Some(path),
        ))
    };
    let mut json = Json::new();
    json.insert("path".into(), Value::from(path));
    if let Some(hidden) = options.hidden {
        json.insert("hidden".into(), Value::Bool(hidden));
    }
    if let Some(gitignore) = options.gitignore {
        json.insert("gitignore".into(), Value::Bool(gitignore));
    }
    if !options.exclude.is_empty() {
        json.insert("exclude".into(), Value::from(options.exclude.clone()));
    }
    for (key, value) in [
        ("maxEntries", options.max_entries),
        ("maxBytes", options.max_bytes),
    ] {
        if let Some(value) = value {
            if value > MAX_SAFE_INTEGER {
                return invalid(key);
            }
            json.insert(key.into(), Value::from(value));
        }
    }
    if let Some(timeout) = options.timeout {
        // Only whole milliseconds are a safe integer.
        if !timeout.subsec_nanos().is_multiple_of(1_000_000)
            || timeout.as_millis() > u128::from(MAX_SAFE_INTEGER)
        {
            return invalid("timeoutMs");
        }
        json.insert("timeoutMs".into(), Value::from(timeout.as_millis() as u64));
    }
    check_abort(cx, Some(path))?;
    let listed = async {
        let info = connection.info().await?;
        if !info
            .features
            .as_ref()
            .is_some_and(|features| features.iter().any(|feature| feature == "listFiles"))
        {
            return Err(Failure::File(FileError::new(
                FileErrorCode::NotSupported,
                "The pi-env daemon does not list files; update it",
                Some(path),
            )));
        }
        let reply = connection
            .request(
                "listFiles",
                json,
                RequestOptions {
                    cx: Some(cx),
                    cancel_on_drop: true,
                    ..RequestOptions::default()
                },
            )
            .await?;
        Ok(reply)
    };
    let reply = listed
        .await
        .map_err(|failure| to_file_error(failure, Some(path)))?;
    check_abort(cx, Some(path))?;
    // Records of a kind byte, the UTF-8 path, and a NUL.
    let mut entries = Vec::new();
    let mut start = 0;
    while start < reply.payload.len() {
        let Some(end) = reply.payload[start..].iter().position(|byte| *byte == 0) else {
            break;
        };
        let end = start + end;
        let kind = match reply.payload[start] {
            b'f' => Some(FileKind::File),
            b'd' => Some(FileKind::Directory),
            b'l' => Some(FileKind::Symlink),
            _ => None,
        };
        if let Some(kind) = kind {
            let path = String::from_utf8_lossy(&reply.payload[start + 1..end]).into_owned();
            entries.push(FileListEntry { path, kind });
        }
        start = end + 1;
    }
    Ok(FileListing {
        entries,
        truncated: reply.json.get("truncated") == Some(&Value::Bool(true)),
    })
}

#[async_trait]
impl FileSystem for RemoteEnv {
    fn id(&self) -> &str {
        &self.id
    }

    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, _cx: &Context) -> FileResult<String> {
        self.resolve(path)
            .await
            .map_err(|failure| to_file_error(failure, Some(path)))
    }

    async fn join_path(&self, parts: &[&str], _cx: &Context) -> FileResult<String> {
        self.info()
            .await
            .map_err(|failure| to_file_error(failure, None))?;
        Ok(join(parts))
    }

    async fn read_text_file(&self, path: &str, cx: &Context) -> FileResult<String> {
        let bytes = self.read_file(path, true, cx).await?;
        // Node decodes a whole file keeping its byte-order mark.
        Ok(match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        })
    }

    async fn open_text_line_reader(
        &self,
        path: &str,
        cx: &Context,
    ) -> FileResult<Box<dyn TextLineReader>> {
        let resolved = self.begin(path, cx).await?;
        let (handle, _) = self
            .open(&resolved, json!({ "mode": "read" }))
            .await
            .map_err(|failure| to_file_error(failure, Some(&resolved)))?;
        if cx.is_aborted() {
            handle.close().await;
            return Err(FileError::aborted(Some(&resolved)));
        }
        Ok(Box::new(RemoteTextLineReader {
            handle,
            decoder: StreamDecoder::new(),
            offset: 0,
            buffered: String::new(),
            consumed: 0,
            ended: false,
            closed: false,
        }))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<u64>,
        cx: &Context,
    ) -> FileResult<Vec<String>> {
        if max_lines == Some(0) {
            return Ok(Vec::new());
        }
        let mut reader = self.open_text_line_reader(path, cx).await?;
        let mut lines = Vec::new();
        let result = loop {
            if max_lines.is_some_and(|max| lines.len() as u64 >= max) {
                break Ok(lines);
            }
            match reader.read_line(cx).await {
                Ok(Some(line)) => lines.push(line.text),
                Ok(None) => break Ok(lines),
                Err(error) => break Err(error),
            }
        };
        reader.close(cx).await;
        result
    }

    async fn read_binary_file(&self, path: &str, cx: &Context) -> FileResult<Vec<u8>> {
        self.read_file(path, false, cx).await
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        options: BinaryReaderOptions,
        cx: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        let resolved = self.begin(path, cx).await?;
        let (handle, _) = self
            .open(&resolved, json!({ "noFollow": options.no_follow }))
            .await
            .map_err(|failure| to_file_error(failure, Some(&resolved)))?;
        if cx.is_aborted() {
            handle.close().await;
            return Err(FileError::aborted(Some(&resolved)));
        }
        Ok(Box::new(RemoteBinaryReader {
            handle,
            closed: false,
        }))
    }

    async fn write_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()> {
        self.write(path, content, false, cx).await
    }

    async fn append_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()> {
        self.write(path, content, true, cx).await
    }

    async fn truncate_file(&self, path: &str, size: u64, cx: &Context) -> FileResult<()> {
        let resolved = self.begin(path, cx).await?;
        if size > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "File size must be a non-negative safe integer",
                Some(&resolved),
            ));
        }
        self.simple(&resolved, cx, true, "truncate", json!({ "size": size }))
            .await
            .map(drop)
    }

    async fn flush_file(&self, path: &str, cx: &Context) -> FileResult<()> {
        self.simple(path, cx, true, "fsync", json!({}))
            .await
            .map(drop)
    }

    async fn rename_file(&self, source: &str, destination: &str, cx: &Context) -> FileResult<()> {
        let resolved = async {
            Ok::<_, Failure>((
                self.resolve(source).await?,
                self.resolve(destination).await?,
            ))
        };
        let (source, destination) = resolved
            .await
            .map_err(|failure| to_file_error(failure, Some(source)))?;
        check_abort(cx, Some(&destination))?;
        self.request("rename", json!({ "path": source, "to": destination }))
            .await
            .map(drop)
            .map_err(|error| to_file_error(error.into(), Some(&source)))
    }

    async fn file_info(&self, path: &str, cx: &Context) -> FileResult<FileInfo> {
        let resolved = self.begin(path, cx).await?;
        let reply = self
            .request("lstat", json!({ "path": resolved }))
            .await
            .map_err(|error| to_file_error(error.into(), Some(&resolved)))?;
        to_info(&resolved, &Value::Object(reply.json))
    }

    async fn list_dir(&self, path: &str, cx: &Context) -> FileResult<Vec<FileInfo>> {
        let resolved = self.begin(path, cx).await?;
        let listed = async {
            let handle = self.open_dir(&resolved).await?;
            let mut entries = Vec::new();
            let read = async {
                loop {
                    let reply = handle
                        .request("readdir", object(json!({ "max": 1000 })), None)
                        .await?;
                    if let Some(Value::Array(page)) = reply.json.get("entries") {
                        entries.extend(page.iter().cloned());
                    }
                    if reply.json.get("done") == Some(&Value::Bool(true)) {
                        return Ok::<_, Failure>(());
                    }
                }
            }
            .await;
            handle.close().await;
            read?;
            Ok::<_, Failure>(entries)
        };
        let mut entries = listed
            .await
            .map_err(|failure| to_file_error(failure, Some(&resolved)))?;
        // Node's `readdir` fails on any entry it cannot lstat. libuv sorts names by their bytes on POSIX.
        let key = |entry: &Value| -> Vec<u8> {
            match entry.get("raw").and_then(Value::as_array) {
                Some(raw) => raw
                    .iter()
                    .map(|byte| byte.as_u64().unwrap_or(0) as u8)
                    .collect(),
                None => entry["name"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes()
                    .to_vec(),
            }
        };
        entries.sort_by_key(key);
        let mut infos = Vec::new();
        for entry in entries {
            check_abort(cx, Some(&resolved))?;
            let entry_path = resolve(&[
                resolved.as_str(),
                entry["name"].as_str().unwrap_or_default(),
            ]);
            if let Some(error) = entry.get("error") {
                let error = RemoteError::new(object(error.clone()));
                return Err(to_file_error(error.into(), Some(&entry_path)));
            }
            if let Ok(info) = to_info(&entry_path, &entry["info"]) {
                infos.push(info);
            }
        }
        Ok(infos)
    }

    /// Through the daemon; `not_supported` with a daemon released before `listFiles`.
    async fn list_files(
        &self,
        path: &str,
        options: &ListFilesOptions,
        cx: &Context,
    ) -> FileResult<FileListing> {
        let resolved = self
            .resolve(path)
            .await
            .map_err(|failure| to_file_error(failure, Some(path)))?;
        list_files_over(&self.connection, &resolved, options, cx).await
    }

    async fn open_dir_reader(&self, path: &str, cx: &Context) -> FileResult<Box<dyn DirReader>> {
        let resolved = self.begin(path, cx).await?;
        let handle = self
            .open_dir(&resolved)
            .await
            .map_err(|failure| to_file_error(failure, Some(&resolved)))?;
        if cx.is_aborted() {
            handle.close().await;
            return Err(FileError::aborted(Some(&resolved)));
        }
        Ok(Box::new(RemoteDirReader {
            handle,
            done: false,
            closed: false,
        }))
    }

    async fn watch(
        &self,
        targets: &[WatchTarget],
        on_change: WatchCallback,
        cx: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        check_abort(cx, None)?;
        let mut resolved = Vec::with_capacity(targets.len());
        for target in targets {
            let path = self
                .resolve(&target.path)
                .await
                .map_err(|failure| to_file_error(failure, None))?;
            resolved.push(WatchTarget {
                path,
                ..target.clone()
            });
        }
        check_abort(cx, None)?;
        let mut watcher = RemoteWatcher::open(
            self.connection.clone(),
            &resolved,
            on_change,
            &self.watch_options,
        )
        .await
        .map_err(|error| to_file_error(error.into(), None))?;
        if cx.is_aborted() {
            watcher.close(cx).await;
            return Err(FileError::aborted(None));
        }
        Ok(Box::new(watcher))
    }

    async fn canonical_path(&self, path: &str, cx: &Context) -> FileResult<String> {
        let reply = self.simple(path, cx, false, "realpath", json!({})).await?;
        reply
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| to_file_error(malformed("realpath"), Some(path)))
    }

    async fn exists(&self, path: &str, cx: &Context) -> FileResult<bool> {
        match self.file_info(path, cx).await {
            Ok(_) => Ok(true),
            Err(error) if error.code == FileErrorCode::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn create_dir(
        &self,
        path: &str,
        options: CreateDirOptions,
        cx: &Context,
    ) -> FileResult<()> {
        self.simple(
            path,
            cx,
            false,
            "mkdir",
            json!({ "recursive": options.recursive }),
        )
        .await
        .map(drop)
    }

    async fn remove(&self, path: &str, options: RemoveOptions, cx: &Context) -> FileResult<()> {
        self.simple(
            path,
            cx,
            false,
            "rm",
            json!({ "recursive": options.recursive, "force": options.force }),
        )
        .await
        .map(drop)
    }

    async fn create_temp_dir(&self, prefix: Option<&str>, cx: &Context) -> FileResult<String> {
        check_abort(cx, None)?;
        let created = async {
            let info = self.info().await?;
            let template = join(&[info.tmpdir.as_str(), prefix.unwrap_or("tmp-")]);
            let reply = self.request("mkdtemp", json!({ "path": template })).await?;
            reply
                .json
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| malformed("mkdtemp"))
        };
        created
            .await
            .map_err(|failure| to_file_error(failure, None))
    }

    async fn create_temp_file(
        &self,
        options: &TempFileOptions,
        cx: &Context,
    ) -> FileResult<String> {
        let directory = self.create_temp_dir(Some("tmp-"), cx).await?;
        let file = join(&[
            directory.as_str(),
            &format!(
                "{}{}{}",
                options.prefix.as_deref().unwrap_or_default(),
                crate::local::random_uuid(),
                options.suffix.as_deref().unwrap_or_default()
            ),
        ]);
        // Node's `writeFile(file, "")`, which does not create parents.
        self.request(
            "write",
            json!({ "path": file, "append": false, "parents": false }),
        )
        .await
        .map_err(|error| to_file_error(error.into(), Some(&file)))?;
        Ok(file)
    }
}

fn output_skip(value: &Value) -> Option<OutputSkip> {
    Some(OutputSkip {
        bytes: value.get("bytes")?.as_u64()?,
        newlines: value.get("newlines")?.as_u64()?,
        ends_with_newline: value.get("endsWithNewline")?.as_bool()?,
    })
}

#[async_trait]
impl Shell for RemoteEnv {
    async fn exec(
        &self,
        command: ShellCommand,
        options: ExecOptions<'_>,
        cx: &Context,
    ) -> Result<ExecResult, ExecutionError> {
        if cx.is_aborted() {
            return Err(ExecutionError::aborted());
        }
        if let Some(timeout) = options.timeout {
            if timeout.is_zero() {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    "Invalid timeout: must be a finite number of seconds",
                ));
            }
            if timeout > MAX_TIMEOUT {
                return Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!(
                        "Invalid timeout: maximum is {} seconds",
                        MAX_TIMEOUT.as_secs_f64()
                    ),
                ));
            }
        }
        let cwd = match options.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => self.resolve(cwd).await.map_err(|failure| {
                ExecutionError::new(
                    ExecutionErrorCode::Unknown,
                    to_file_error(failure, None).message,
                )
            })?,
            None => self.cwd.clone(),
        };
        if cx.is_aborted() {
            return Err(ExecutionError::aborted());
        }
        // The daemon's environment, then `shell_env`, then the command's own; an entry without a value only removes
        // what an earlier layer here added, as upstream skips `undefined` values.
        let mut env: Vec<(String, Option<String>)> = Vec::new();
        let mut set = |name: &str, value: Option<&str>| {
            env.retain(|(existing, _)| existing != name);
            env.push((name.to_string(), value.map(str::to_string)));
        };
        if options.inherit_env {
            for (name, value) in &self.shell_env {
                set(name, Some(value));
            }
        }
        for (name, value) in &options.env {
            set(name, value.as_deref());
        }
        let env: Json = env
            .into_iter()
            .filter_map(|(name, value)| Some((name, Value::from(value?))))
            .collect();
        let mut json = Json::new();
        match command {
            ShellCommand::Script(script) => json.insert("command".into(), Value::from(script)),
            ShellCommand::Argv(argv) => json.insert("argv".into(), Value::from(argv)),
        };
        json.insert("cwd".into(), Value::from(cwd));
        json.insert("env".into(), Value::Object(env));
        json.insert("inheritEnv".into(), Value::Bool(options.inherit_env));
        if let Some(shell_path) = &self.shell_path {
            json.insert("shellPath".into(), Value::from(shell_path.as_str()));
        }
        if let Some(timeout) = options.timeout {
            json.insert("timeoutMs".into(), millis(timeout));
        }
        if let Some(spill) = options.spill {
            json.insert(
                "spill".into(),
                json!({ "afterBytes": spill.after_bytes, "afterLines": spill.after_lines }),
            );
        }
        if let Some(window) = options.window {
            json.insert(
                "window".into(),
                json!({
                    "maxBytes": window.max_bytes,
                    "maxLines": window.max_lines,
                    "minIntervalMs": millis(window.min_interval),
                    "bytesPerSecond": js_number(window.bytes_per_second),
                }),
            );
        }

        let mut on_output = options.on_output;
        let mut callback_error: Option<String> = None;
        let mut on_event = |event: &Json, payload: &[u8]| -> bool {
            if callback_error.is_some() || event.get("kind") != Some(&Value::from("output")) {
                return true;
            }
            let text = String::from_utf8_lossy(payload);
            let Some(on_output) = on_output.as_mut() else {
                return true;
            };
            if text.is_empty() {
                return true;
            }
            let stream = if event.get("stream") == Some(&Value::from("stderr")) {
                OutputStream::Stderr
            } else {
                OutputStream::Stdout
            };
            let skipped = event.get("skipped").and_then(output_skip);
            match on_output(OutputChunk {
                text: &text,
                stream,
                skipped,
                raw: None,
            }) {
                Ok(()) => true,
                Err(message) => {
                    // Kills the command; it still settles before the call returns.
                    callback_error = Some(message);
                    false
                }
            }
        };
        let mut started = None;
        let running = self.running.clone();
        let mut on_start = |id: u32, session: u32| {
            started = Some(id);
            running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id, session);
        };
        let result = self
            .connection
            .request(
                "exec",
                json,
                RequestOptions {
                    cx: Some(cx),
                    on_event: Some(&mut on_event),
                    on_start: Some(&mut on_start),
                    cancel_on_drop: true,
                    ..RequestOptions::default()
                },
            )
            .await;
        if let Some(id) = started {
            self.running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
        }
        if let Some(message) = callback_error {
            return Err(ExecutionError::new(
                ExecutionErrorCode::CallbackError,
                message,
            ));
        }
        match result {
            Ok(reply) => Ok(ExecResult {
                exit_code: reply
                    .json
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .map_or(1, |code| code as i32),
                spill_path: reply
                    .json
                    .get("spillPath")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            Err(error) => {
                let failure = match error.code.as_str() {
                    "timeout" => ExecutionError::new(
                        ExecutionErrorCode::Timeout,
                        format!(
                            "timeout:{}",
                            options.timeout.unwrap_or_default().as_secs_f64()
                        ),
                    ),
                    "aborted" => ExecutionError::aborted(),
                    "shell_unavailable" => {
                        ExecutionError::new(ExecutionErrorCode::ShellUnavailable, error.message)
                    }
                    "spawn_error" => {
                        ExecutionError::new(ExecutionErrorCode::SpawnError, error.message)
                    }
                    _ => ExecutionError::new(ExecutionErrorCode::Unknown, error.message),
                };
                Err(ExecutionError {
                    spill_path: error
                        .fields
                        .get("spillPath")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ..failure
                })
            }
        }
    }

    /// Kills without aborting, so the commands settle with their killed status, as `NodeExecutionEnv` does.
    async fn cleanup(&self, _cx: &Context) {
        let running =
            std::mem::take(&mut *self.running.lock().unwrap_or_else(PoisonError::into_inner));
        for (id, session) in running {
            self.connection.kill(id, session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_sent_as_javascript_sends_them() {
        assert_eq!(millis(Duration::from_millis(300)).to_string(), "300");
        assert_eq!(millis(Duration::from_micros(1500)).to_string(), "1.5");
        assert_eq!(js_number(1e6).to_string(), "1000000");
        assert_eq!(js_number(0.5).to_string(), "0.5");
    }

    #[test]
    fn remote_errors_map_as_node_execution_env_maps_node_codes() {
        let code =
            |code: &str| to_file_error(RemoteError::with(code, "x", false).into(), Some("/p")).code;
        assert_eq!(code("ENOENT"), FileErrorCode::NotFound);
        assert_eq!(code("EPERM"), FileErrorCode::PermissionDenied);
        assert_eq!(code("NOT_REGULAR"), FileErrorCode::Invalid);
        assert_eq!(code("ERR_FS_EISDIR"), FileErrorCode::Unknown);
        assert_eq!(code("aborted"), FileErrorCode::Aborted);
        let error = to_file_error(RemoteError::with("EISDIR", "x", false).into(), Some("/p"));
        assert_eq!(error.path.as_deref(), Some("/p"));
        let info = to_info(
            "/a/b.txt",
            &json!({"kind": "file", "size": 3, "mtimeSec": 2, "mtimeNsec": 500_000}),
        )
        .unwrap();
        assert_eq!(
            (info.name.as_str(), info.size, info.mtime_ms),
            ("b.txt", 3, 2000.5)
        );
        assert_eq!(
            to_info("/a", &json!({"kind": "other"})).unwrap_err().code,
            FileErrorCode::Invalid
        );
    }
}
