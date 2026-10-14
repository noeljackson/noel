//! `LocalEnv`: the workspace on this machine. It behaves as upstream's
//! `NodeExecutionEnv` (`packages/durable/src/env/node.ts`, described in
//! `packages/env/docs/semantics.md`): the same path resolution, abort
//! checkpoints, error codes, and the rules Node's own library follows.

use std::collections::hash_map::RandomState;
use std::collections::BTreeSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use crate::line_scan::LineScanner;
use crate::path::{basename, home_dir, join, resolve, resolve_local_path, tmpdir};
use crate::{
    BinaryReader, BinaryReaderOptions, Context, CreateDirOptions, DirPage, DirReader, FileError,
    FileErrorCode, FileInfo, FileKind, FileResult, FileSystem, FileWatcher, LineRange, LineScan,
    RemoveOptions, StreamDecoder, TempFileOptions, TextLine, TextLineReader, WatchCallback,
    WatchTarget, MAX_SAFE_INTEGER,
};

/// The id of every [`LocalEnv`]: they all see this machine's files.
pub const LOCAL_ENV_ID: &str = "local";

/// Largest single read of a binary reader, so a huge length allocates only
/// as much as the file yields.
const BINARY_READ_CHUNK: u64 = 1024 * 1024;
/// Reads of the line reader and of line scans.
const SCAN_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct LocalEnvOptions {
    /// The shell for string commands (the `shellPath` setting). Without it,
    /// `/bin/bash`, then `bash` on `PATH`, then `sh`.
    pub shell_path: Option<String>,
}

/// The workspace on this machine, rooted at a working directory.
#[derive(Debug)]
pub struct LocalEnv {
    pub(crate) cwd: String,
    pub(crate) shell_path: Option<String>,
    /// Process groups of the commands this environment runs.
    pub(crate) running: Arc<Mutex<BTreeSet<i32>>>,
}

impl LocalEnv {
    /// An environment whose relative paths and commands start at `cwd`.
    pub fn new(cwd: impl AsRef<std::path::Path>) -> Self {
        Self::with_options(cwd, LocalEnvOptions::default())
    }

    pub fn with_options(cwd: impl AsRef<std::path::Path>, options: LocalEnvOptions) -> Self {
        Self {
            cwd: cwd.as_ref().to_string_lossy().into_owned(),
            shell_path: options.shell_path,
            running: Arc::default(),
        }
    }

    pub(crate) fn resolve(&self, path: &str) -> String {
        resolve_local_path(&self.cwd, path, home_dir().as_deref())
    }

    pub(crate) fn running(&self) -> MutexGuard<'_, BTreeSet<i32>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs blocking file system work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) => std::panic::resume_unwind(error.into_panic()),
    }
}

fn check_abort(cx: &Context, path: Option<&str>) -> FileResult<()> {
    if cx.is_aborted() {
        Err(FileError::aborted(path))
    } else {
        Ok(())
    }
}

fn file_kind(metadata: &Metadata) -> Option<FileKind> {
    let file_type = metadata.file_type();
    if file_type.is_file() {
        Some(FileKind::File)
    } else if file_type.is_dir() {
        Some(FileKind::Directory)
    } else if file_type.is_symlink() {
        Some(FileKind::Symlink)
    } else {
        None
    }
}

/// `fileInfoFromStats`: other kinds of files are `invalid`.
fn file_info(path: &str, metadata: &Metadata) -> FileResult<FileInfo> {
    let kind = file_kind(metadata).ok_or_else(|| {
        FileError::new(FileErrorCode::Invalid, "Unsupported file type", Some(path))
    })?;
    Ok(FileInfo {
        name: basename(path).to_string(),
        path: path.to_string(),
        kind,
        size: metadata.len(),
        mtime_ms: metadata.mtime() as f64 * 1000.0 + metadata.mtime_nsec() as f64 / 1e6,
    })
}

fn io_error(path: &str) -> impl Fn(io::Error) -> FileError + '_ {
    move |error| FileError::from_io(&error, path)
}

/// Node's `readFile`: a regular file up to its size at open, other files to
/// their end.
fn read_whole(path: &str) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut bytes = Vec::new();
    if metadata.is_file() {
        bytes.reserve(metadata.len() as usize);
        file.take(metadata.len()).read_to_end(&mut bytes)?;
    } else {
        file.read_to_end(&mut bytes)?;
    }
    Ok(bytes)
}

