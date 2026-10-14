//! `LocalEnv`'s shell: commands as `NodeExecutionEnv` runs them
//! (`packages/env/docs/semantics.md`, "Commands"), started with noel's
//! process code: a new session (`setsid`), stdin from `/dev/null`, and the
//! whole process group killed on timeout, abort, or when the future is
//! dropped.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::ChildStdout;
use tokio::time::Instant;

use crate::local::create_temp_file_sync;
use crate::process::{kill_group, spawn_tool_process};
use crate::{
    Context, ExecOptions, ExecResult, ExecutionError, ExecutionErrorCode, LocalEnv, OutputCallback,
    OutputChunk, OutputStream, Shell, ShellCommand, SpillOptions, StreamDecoder,
};

/// Largest timeout, a 32-bit millisecond timer as Node's.
const MAX_TIMEOUT: Duration = Duration::from_millis(2_147_483_647);
/// After the process exits, output still arriving (a background job holds
/// the pipes) is collected until it pauses this long.
const EXIT_OUTPUT_GRACE: Duration = Duration::from_millis(100);
const READ_CHUNK: usize = 64 * 1024;

impl LocalEnv {
    /// The shell for string commands (`getShellConfig`): the configured
    /// path, else `/bin/bash`, else `bash` on `PATH`, else `sh`.
    fn shell(&self) -> Result<PathBuf, ExecutionError> {
        if let Some(shell_path) = &self.shell_path {
            return if Path::new(shell_path).exists() {
                Ok(PathBuf::from(shell_path))
            } else {
                Err(ExecutionError::new(
                    ExecutionErrorCode::ShellUnavailable,
                    format!("Custom shell path not found: {shell_path}"),
                ))
            };
        }
        let bin_bash = Path::new("/bin/bash");
        if bin_bash.exists() {
            return Ok(bin_bash.to_path_buf());
        }
        Ok(std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join("bash"))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap_or_else(|| PathBuf::from("sh")))
    }
}

/// Reads the next chunk of a pipe; `None` once it is closed. A closed pipe
/// never completes, so `select!` keeps waiting on its other branches.
async fn read_chunk<R: AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    buffer: &mut [u8],
) -> Option<usize> {
    match pipe {
        Some(open) => match open.read(buffer).await {
            Ok(read) if read > 0 => Some(read),
            _ => None,
        },
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The complete raw output in a temporary file, once it crosses a
/// threshold. Counted over raw chunks in arrival order, as Node counts.
struct Spill {
    options: SpillOptions,
    prefix: Vec<Vec<u8>>,
    seen_bytes: u64,
    seen_newlines: u64,
    file: Option<(String, File)>,
    error: Option<String>,
}

impl Spill {
    fn new(options: SpillOptions) -> Self {
        Self {
            options,
            prefix: Vec::new(),
            seen_bytes: 0,
            seen_newlines: 0,
            file: None,
            error: None,
        }
    }

    /// False once the spill failed: the command must be stopped.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        if self.error.is_some() || chunk.is_empty() {
            return self.error.is_none();
        }
        if self.file.is_none() {
            self.seen_bytes += chunk.len() as u64;
            self.seen_newlines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
            let lines = self.seen_newlines + u64::from(chunk.last() != Some(&b'\n'));
            if self.seen_bytes <= self.options.after_bytes && lines <= self.options.after_lines {
                self.prefix.push(chunk.to_vec());
                return true;
            }
            match create_temp_file_sync("pi-output-", ".log")
                .map_err(|error| error.message)
                .and_then(|path| {
                    File::options()
                        .append(true)
                        .open(&path)
                        .map(|file| (path, file))
                        .map_err(|error| error.to_string())
                }) {
                Ok(file) => self.file = Some(file),
                Err(message) => return self.fail(message),
            }
            let prefix = std::mem::take(&mut self.prefix);
            for earlier in prefix {
                if !self.write(&earlier) {
                    return false;
                }
            }
        }
        self.write(chunk)
    }

    fn write(&mut self, chunk: &[u8]) -> bool {
        let Some((_, file)) = &mut self.file else {
            return true;
        };
        match file.write_all(chunk) {
            Ok(()) => true,
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn fail(&mut self, message: String) -> bool {
        self.error.get_or_insert(message);
        false
    }

    fn finish(&mut self) -> Option<String> {
        if let Some((_, file)) = &mut self.file {
            if let Err(error) = file.flush() {
                self.fail(error.to_string());
            }
        }
        self.file.as_ref().map(|(path, _)| path.clone())
    }
}

/// Output on its way to the caller: one decoder per stream, so a character
/// split across chunks of one stream survives interleaving.
struct Output<'a> {
    on_output: Option<OutputCallback<'a>>,
    stdout: StreamDecoder,
    stderr: StreamDecoder,
    callback_error: Option<String>,
    spill: Option<Spill>,
}

impl Output<'_> {
    /// Hands a chunk on. False when the command must be stopped: the
    /// callback or the spill failed.
    fn deliver(&mut self, stream: OutputStream, bytes: &[u8]) -> bool {
        let decoder = match stream {
            OutputStream::Stdout => &mut self.stdout,
            OutputStream::Stderr => &mut self.stderr,
        };
        let text = decoder.decode(bytes);
        let delivered = self.emit(stream, &text, bytes);
        let spilled = self.spill.as_mut().is_none_or(|spill| spill.feed(bytes));
        delivered && spilled
    }

    fn emit(&mut self, stream: OutputStream, text: &str, raw: &[u8]) -> bool {
        if self.callback_error.is_some() {
            return false;
        }
        let Some(on_output) = self.on_output.as_mut() else {
            return true;
        };
        if text.is_empty() && raw.is_empty() {
            return true;
        }
        match on_output(OutputChunk {
            text,
            stream,
            skipped: None,
            raw: Some(raw),
        }) {
            Ok(()) => true,
            Err(message) => {
                self.callback_error = Some(message);
                false
            }
        }
    }

    /// The end of both streams: an incomplete character becomes U+FFFD.
    fn finish(&mut self) {
        let stdout = self.stdout.finish();
        self.emit(OutputStream::Stdout, &stdout, &[]);
        let stderr = self.stderr.finish();
        self.emit(OutputStream::Stderr, &stderr, &[]);
    }
}