/// Random characters as `mkdtemp(3)` uses.
fn random_characters(count: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = RandomState::new().build_hasher().finish();
    (0..count)
        .map(|index| {
            if index % 10 == 9 {
                random = RandomState::new().build_hasher().finish();
            }
            let character = ALPHABET[(random % ALPHABET.len() as u64) as usize];
            random /= ALPHABET.len() as u64;
            character as char
        })
        .collect()
}

/// A random version 4 UUID, as Node's `randomUUID()` writes it.
pub(crate) fn random_uuid() -> String {
    let high = RandomState::new().build_hasher().finish();
    let low = RandomState::new().build_hasher().finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// `mkdtemp(3)` of `<tmpdir>/<prefix>XXXXXX`, the prefix joined as
/// `path.join` joins it.
pub(crate) fn create_temp_dir_sync(prefix: &str) -> FileResult<String> {
    let template = join(&[tmpdir().as_str(), prefix]);
    loop {
        let candidate = format!("{template}{}", random_characters(6));
        match fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(FileError::from_io(&error, &format!("{template}XXXXXX"))),
        }
    }
}

/// An empty file named `<prefix><uuid><suffix>` in a fresh temporary
/// directory.
pub(crate) fn create_temp_file_sync(prefix: &str, suffix: &str) -> FileResult<String> {
    let directory = create_temp_dir_sync("tmp-")?;
    let path = join(&[
        directory.as_str(),
        &format!("{prefix}{}{suffix}", random_uuid()),
    ]);
    fs::write(&path, b"").map_err(io_error(&path))?;
    Ok(path)
}

#[async_trait]
impl FileSystem for LocalEnv {
    fn id(&self) -> &str {
        LOCAL_ENV_ID
    }

    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, _cx: &Context) -> FileResult<String> {
        Ok(self.resolve(path))
    }

    async fn join_path(&self, parts: &[&str], _cx: &Context) -> FileResult<String> {
        Ok(join(parts))
    }

    async fn read_text_file(&self, path: &str, cx: &Context) -> FileResult<String> {
        let bytes = self.read_binary_file(path, cx).await?;
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
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let file = blocking({
            let resolved = resolved.clone();
            move || File::open(&resolved)
        })
        .await
        .map_err(io_error(&resolved))?;
        check_abort(cx, Some(&resolved))?;
        Ok(Box::new(LocalTextLineReader {
            file: Some(Arc::new(file)),
            path: resolved,
            decoder: StreamDecoder::new(),
            offset: 0,
            buffered: String::new(),
            consumed: 0,
            ended: false,
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
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        blocking({
            let resolved = resolved.clone();
            move || read_whole(&resolved)
        })
        .await
        .map_err(io_error(&resolved))
    }

    async fn open_binary_reader(
        &self,
        path: &str,
        options: BinaryReaderOptions,
        cx: &Context,
    ) -> FileResult<Box<dyn BinaryReader>> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let opened = blocking({
            let resolved = resolved.clone();
            move || {
                // Nonblocking, so opening a FIFO does not wait for a writer.
                let mut flags = libc::O_NONBLOCK;
                if options.no_follow {
                    flags |= libc::O_NOFOLLOW;
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(flags)
                    .open(&resolved)?;
                let metadata = file.metadata()?;
                Ok::<_, io::Error>((file, metadata))
            }
        })
        .await;
        let (file, metadata) = opened.map_err(|error| {
            // O_NOFOLLOW reports a final symbolic link as ELOOP (EMLINK on
            // some BSDs).
            let refused = matches!(error.raw_os_error(), Some(libc::ELOOP | libc::EMLINK));
            if options.no_follow && refused {
                FileError {
                    errno: error.raw_os_error(),
                    ..FileError::new(
                        FileErrorCode::Invalid,
                        "Refusing to follow a symbolic link",
                        Some(&resolved),
                    )
                }
            } else {
                FileError::from_io(&error, &resolved)
            }
        })?;
        if !metadata.is_file() {
            return Err(if metadata.is_dir() {
                FileError::new(
                    FileErrorCode::IsDirectory,
                    "EISDIR: illegal operation on a directory, read",
                    Some(&resolved),
                )
            } else {
                FileError::new(
                    FileErrorCode::Invalid,
                    "Not a regular file",
                    Some(&resolved),
                )
            });
        }
        check_abort(cx, Some(&resolved))?;
        Ok(Box::new(LocalBinaryReader {
            file: Some(Arc::new(file)),
            path: resolved,
        }))
    }

    async fn write_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let parent = resolve(&[resolved.as_str(), ".."]);
        blocking({
            let parent = parent.clone();
            move || fs::create_dir_all(&parent)
        })
        .await
        .map_err(io_error(&parent))?;
        check_abort(cx, Some(&resolved))?;
        let content = content.to_vec();
        blocking({
            let resolved = resolved.clone();
            move || fs::write(&resolved, content)
        })
        .await
        .map_err(io_error(&resolved))
    }

    async fn append_file(&self, path: &str, content: &[u8], cx: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let parent = resolve(&[resolved.as_str(), ".."]);
        blocking({
            let parent = parent.clone();
            move || fs::create_dir_all(&parent)
        })
        .await
        .map_err(io_error(&parent))?;
        check_abort(cx, Some(&resolved))?;
        let content = content.to_vec();
        blocking({
            let resolved = resolved.clone();
            move || {
                OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&resolved)?
                    .write_all(&content)
            }
        })
        .await
        .map_err(io_error(&resolved))?;
        check_abort(cx, Some(&resolved))
    }

    async fn truncate_file(&self, path: &str, size: u64, cx: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        if size > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "File size must be a non-negative safe integer",
                Some(&resolved),
            ));
        }
        blocking({
            let resolved = resolved.clone();
            move || {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&resolved)?
                    .set_len(size)
            }
        })
        .await
        .map_err(io_error(&resolved))?;
        check_abort(cx, Some(&resolved))
    }

    async fn flush_file(&self, path: &str, cx: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let directory = blocking({
            let resolved = resolved.clone();
            move || {
                let file = OpenOptions::new().read(true).write(true).open(&resolved)?;
                if file.metadata()?.is_dir() {
                    return Ok(true);
                }
                file.sync_all().map(|()| false)
            }
        })
        .await
        .map_err(io_error(&resolved))?;
        if directory {
            return Err(FileError::new(
                FileErrorCode::IsDirectory,
                "Is a directory",
                Some(&resolved),
            ));
        }
        check_abort(cx, Some(&resolved))
    }

    async fn rename_file(&self, source: &str, destination: &str, cx: &Context) -> FileResult<()> {
        let source = self.resolve(source);
        let destination = self.resolve(destination);
        check_abort(cx, Some(&destination))?;
        blocking({
            let source = source.clone();
            move || fs::rename(&source, &destination)
        })
        .await
        .map_err(io_error(&source))
    }

    async fn file_info(&self, path: &str, cx: &Context) -> FileResult<FileInfo> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let metadata = blocking({
            let resolved = resolved.clone();
            move || fs::symlink_metadata(&resolved)
        })
        .await
        .map_err(io_error(&resolved))?;
        file_info(&resolved, &metadata)
    }

    async fn list_dir(&self, path: &str, cx: &Context) -> FileResult<Vec<FileInfo>> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let cx = cx.clone();
        blocking(move || {
            let mut names = fs::read_dir(&resolved)
                .and_then(|entries| {
                    entries
                        .map(|entry| entry.map(|entry| entry.file_name()))
                        .collect::<io::Result<Vec<_>>>()
                })
                .map_err(io_error(&resolved))?;
            // Byte order, as libuv's scandir sorts.
            names.sort();
            let mut infos = Vec::with_capacity(names.len());
            for name in names {
                check_abort(&cx, Some(&resolved))?;
                let entry_path = resolve(&[resolved.as_str(), &name.to_string_lossy()]);
                let metadata = fs::symlink_metadata(&entry_path).map_err(io_error(&entry_path))?;
                if let Ok(info) = file_info(&entry_path, &metadata) {
                    infos.push(info);
                }
            }
            Ok(infos)
        })
        .await
    }

    async fn open_dir_reader(&self, path: &str, cx: &Context) -> FileResult<Box<dyn DirReader>> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        let entries = blocking({
            let resolved = resolved.clone();
            move || fs::read_dir(&resolved)
        })
        .await
        .map_err(io_error(&resolved))?;
        check_abort(cx, Some(&resolved))?;
        Ok(Box::new(LocalDirReader {
            entries: Some(entries),
            path: resolved,
            done: false,
            closed: false,
        }))
    }

    async fn watch(
        &self,
        _targets: &[WatchTarget],
        _on_change: WatchCallback,
        cx: &Context,
    ) -> FileResult<Box<dyn FileWatcher>> {
        check_abort(cx, None)?;
        Err(FileError::new(
            FileErrorCode::NotSupported,
            "LocalEnv does not watch files yet",
            None,
        ))
    }

    async fn canonical_path(&self, path: &str, cx: &Context) -> FileResult<String> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        blocking({
            let resolved = resolved.clone();
            move || fs::canonicalize(&resolved)
        })
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(io_error(&resolved))
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
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        blocking({
            let resolved = resolved.clone();
            move || {
                if options.recursive {
                    fs::create_dir_all(&resolved)
                } else {
                    fs::create_dir(&resolved)
                }
            }
        })
        .await
        .map_err(io_error(&resolved))
    }

    async fn remove(&self, path: &str, options: RemoveOptions, cx: &Context) -> FileResult<()> {
        let resolved = self.resolve(path);
        check_abort(cx, Some(&resolved))?;
        blocking(move || {
            let metadata = match fs::symlink_metadata(&resolved) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound && options.force => {
                    return Ok(());
                }
                Err(error) => return Err(FileError::from_io(&error, &resolved)),
            };
            let removed = if metadata.is_dir() {
                if !options.recursive {
                    // Node's ERR_FS_EISDIR, which has no code of its own.
                    return Err(FileError::new(
                        FileErrorCode::Unknown,
                        format!(
                            "Path is a directory: rm returned EISDIR (is a directory) {resolved}"
                        ),
                        Some(&resolved),
                    ));
                }
                fs::remove_dir_all(&resolved)
            } else {
                fs::remove_file(&resolved)
            };
            removed.map_err(io_error(&resolved))
        })
        .await
    }

    async fn create_temp_dir(&self, prefix: Option<&str>, cx: &Context) -> FileResult<String> {
        check_abort(cx, None)?;
        let prefix = prefix.unwrap_or("tmp-").to_string();
        blocking(move || create_temp_dir_sync(&prefix)).await
    }

    async fn create_temp_file(
        &self,
        options: &TempFileOptions,
        cx: &Context,
    ) -> FileResult<String> {
        check_abort(cx, None)?;
        let prefix = options.prefix.clone().unwrap_or_default();
        let suffix = options.suffix.clone().unwrap_or_default();
        blocking(move || create_temp_file_sync(&prefix, &suffix)).await
    }
}