/// Removes a command's process group from its environment's running set.
struct Running {
    running: Arc<std::sync::Mutex<std::collections::BTreeSet<i32>>>,
    pgid: Option<i32>,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            self.running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&pgid);
        }
    }
}

#[async_trait]
impl Shell for LocalEnv {
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
        let cwd = match &options.cwd {
            Some(cwd) => self.resolve(cwd),
            None => self.cwd.clone(),
        };
        // A string runs through the shell; an argv list runs its program
        // directly, without shell parsing.
        let (program, args) = match command {
            ShellCommand::Script(script) => (self.shell()?, vec!["-c".to_string(), script]),
            ShellCommand::Argv(argv) => {
                let mut argv = argv.into_iter();
                let Some(program) = argv.next() else {
                    return Err(ExecutionError::new(
                        ExecutionErrorCode::SpawnError,
                        "Empty argv: no program to run",
                    ));
                };
                (PathBuf::from(program), argv.collect())
            }
        };
        if !Path::new(&cwd).exists() {
            return Err(ExecutionError::new(
                ExecutionErrorCode::SpawnError,
                format!("Working directory does not exist: {cwd}\nCannot execute bash commands."),
            ));
        }

        let mut process = tokio::process::Command::new(program);
        process
            .args(&args)
            .current_dir(&cwd)
            // Commands are non-interactive: stdin is empty and the session
            // has no controlling terminal, so nothing reads noel's input.
            .stdin(Stdio::null());
        if !options.inherit_env {
            process.env_clear();
        }
        for (name, value) in &options.env {
            match value {
                Some(value) => process.env(name, value),
                None => process.env_remove(name),
            };
        }
        let spawn_error = |error: io::Error| ExecutionError::spawn(&error);
        // With merged output both streams write one pipe, in order.
        let merged_reader = if options.merge_stderr {
            let (reader, writer) = std::io::pipe().map_err(spawn_error)?;
            process
                .stdout(writer.try_clone().map_err(spawn_error)?)
                .stderr(writer);
            Some(reader)
        } else {
            process.stdout(Stdio::piped()).stderr(Stdio::piped());
            None
        };
        // The command leads its own process group, so a timeout, an abort,
        // or dropping this future kills the whole tree.
        let (mut child, group) = spawn_tool_process(&mut process).map_err(spawn_error)?;
        // The builder holds the merged pipe's write ends; the pipe closes
        // once the command's processes are its only writers.
        drop(process);
        let _running = Running {
            running: self.running.clone(),
            pgid: group.pgid(),
        };
        if let Some(pgid) = group.pgid() {
            self.running().insert(pgid);
        }
        let mut group = Some(group);
        let (mut stdout, mut stderr) = match merged_reader {
            Some(reader) => (
                Some(
                    ChildStdout::from_std(std::process::ChildStdout::from(OwnedFd::from(reader)))
                        .map_err(spawn_error)?,
                ),
                None,
            ),
            None => (child.stdout.take(), child.stderr.take()),
        };