/// A strict LF line reader over positional reads, decoding as a default
/// `TextDecoder` does.
struct LocalTextLineReader {
    file: Option<Arc<File>>,
    path: String,
    decoder: StreamDecoder,
    offset: u64,
    buffered: String,
    /// How much of `buffered` was returned already.
    consumed: usize,
    ended: bool,
}

#[async_trait]
impl TextLineReader for LocalTextLineReader {
    async fn read_line(&mut self, cx: &Context) -> FileResult<Option<TextLine>> {
        check_abort(cx, Some(&self.path))?;
        let Some(file) = self.file.clone() else {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Text line reader is closed",
                Some(&self.path),
            ));
        };
        loop {
            if let Some(newline) = self.buffered[self.consumed..].find('\n') {
                let end = self.consumed + newline;
                let text = self.buffered[self.consumed..end].to_string();
                self.consumed = end + 1;
                if self.consumed > SCAN_CHUNK {
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
            // Explicit positions let an aborted read be retried without
            // skipping bytes.
            let offset = self.offset;
            let chunk = blocking({
                let file = file.clone();
                move || {
                    let mut chunk = vec![0; SCAN_CHUNK];
                    let read = file.read_at(&mut chunk, offset)?;
                    chunk.truncate(read);
                    Ok::<_, io::Error>(chunk)
                }
            })
            .await
            .map_err(io_error(&self.path))?;
            check_abort(cx, Some(&self.path))?;
            self.offset += chunk.len() as u64;
            if chunk.is_empty() {
                let rest = self.decoder.finish();
                self.buffered.push_str(&rest);
                self.ended = true;
            } else {
                let text = self.decoder.decode(&chunk);
                self.buffered.push_str(&text);
            }
        }
    }

    async fn close(&mut self, _cx: &Context) {
        self.file = None;
        self.buffered.clear();
        self.consumed = 0;
    }
}

fn closed(what: &str, path: &str) -> FileError {
    FileError::new(
        FileErrorCode::Invalid,
        format!("{what} is closed"),
        Some(path),
    )
}

struct LocalBinaryReader {
    file: Option<Arc<File>>,
    path: String,
}

impl LocalBinaryReader {
    fn open_file(&self, cx: &Context) -> FileResult<Arc<File>> {
        check_abort(cx, Some(&self.path))?;
        self.file
            .clone()
            .ok_or_else(|| closed("Binary reader", &self.path))
    }
}

#[async_trait]
impl BinaryReader for LocalBinaryReader {
    async fn info(&mut self, cx: &Context) -> FileResult<FileInfo> {
        let file = self.open_file(cx)?;
        let metadata = blocking(move || file.metadata())
            .await
            .map_err(io_error(&self.path))?;
        file_info(&self.path, &metadata)
    }

    async fn read(&mut self, offset: u64, length: u64, cx: &Context) -> FileResult<Vec<u8>> {
        let file = self.open_file(cx)?;
        if offset > MAX_SAFE_INTEGER || length > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Offset and length must be non-negative safe integers",
                Some(&self.path),
            ));
        }
        let path = self.path.clone();
        let cx = cx.clone();
        blocking(move || {
            let mut bytes = Vec::new();
            while (bytes.len() as u64) < length {
                let want = (length - bytes.len() as u64).min(BINARY_READ_CHUNK) as usize;
                let start = bytes.len();
                bytes.resize(start + want, 0);
                let read = file
                    .read_at(&mut bytes[start..], offset + start as u64)
                    .map_err(io_error(&path))?;
                bytes.truncate(start + read);
                check_abort(&cx, Some(&path))?;
                if read == 0 {
                    break;
                }
            }
            Ok(bytes)
        })
        .await
    }

    async fn scan_lines(&mut self, range: LineRange, cx: &Context) -> FileResult<LineScan> {
        let file = self.open_file(cx)?;
        let mut scanner = LineScanner::new(range).ok_or_else(|| {
            FileError::new(
                FileErrorCode::Invalid,
                "Invalid line range",
                Some(&self.path),
            )
        })?;
        let path = self.path.clone();
        let cx = cx.clone();
        blocking(move || {
            let mut chunk = vec![0; SCAN_CHUNK];
            let mut position = 0u64;
            loop {
                let read = file
                    .read_at(&mut chunk, position)
                    .map_err(io_error(&path))?;
                check_abort(&cx, Some(&path))?;
                if read == 0 {
                    return Ok(scanner.finish());
                }
                scanner.push(&chunk[..read]);
                position += read as u64;
            }
        })
        .await
    }

    async fn close(&mut self, _cx: &Context) {
        self.file = None;
    }
}

struct LocalDirReader {
    entries: Option<fs::ReadDir>,
    path: String,
    done: bool,
    closed: bool,
}

#[async_trait]
impl DirReader for LocalDirReader {
    async fn next(&mut self, max_entries: u64, cx: &Context) -> FileResult<DirPage> {
        check_abort(cx, Some(&self.path))?;
        if self.closed {
            return Err(closed("Directory reader", &self.path));
        }
        if max_entries == 0 || max_entries > MAX_SAFE_INTEGER {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "maxEntries must be a positive safe integer",
                Some(&self.path),
            ));
        }
        if self.done {
            return Ok(DirPage {
                entries: Vec::new(),
                done: true,
            });
        }
        let Some(mut entries) = self.entries.take() else {
            return Err(closed("Directory reader", &self.path));
        };
        let path = self.path.clone();
        let cx = cx.clone();
        let (entries, page) = blocking(move || {
            let mut page = Vec::new();
            let mut done = false;
            let result = loop {
                if page.len() as u64 >= max_entries {
                    break Ok(());
                }
                let entry = match entries.next() {
                    None => {
                        done = true;
                        break Ok(());
                    }
                    Some(Err(error)) => break Err(FileError::from_io(&error, &path)),
                    Some(Ok(entry)) => entry,
                };
                if let Err(error) = check_abort(&cx, Some(&path)) {
                    break Err(error);
                }
                let entry_path = resolve(&[path.as_str(), &entry.file_name().to_string_lossy()]);
                let metadata = match fs::symlink_metadata(&entry_path) {
                    Ok(metadata) => metadata,
                    // Removed since it was listed: not part of the listing.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => break Err(FileError::from_io(&error, &entry_path)),
                };
                if let Ok(info) = file_info(&entry_path, &metadata) {
                    page.push(info);
                }
            };
            (
                entries,
                result.map(|()| DirPage {
                    entries: page,
                    done,
                }),
            )
        })
        .await;
        self.entries = Some(entries);
        let page = page?;
        self.done = page.done;
        Ok(page)
    }

    async fn close(&mut self, _cx: &Context) {
        self.closed = true;
        self.entries = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_names_look_like_upstreams() {
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"));
        let name = random_characters(6);
        assert_eq!(name.len(), 6);
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