        let mut output = Output {
            on_output: options.on_output,
            stdout: StreamDecoder::new(),
            stderr: StreamDecoder::new(),
            callback_error: None,
            spill: options.spill.map(Spill::new),
        };
        let mut stdout_buffer = vec![0; READ_CHUNK];
        let mut stderr_buffer = vec![0; READ_CHUNK];
        let mut deadline = options
            .timeout
            .and_then(|timeout| Instant::now().checked_add(timeout));
        let mut timed_out = false;
        let mut abort_seen = false;
        let mut status = None;
        let mut grace = None;
        while status.is_none() || stdout.is_some() || stderr.is_some() {
            let mut stop = false;
            tokio::select! {
                read = read_chunk(&mut stdout, &mut stdout_buffer) => match read {
                    Some(read) => stop = !output.deliver(OutputStream::Stdout, &stdout_buffer[..read]),
                    None => stdout = None,
                },
                read = read_chunk(&mut stderr, &mut stderr_buffer) => match read {
                    Some(read) => stop = !output.deliver(OutputStream::Stderr, &stderr_buffer[..read]),
                    None => stderr = None,
                },
                exit = child.wait(), if status.is_none() => {
                    status = Some(exit.map_err(spawn_error)?);
                }
                () = sleep_until(deadline) => {
                    timed_out = true;
                    deadline = None;
                    drop(group.take());
                }
                () = cx.aborted(), if !abort_seen => {
                    abort_seen = true;
                    drop(group.take());
                }
                () = sleep_until(grace) => break,
            }
            if stop {
                drop(group.take());
            }
            if status.is_some() {
                grace = Some(Instant::now() + EXIT_OUTPUT_GRACE);
            }
        }
        // The pipes close when the command settles, even if a descendant
        // keeps them open. A command that ran to its end leaves the
        // background jobs it started running.
        drop((stdout, stderr));
        if let Some(group) = group {
            group.finish();
        }
        output.finish();
        let spill_path = output.spill.as_mut().and_then(Spill::finish);
        let spill_error = output.spill.as_ref().and_then(|spill| spill.error.clone());

        if let Some(message) = output.callback_error {
            return Err(ExecutionError::new(
                ExecutionErrorCode::CallbackError,
                message,
            ));
        }
        let interrupted = if timed_out {
            Some(ExecutionError::new(
                ExecutionErrorCode::Timeout,
                format!(
                    "timeout:{}",
                    options.timeout.unwrap_or_default().as_secs_f64()
                ),
            ))
        } else if cx.is_aborted() {
            Some(ExecutionError::aborted())
        } else {
            None
        };
        if let Some(interrupted) = interrupted {
            return Err(ExecutionError {
                spill_path,
                ..interrupted
            });
        }
        if let Some(message) = spill_error {
            return Err(ExecutionError::new(
                ExecutionErrorCode::Unknown,
                format!("Failed to preserve complete shell output: {message}"),
            ));
        }
        // A process killed by a signal has no exit code: 128 + the signal.
        let exit_code = status
            .and_then(|status| {
                status.code().or_else(|| {
                    std::os::unix::process::ExitStatusExt::signal(&status)
                        .map(|signal| 128 + signal)
                })
            })
            .unwrap_or(1);
        Ok(ExecResult {
            exit_code,
            spill_path,
        })
    }

    async fn cleanup(&self, _cx: &Context) {
        let groups = std::mem::take(&mut *self.running());
        for pgid in groups {
            kill_group(pgid);
        }
    }
}
