//! Upstream Durable's env conformance cases (`packages/durable/src/testing/env-conformance.ts`), ported one for
//! one, for any `ExecutionEnv`: `tests/local_env_conformance.rs` runs them against `LocalEnv`, and
//! `crates/noel-env-daemon/tests/remote_env_conformance.rs` against `RemoteEnv` over a daemon.
//!
//! The cases recorded with their assertions in `tests/fixtures/ts-parity/local-env.json` (made by
//! `scripts/ts-parity/local-env-fixture.ts` from `NodeExecutionEnv`) make the same assertions here, and each one's
//! actual value must equal what `NodeExecutionEnv` gave. The watch cases are recorded by outcome only: an
//! environment that watches must pass them, one that does not must refuse with `not_supported`. The fixture has no
//! `listFiles` cases (`NodeExecutionEnv` has no `listFiles`), so those ports assert upstream's expected values.
//!
//! Ported from upstream pi's `packages/durable/src/testing/env-conformance.ts` (<https://github.com/earendil-works/pi>,
//! commit `1692bba`), Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use noel_env::{
    AbortController, AbortSignal, BinaryReaderOptions, Context, CreateDirOptions, ExecOptions,
    ExecutionEnv, FileErrorCode, FileInfo, FileSystem, FileWatcher, LineRange, ListFilesOptions,
    LocalEnv, OutputStream, OutputWindow, RemoveOptions, ShellCommand, WatchChange, WatchExclude,
    WatchTarget, MAX_SAFE_INTEGER,
};
use serde::Serialize;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../../tests/fixtures/ts-parity/local-env.json");
const MAKEFILE: &str = include_str!("../../../../Makefile");

fn cx() -> Context {
    Context::background()
}

fn aborted() -> Context {
    Context::with_abort_signal(AbortSignal::already_aborted())
}

fn shell() -> Vec<String> {
    vec!["sh".to_string(), "-c".to_string()]
}

fn argv(script: &str, rest: &[&str]) -> ShellCommand {
    let mut argv = shell();
    argv.push(script.to_string());
    argv.extend(rest.iter().map(|arg| arg.to_string()));
    ShellCommand::Argv(argv)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

fn code<T, E: HasCode>(result: Result<T, E>) -> Value {
    match result {
        Ok(_) => Value::Null,
        Err(error) => Value::from(error.code()),
    }
}

trait HasCode {
    fn code(&self) -> String;
}

impl HasCode for noel_env::FileError {
    fn code(&self) -> String {
        self.code.to_string()
    }
}

impl HasCode for noel_env::ExecutionError {
    fn code(&self) -> String {
        self.code.to_string()
    }
}

/// The assertions of upstream's suite, recorded as the fixture records them.
struct Recorder {
    cwd: String,
    log: Vec<Value>,
}

impl Recorder {
    fn value(&self, value: impl Serialize) -> Value {
        let value = serde_json::to_value(value).expect("a JSON value");
        self.placeholders(value)
    }

    fn placeholders(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace(&self.cwd, "<cwd>")),
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| self.placeholders(item))
                    .collect(),
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(key, field)| {
                        let field = if key == "mtimeMs" && field.is_number() {
                            json!("<mtime>")
                        } else {
                            self.placeholders(field)
                        };
                        (key, field)
                    })
                    .collect(),
            ),
            other => other,
        }
    }

    fn record(
        &mut self,
        assert: &str,
        actual: Value,
        expected: Option<Value>,
        message: Option<&str>,
    ) {
        let mut entry = json!({"assert": assert, "actual": actual});
        if let Some(expected) = expected {
            entry["expected"] = expected;
        }
        if let Some(message) = message {
            entry["message"] = json!(message);
        }
        self.log.push(entry);
    }

    fn ok(&mut self, value: bool, message: &str) {
        self.record("ok", json!(value), None, Some(message));
    }

    fn strict_equal(&mut self, actual: impl Serialize, expected: impl Serialize) {
        let (actual, expected) = (self.value(actual), self.value(expected));
        self.record("strictEqual", actual, Some(expected), None);
    }

    fn deep_equal(&mut self, actual: impl Serialize, expected: impl Serialize) {
        let (actual, expected) = (self.value(actual), self.value(expected));
        self.record("deepEqual", actual, Some(expected), None);
    }

    fn partial_deep_equal(&mut self, actual: impl Serialize, expected: Value) {
        let actual = self.value(actual);
        self.record("partialDeepEqual", actual, Some(expected), None);
    }
}

/// `execCollect`: a command's exit and its output per stream.
async fn exec_collect(
    env: &dyn ExecutionEnv,
    command: ShellCommand,
    cwd: Option<&str>,
) -> (
    Result<noel_env::ExecResult, noel_env::ExecutionError>,
    String,
    String,
) {
    let (mut stdout, mut stderr) = (String::new(), String::new());
    let mut on_output = |chunk: noel_env::OutputChunk<'_>| {
        match chunk.stream {
            OutputStream::Stdout => stdout.push_str(chunk.text),
            OutputStream::Stderr => stderr.push_str(chunk.text),
        }
        Ok(())
    };
    let result = env
        .exec(
            command,
            ExecOptions {
                cwd: cwd.map(str::to_string),
                on_output: Some(&mut on_output),
                ..Default::default()
            },
            &cx(),
        )
        .await;
    (result, stdout, stderr)
}

async fn binary_reader_reads_byte_ranges(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.write_file("data.txt", b"hello world", &cx())
        .await
        .unwrap();
    let mut reader = env
        .open_binary_reader("data.txt", BinaryReaderOptions::default(), &cx())
        .await
        .unwrap();
    let info = reader.info(&cx()).await.unwrap();
    r.partial_deep_equal(
        info,
        json!({"name": "data.txt", "kind": "file", "size": 11}),
    );
    r.strict_equal(text(reader.read(0, 5, &cx()).await.unwrap()), "hello");
    r.strict_equal(text(reader.read(6, 100, &cx()).await.unwrap()), "world");
    r.strict_equal(reader.read(11, 4, &cx()).await.unwrap().len(), 0);
    r.strict_equal(reader.read(50, 1, &cx()).await.unwrap().len(), 0);
    r.strict_equal(reader.read(3, 0, &cx()).await.unwrap().len(), 0);
    // Upstream reads at offset -1 and length 1.5, which unsigned integers
    // cannot hold; the nearest values that are not safe integers either.
    r.strict_equal(code(reader.read(u64::MAX, 1, &cx()).await), "invalid");
    r.strict_equal(code(reader.read(0, 1 << 53, &cx()).await), "invalid");
    r.strict_equal(code(reader.read(0, 1, &aborted()).await), "aborted");
    reader.close(&cx()).await;
    reader.close(&cx()).await;
    r.strict_equal(code(reader.read(0, 1, &cx()).await), "invalid");
    r.strict_equal(code(reader.info(&cx()).await), "invalid");
}

/// `new TextDecoder("utf-8", {ignoreBOM}).decode(bytes)`.
fn decode(bytes: &[u8], keep_bom: bool) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    match text.strip_prefix('\u{feff}') {
        Some(rest) if !keep_bom => rest.to_string(),
        _ => text,
    }
}

async fn binary_reader_scans_lines(env: &dyn ExecutionEnv, r: &mut Recorder) {
    // A byte-order mark, an invalid sequence before a newline, an empty
    // line, a later U+FEFF, and no final newline.
    let bytes: &[u8] = &[
        0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a, 0xc3,
        0xa9,
    ];
    env.write_file("lines.txt", bytes, &cx()).await.unwrap();
    let decoded = decode(bytes, false);
    let lines = decoded.split('\n').collect::<Vec<_>>();
    let mut reader = env
        .open_binary_reader("lines.txt", BinaryReaderOptions::default(), &cx())
        .await
        .unwrap();
    let range = |from: u64, to: u64| decode(&bytes[from as usize..to as usize], from > 0);
    for (start_line, end_line) in [
        (0, None),
        (0, Some(1)),
        (1, Some(3)),
        (2, Some(3)),
        (3, None),
        (4, Some(9)),
    ] {
        let scan = reader
            .scan_lines(
                LineRange {
                    start_line,
                    end_line,
                },
                &cx(),
            )
            .await
            .unwrap();
        let end = end_line.map_or(lines.len(), |end| (end as usize).min(lines.len()));
        let selected = lines[start_line as usize..end].join("\n");
        r.strict_equal(scan.newlines, lines.len() - 1);
        r.strict_equal(range(scan.start, scan.end), &selected);
        r.strict_equal(scan.selected_bytes, selected.len());
        r.strict_equal(
            range(scan.start, scan.first_line_end),
            lines[start_line as usize],
        );
        r.strict_equal(scan.first_line_bytes, lines[start_line as usize].len());
    }
    let past = reader
        .scan_lines(
            LineRange {
                start_line: 9,
                end_line: None,
            },
            &cx(),
        )
        .await
        .unwrap();
    let size = bytes.len();
    r.partial_deep_equal(
        past,
        json!({"start": size, "end": size, "selectedBytes": 0}),
    );
    let empty = LineRange {
        start_line: 2,
        end_line: Some(2),
    };
    r.strict_equal(code(reader.scan_lines(empty, &cx()).await), "invalid");
    reader.close(&cx()).await;
}

async fn binary_reader_keeps_reading_after_rename(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.write_file("a.txt", b"one", &cx()).await.unwrap();
    let mut reader = env
        .open_binary_reader("a.txt", BinaryReaderOptions::default(), &cx())
        .await
        .unwrap();
    env.rename_file("a.txt", "b.txt", &cx()).await.unwrap();
    env.write_file("a.txt", b"two", &cx()).await.unwrap();
    r.strict_equal(text(reader.read(0, 10, &cx()).await.unwrap()), "one");
    reader.close(&cx()).await;
}

async fn binary_reader_refuses(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.create_dir("dir", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    env.write_file("file.txt", b"x", &cx()).await.unwrap();
    let options = BinaryReaderOptions::default();
    r.strict_equal(
        code(env.open_binary_reader("dir", options, &cx()).await),
        "is_directory",
    );
    r.strict_equal(
        code(env.open_binary_reader("missing.txt", options, &cx()).await),
        "not_found",
    );
    r.strict_equal(
        code(
            env.open_binary_reader("file.txt", options, &aborted())
                .await,
        ),
        "aborted",
    );
}

/// `readAll`: every page of a directory reader.
async fn read_all(
    env: &dyn ExecutionEnv,
    path: &str,
    max_entries: u64,
) -> (Vec<Vec<FileInfo>>, bool) {
    let mut reader = env.open_dir_reader(path, &cx()).await.unwrap();
    let mut pages = Vec::new();
    let mut done = false;
    for _ in 0..1000 {
        let next = reader.next(max_entries, &cx()).await.unwrap();
        pages.push(next.entries);
        if next.done {
            done = true;
            break;
        }
    }
    reader.close(&cx()).await;
    (pages, done)
}

async fn directory_reader_pages(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for name in names {
        env.write_file(name, name.as_bytes(), &cx()).await.unwrap();
    }
    env.create_dir("sub", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    let (pages, done) = read_all(env, ".", 2).await;
    r.ok(done, "directory reader reached the end");
    for page in &pages {
        r.ok(page.len() <= 2, "page within maxEntries");
    }
    let entries = pages.into_iter().flatten().collect::<Vec<_>>();
    let mut listed = entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>();
    listed.sort();
    let mut expected = names
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    expected.push("sub".to_string());
    expected.sort();
    r.deep_equal(listed, expected);
    let find = |name: &str| entries.iter().find(|entry| entry.name == name);
    r.strict_equal(find("sub").map(|entry| entry.kind), "directory");
    r.partial_deep_equal(find("a.txt"), json!({"kind": "file", "size": 5}));
}

async fn directory_reader_reports_the_end(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.create_dir("empty", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    let mut reader = env.open_dir_reader("empty", &cx()).await.unwrap();
    let end = json!({"entries": [], "done": true});
    r.deep_equal(reader.next(10, &cx()).await.unwrap(), &end);
    r.deep_equal(reader.next(10, &cx()).await.unwrap(), &end);
    r.strict_equal(code(reader.next(0, &cx()).await), "invalid");
    r.strict_equal(code(reader.next(1, &aborted()).await), "aborted");
    reader.close(&cx()).await;
    reader.close(&cx()).await;
    r.strict_equal(code(reader.next(1, &cx()).await), "invalid");
}

async fn directory_reader_refuses(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.write_file("file.txt", b"x", &cx()).await.unwrap();
    r.strict_equal(
        code(env.open_dir_reader("missing", &cx()).await),
        "not_found",
    );
    r.strict_equal(
        code(env.open_dir_reader("file.txt", &cx()).await),
        "not_directory",
    );
    r.strict_equal(code(env.open_dir_reader(".", &aborted()).await), "aborted");
}

async fn directory_reader_skips_removed(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.create_dir("dir", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    for name in ["x", "y", "z"] {
        env.write_file(&format!("dir/{name}"), name.as_bytes(), &cx())
            .await
            .unwrap();
    }
    let mut reader = env.open_dir_reader("dir", &cx()).await.unwrap();
    for name in ["x", "y", "z"] {
        env.remove(&format!("dir/{name}"), RemoveOptions::default(), &cx())
            .await
            .unwrap();
    }
    let mut entries = Vec::new();
    for _ in 0..10 {
        let next = reader.next(10, &cx()).await.unwrap();
        entries.extend(next.entries);
        if next.done {
            break;
        }
    }
    r.deep_equal(entries, json!([]));
    reader.close(&cx()).await;
}

async fn argv_exec_passes_arguments(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let hostile = "it's $(touch pwned) `touch pwned` *; touch pwned";
    let (result, stdout, _) = exec_collect(
        env,
        argv(r#"printf "%s|%s" "$1" "$2""#, &["argv0", hostile, "a b"]),
        None,
    )
    .await;
    r.strict_equal(result.unwrap().exit_code, 0);
    r.strict_equal(stdout, format!("{hostile}|a b"));
    r.strict_equal(env.exists("pwned", &cx()).await.unwrap(), false);
}

async fn exec_reports_streams(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let script = "printf out; printf err >&2; printf more";
    let (result, stdout, stderr) = exec_collect(env, argv(script, &[]), None).await;
    r.strict_equal(result.unwrap().exit_code, 0);
    r.strict_equal(stdout, "outmore");
    r.strict_equal(stderr, "err");
    let (result, stdout, stderr) = exec_collect(env, script.into(), None).await;
    r.strict_equal(result.unwrap().exit_code, 0);
    r.strict_equal(stdout, "outmore");
    r.strict_equal(stderr, "err");
}

async fn argv_exec_honors_cwd(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.create_dir("sub", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    let (result, _, _) =
        exec_collect(env, argv("printf x > made.txt; exit 3", &[]), Some("sub")).await;
    r.strict_equal(result.unwrap().exit_code, 3);
    r.strict_equal(
        env.read_text_file("sub/made.txt", &cx()).await.unwrap(),
        "x",
    );
}

async fn argv_exec_spawn_errors(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let missing = ShellCommand::Argv(vec!["pi-durable-conformance-missing-program".to_string()]);
    r.strict_equal(
        code(env.exec(missing, ExecOptions::default(), &cx()).await),
        "spawn_error",
    );
    let empty = ShellCommand::Argv(Vec::new());
    r.strict_equal(
        code(env.exec(empty, ExecOptions::default(), &cx()).await),
        "spawn_error",
    );
}

async fn windowed_exec(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let lines = 2000;
    let window = OutputWindow {
        max_bytes: 200,
        max_lines: 5,
        min_interval: Duration::ZERO,
        bytes_per_second: 1e9,
    };
    let (mut bytes, mut newlines, mut tail) = (0u64, 0u64, String::new());
    let mut skip_checks = Vec::new();
    let mut on_output = |chunk: noel_env::OutputChunk<'_>| {
        if let Some(skipped) = chunk.skipped {
            bytes += skipped.bytes;
            newlines += skipped.newlines;
            let after_newlines = chunk.text.matches('\n').count() as u64;
            skip_checks.push(
                chunk.text.len() as u64 > window.max_bytes || after_newlines > window.max_lines,
            );
            tail.clear();
        }
        bytes += chunk.text.len() as u64;
        newlines += chunk.text.matches('\n').count() as u64;
        tail.push_str(chunk.text);
        Ok(())
    };
    let result = env
        .exec(
            argv(
                &format!("i=0; while [ $i -lt {lines} ]; do echo line-$i; i=$((i+1)); done"),
                &[],
            ),
            ExecOptions {
                window: Some(window),
                on_output: Some(&mut on_output),
                ..Default::default()
            },
            &cx(),
        )
        .await;
    for check in skip_checks {
        r.ok(check, SKIP_CHECK);
    }
    r.strict_equal(result.unwrap().exit_code, 0);
    let expected = (0..lines)
        .map(|index| format!("line-{index}\n"))
        .collect::<Vec<_>>();
    r.strict_equal(bytes, expected.concat().len());
    r.strict_equal(newlines, lines);
    r.ok(
        tail.ends_with(&expected[expected.len() - window.max_lines as usize..].concat()),
        "the delivered output ends with the tail",
    );
}

async fn argv_exec_timeout_and_abort(env: &dyn ExecutionEnv, r: &mut Recorder) {
    let timed_out = env
        .exec(
            argv("sleep 2", &[]),
            ExecOptions {
                timeout: Some(Duration::from_millis(100)),
                ..Default::default()
            },
            &cx(),
        )
        .await;
    r.strict_equal(code(timed_out), "timeout");
    let controller = AbortController::new();
    let context = Context::with_abort_signal(controller.signal());
    let running = env.exec(argv("sleep 2", &[]), ExecOptions::default(), &context);
    let abort = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        controller.abort();
    };
    let (result, ()) = tokio::join!(running, abort);
    r.strict_equal(code(result), "aborted");
}

async fn binary_reader_symlinks(env: &dyn ExecutionEnv, r: &mut Recorder) {
    env.write_file("target.txt", b"target", &cx())
        .await
        .unwrap();
    env.create_dir("sub", CreateDirOptions::default(), &cx())
        .await
        .unwrap();
    env.write_file("sub/inner.txt", b"inner", &cx())
        .await
        .unwrap();
    let linked = env
        .exec(
            argv("ln -s target.txt link.txt && ln -s sub dirlink", &[]),
            ExecOptions::default(),
            &cx(),
        )
        .await;
    r.strict_equal(linked.unwrap().exit_code, 0);
    let follow = BinaryReaderOptions::default();
    let no_follow = BinaryReaderOptions { no_follow: true };
    let mut followed = env
        .open_binary_reader("link.txt", follow, &cx())
        .await
        .unwrap();
    r.strict_equal(text(followed.read(0, 10, &cx()).await.unwrap()), "target");
    followed.close(&cx()).await;
    r.strict_equal(
        code(env.open_binary_reader("link.txt", no_follow, &cx()).await),
        "invalid",
    );
    // Only the final component is refused; earlier symlinked directories
    // still resolve.
    let mut inner = env
        .open_binary_reader("dirlink/inner.txt", no_follow, &cx())
        .await
        .unwrap();
    r.strict_equal(text(inner.read(0, 10, &cx()).await.unwrap()), "inner");
    inner.close(&cx()).await;
}

/// Runs the port of the case `name`; false when there is none.
async fn run_case(name: &str, env: &dyn ExecutionEnv, r: &mut Recorder) -> bool {
    match name {
        "binary reader reads byte ranges of the opened file" => {
            binary_reader_reads_byte_ranges(env, r).await
        }
        "binary reader scans lines like decoding the whole file" => {
            binary_reader_scans_lines(env, r).await
        }
        "binary reader keeps reading the file it opened after a rename" => {
            binary_reader_keeps_reading_after_rename(env, r).await
        }
        "binary reader refuses directories, missing files and aborted opens" => {
            binary_reader_refuses(env, r).await
        }
        "directory reader pages every entry exactly once" => directory_reader_pages(env, r).await,
        "directory reader reports the end and refuses use after close" => {
            directory_reader_reports_the_end(env, r).await
        }
        "directory reader refuses missing paths and files" => {
            directory_reader_refuses(env, r).await
        }
        "directory reader skips entries removed during enumeration" => {
            directory_reader_skips_removed(env, r).await
        }
        "argv exec passes arguments to the program without shell parsing" => {
            argv_exec_passes_arguments(env, r).await
        }
        "exec reports the stream of every chunk in both forms" => {
            exec_reports_streams(env, r).await
        }
        "argv exec honors cwd and exit codes" => argv_exec_honors_cwd(env, r).await,
        "argv exec reports missing programs and empty argv as spawn errors" => {
            argv_exec_spawn_errors(env, r).await
        }
        "windowed exec keeps the exact tail and counts what it skips" => {
            windowed_exec(env, r).await
        }
        "argv exec distinguishes timeout from abort" => argv_exec_timeout_and_abort(env, r).await,
        "binary reader follows symlinks unless noFollow refuses the final one" => {
            binary_reader_symlinks(env, r).await
        }
        _ => return false,
    }
    true
}

/// `getOrThrow` for the cases that report a failure as an error.
fn ok<T, E: std::fmt::Debug>(result: Result<T, E>, what: &str) -> Result<T, String> {
    result.map_err(|error| format!("{what}: {error:?}"))
}

fn check(condition: bool, what: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("failed: {what}"))
    }
}

fn target(path: &str) -> WatchTarget {
    WatchTarget {
        path: path.to_string(),
        ..WatchTarget::default()
    }
}

fn recursive(path: &str) -> WatchTarget {
    WatchTarget {
        recursive: true,
        ..target(path)
    }
}

/// Whether a change reports `path`: an overflow, or a reported path at or above it.
fn covers(change: &WatchChange, path: &str) -> bool {
    match change {
        WatchChange::Overflow => true,
        WatchChange::Paths(paths) => paths
            .iter()
            .any(|reported| path == reported || path.starts_with(&format!("{reported}/"))),
        WatchChange::Error(_) => false,
    }
}

/// Upstream's `watching`: a watcher whose changes are collected, and a wait for a change that reports a path.
struct Watching<'a> {
    env: &'a dyn ExecutionEnv,
    changes: Arc<Mutex<Vec<WatchChange>>>,
    watcher: Box<dyn FileWatcher>,
}

impl<'a> Watching<'a> {
    async fn start(env: &'a dyn ExecutionEnv, targets: &[WatchTarget]) -> Result<Self, String> {
        let changes = Arc::new(Mutex::new(Vec::new()));
        let sink = changes.clone();
        let watcher = ok(
            env.watch(
                targets,
                Arc::new(move |change| sink.lock().unwrap().push(change)),
                &cx(),
            )
            .await,
            "watch",
        )?;
        Ok(Self {
            env,
            changes,
            watcher,
        })
    }

    async fn absolute(&self, path: &str) -> String {
        self.env.absolute_path(path, &cx()).await.unwrap()
    }

    fn mark(&self) -> usize {
        self.changes.lock().unwrap().len()
    }

    /// Waits up to three seconds for a change after `from` that reports `path`.
    async fn expect(&self, path: &str, from: usize) -> Result<(), String> {
        let target = self.absolute(path).await;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            {
                let changes = self.changes.lock().unwrap();
                if changes[from..].iter().any(|change| covers(change, &target)) {
                    return Ok(());
                }
                if Instant::now() > deadline {
                    return Err(format!(
                        "No change reported {target}; got {:?}",
                        &changes[from..]
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn close(mut self) {
        self.watcher.close(&cx()).await;
    }
}

/// Runs `body` with a watcher of `targets`, closing it afterwards.
macro_rules! watching {
    ($env:expr, $targets:expr, |$w:ident| $body:block) => {{
        let $w = Watching::start($env, $targets).await?;
        let result: Result<(), String> = async { $body }.await;
        $w.close().await;
        result
    }};
}

async fn write(env: &dyn ExecutionEnv, path: &str, content: &str) -> Result<(), String> {
    ok(env.write_file(path, content.as_bytes(), &cx()).await, path)
}

async fn watch_missing_file(env: &dyn ExecutionEnv) -> Result<(), String> {
    watching!(env, &[target("AGENTS.md")], |w| {
        let from = w.mark();
        write(env, "AGENTS.md", "one").await?;
        w.expect("AGENTS.md", from).await?;
        let from = w.mark();
        write(env, "AGENTS.md", "two!").await?;
        w.expect("AGENTS.md", from).await?;
        // Editors replace a file by renaming a new one over it.
        let from = w.mark();
        write(env, "AGENTS.md.tmp", "three").await?;
        ok(
            env.rename_file("AGENTS.md.tmp", "AGENTS.md", &cx()).await,
            "rename",
        )?;
        w.expect("AGENTS.md", from).await?;
        let from = w.mark();
        write(env, "AGENTS.md", "four").await?;
        w.expect("AGENTS.md", from).await?;
        let from = w.mark();
        ok(
            env.remove("AGENTS.md", RemoveOptions::default(), &cx())
                .await,
            "remove",
        )?;
        w.expect("AGENTS.md", from).await
    })
}

async fn watch_missing_ancestors(env: &dyn ExecutionEnv) -> Result<(), String> {
    watching!(env, &[target("a/b/c/AGENTS.md")], |w| {
        let from = w.mark();
        write(env, "a/b/c/AGENTS.md", "x").await?;
        w.expect("a/b/c/AGENTS.md", from).await
    })
}

async fn watch_new_directories(env: &dyn ExecutionEnv) -> Result<(), String> {
    ok(
        env.create_dir("skills", CreateDirOptions::default(), &cx())
            .await,
        "mkdir",
    )?;
    watching!(env, &[recursive("skills")], |w| {
        // Written before any watcher on the new directories can exist.
        let from = w.mark();
        write(env, "skills/a/b/SKILL.md", "one").await?;
        w.expect("skills/a/b/SKILL.md", from).await?;
        let from = w.mark();
        write(env, "skills/a/b/SKILL.md", "two!").await?;
        w.expect("skills/a/b/SKILL.md", from).await?;
        let from = w.mark();
        write(env, "skills/a/b/c/SKILL.md", "deeper").await?;
        w.expect("skills/a/b/c/SKILL.md", from).await
    })
}

async fn watch_renamed_parent(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "proj/.pi/skills/x.md", "x").await?;
    watching!(env, &[recursive("proj/.pi/skills")], |w| {
        let from = w.mark();
        ok(
            env.rename_file("proj/.pi", "proj/old", &cx()).await,
            "rename",
        )?;
        w.expect("proj/.pi/skills", from).await?;
        let from = w.mark();
        write(env, "proj/.pi/skills/y.md", "y").await?;
        w.expect("proj/.pi/skills/y.md", from).await?;
        let from = w.mark();
        write(env, "proj/.pi/skills/y.md", "yy").await?;
        w.expect("proj/.pi/skills/y.md", from).await
    })
}

async fn watch_excluded(env: &dyn ExecutionEnv) -> Result<(), String> {
    ok(
        env.create_dir("skills", CreateDirOptions::default(), &cx())
            .await,
        "mkdir",
    )?;
    let targets = [WatchTarget {
        exclude: Some(WatchExclude {
            hidden: true,
            names: vec!["node_modules".to_string()],
        }),
        ..recursive("skills")
    }];
    watching!(env, &targets, |w| {
        write(env, "skills/node_modules/dep/SKILL.md", "dep").await?;
        write(env, "skills/.SKILL.md.tmp", "draft").await?;
        let from = w.mark();
        ok(
            env.rename_file("skills/.SKILL.md.tmp", "skills/SKILL.md", &cx())
                .await,
            "rename",
        )?;
        w.expect("skills/SKILL.md", from).await?;
        let hidden = [
            w.absolute("skills/node_modules").await,
            w.absolute("skills/.SKILL.md.tmp").await,
        ];
        for change in w.changes.lock().unwrap().iter() {
            if let WatchChange::Paths(paths) = change {
                for path in paths {
                    check(
                        !hidden
                            .iter()
                            .any(|excluded| path.starts_with(excluded.as_str())),
                        &format!("excluded {path}"),
                    )?;
                }
            }
        }
        Ok(())
    })
}

async fn watch_overlapping_targets(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "skills/a/one.md", "one").await?;
    watching!(env, &[target("skills"), recursive("skills")], |w| {
        let from = w.mark();
        write(env, "skills/a/two.md", "two").await?;
        w.expect("skills/a/two.md", from).await
    })
}

async fn watch_replaced_directory(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "skills/a/x.md", "x").await?;
    watching!(env, &[recursive("skills")], |w| {
        let from = w.mark();
        ok(
            env.rename_file("skills/a", "skills-old", &cx()).await,
            "rename",
        )?;
        ok(
            env.create_dir("skills/a", CreateDirOptions::default(), &cx())
                .await,
            "mkdir",
        )?;
        w.expect("skills/a", from).await?;
        let from = w.mark();
        write(env, "skills/a/y.md", "y").await?;
        w.expect("skills/a/y.md", from).await?;
        let from = w.mark();
        write(env, "skills/a/y.md", "yy").await?;
        w.expect("skills/a/y.md", from).await
    })
}

async fn watch_stops_once_closed(env: &dyn ExecutionEnv) -> Result<(), String> {
    let changes = Arc::new(Mutex::new(Vec::new()));
    let sink = changes.clone();
    let mut watcher = ok(
        env.watch(
            &[target("file.txt")],
            Arc::new(move |change| sink.lock().unwrap().push(change)),
            &cx(),
        )
        .await,
        "watch",
    )?;
    // `mode()` is `Native` or `Polling` by its type.
    let _ = watcher.mode();
    watcher.close(&cx()).await;
    watcher.close(&cx()).await;
    write(env, "file.txt", "x").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let empty = changes.lock().unwrap().is_empty();
    check(empty, "no change after close")
}

async fn watch_symlink_target(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "data/real.md", "one").await?;
    ok(
        env.create_dir("config", CreateDirOptions::default(), &cx())
            .await,
        "mkdir",
    )?;
    let linked = env
        .exec(
            argv("ln -s ../data/real.md config/AGENTS.md", &[]),
            ExecOptions::default(),
            &cx(),
        )
        .await;
    check(ok(linked, "ln")?.exit_code == 0, "ln -s")?;
    watching!(env, &[target("config/AGENTS.md")], |w| {
        let from = w.mark();
        write(env, "data/real.md", "two!").await?;
        w.expect("config/AGENTS.md", from).await
    })
}

/// Runs the port of the watch case `name`; `None` when there is none.
async fn run_watch_case(name: &str, env: &dyn ExecutionEnv) -> Option<Result<(), String>> {
    Some(match name {
        "watch reports a missing file's creation, changes, replacement and removal" => {
            watch_missing_file(env).await
        }
        "watch reports a missing target whose ancestors are created" => {
            watch_missing_ancestors(env).await
        }
        "watch follows directories created together with their contents" => {
            watch_new_directories(env).await
        }
        "watch keeps watching a path whose parent is renamed and recreated" => {
            watch_renamed_parent(env).await
        }
        "watch skips excluded entries and reports a rename out of them" => {
            watch_excluded(env).await
        }
        "watch keeps recursive coverage where a non-recursive target overlaps" => {
            watch_overlapping_targets(env).await
        }
        "watch follows a directory replaced at the same path" => {
            watch_replaced_directory(env).await
        }
        "watch stops reporting once closed" => watch_stops_once_closed(env).await,
        "watch reports changes to the file a watched symbolic link points to" => {
            watch_symlink_target(env).await
        }
        _ => return None,
    })
}

/// A listing as `<kind letter> <path>` lines (`f`, `d`, `l`), with its truncation.
async fn listed(
    env: &dyn ExecutionEnv,
    path: &str,
    options: ListFilesOptions,
) -> Result<(Vec<String>, bool), String> {
    let listing = ok(env.list_files(path, &options, &cx()).await, "listFiles")?;
    let entries = listing
        .entries
        .iter()
        .map(|entry| {
            let letter = match entry.kind {
                noel_env::FileKind::File => "f",
                noel_env::FileKind::Directory => "d",
                noel_env::FileKind::Symlink => "l",
            };
            format!("{letter} {}", entry.path)
        })
        .collect();
    Ok((entries, listing.truncated))
}

fn lines(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| entry.to_string()).collect()
}

fn equal<T: PartialEq + std::fmt::Debug>(actual: T, expected: T) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("expected {expected:?}, got {actual:?}"))
    }
}

async fn write_all(env: &dyn ExecutionEnv, files: &[&str]) -> Result<(), String> {
    for file in files {
        write(env, file, "").await?;
    }
    Ok(())
}

fn list_options() -> ListFilesOptions {
    ListFilesOptions::default()
}

async fn list_files_order(env: &dyn ExecutionEnv) -> Result<(), String> {
    write_all(
        env,
        &["b.txt", "a/z.txt", "a/b/c.txt", ".hidden/x", "A.txt"],
    )
    .await?;
    ok(
        env.create_dir("empty", CreateDirOptions::default(), &cx())
            .await,
        "mkdir",
    )?;
    equal(
        listed(env, ".", list_options()).await?,
        (
            lines(&[
                "d .hidden",
                "f .hidden/x",
                "f A.txt",
                "d a",
                "d a/b",
                "f a/b/c.txt",
                "f a/z.txt",
                "f b.txt",
                "d empty",
            ]),
            false,
        ),
    )?;
    let hidden = ListFilesOptions {
        hidden: Some(false),
        ..list_options()
    };
    equal(
        listed(env, ".", hidden).await?.0,
        lines(&[
            "f A.txt",
            "d a",
            "d a/b",
            "f a/b/c.txt",
            "f a/z.txt",
            "f b.txt",
            "d empty",
        ]),
    )?;
    let expected = (lines(&["d b", "f b/c.txt", "f z.txt"]), false);
    equal(listed(env, "a", list_options()).await?, expected.clone())?;
    let absolute = ok(env.absolute_path("a", &cx()).await, "absolutePath")?;
    equal(listed(env, &absolute, list_options()).await?, expected)?;
    equal(
        listed(env, "empty", list_options()).await?,
        (Vec::new(), false),
    )
}

async fn list_files_gitignore(env: &dyn ExecutionEnv) -> Result<(), String> {
    write_all(
        env,
        &[
            "keep.txt",
            "out/x.js",
            "sub/keep.txt",
            "sub/drop.log",
            "node_modules/m/i.js",
        ],
    )
    .await?;
    write(env, ".gitignore", "out/\n*.log\n").await?;
    write(env, "sub/.gitignore", "!keep.txt\n").await?;
    // Not a work tree yet: `.gitignore` files have no effect.
    check(
        listed(env, ".", list_options())
            .await?
            .0
            .contains(&"f out/x.js".to_string()),
        "out/x.js outside a work tree",
    )?;
    write(env, ".git/HEAD", "ref: refs/heads/main\n").await?;
    write(env, ".git/info/exclude", "keep.txt\n").await?;
    let excluded = ListFilesOptions {
        exclude: vec!["node_modules".to_string()],
        ..list_options()
    };
    equal(
        listed(env, ".", excluded).await?.0,
        lines(&[
            "f .gitignore",
            "d sub",
            "f sub/.gitignore",
            "f sub/keep.txt",
        ]),
    )?;
    // A directory inside the work tree applies its parents' rules.
    equal(
        listed(env, "sub", list_options()).await?.0,
        lines(&["f .gitignore", "f keep.txt"]),
    )?;
    let all = listed(
        env,
        ".",
        ListFilesOptions {
            gitignore: Some(false),
            ..list_options()
        },
    )
    .await?
    .0;
    check(
        ["f out/x.js", "f node_modules/m/i.js", "f keep.txt"]
            .iter()
            .all(|entry| all.contains(&entry.to_string())),
        "everything without gitignore",
    )?;
    check(
        !all.iter()
            .any(|entry| entry.contains(".git/") || entry == "d .git"),
        "no .git",
    )
}

async fn list_files_bounds(env: &dyn ExecutionEnv) -> Result<(), String> {
    write_all(env, &["a", "b", "c"]).await?;
    let bounded = |max_entries: Option<u64>, max_bytes: Option<u64>| ListFilesOptions {
        max_entries,
        max_bytes,
        ..list_options()
    };
    equal(
        listed(env, ".", bounded(Some(2), None)).await?,
        (lines(&["f a", "f b"]), true),
    )?;
    equal(
        listed(env, ".", bounded(Some(3), None)).await?,
        (lines(&["f a", "f b", "f c"]), false),
    )?;
    // Each entry counts its path plus two bytes.
    equal(
        listed(env, ".", bounded(None, Some(6))).await?,
        (lines(&["f a", "f b"]), true),
    )?;
    equal(
        listed(env, ".", bounded(Some(0), None)).await?,
        (Vec::new(), true),
    )?;
    // Upstream passes -1 and 1.5; unsigned integers and durations cannot, so the nearest values that are not safe
    // integers either.
    let code = |result: Result<noel_env::FileListing, noel_env::FileError>| {
        result.err().map(|error| error.code)
    };
    equal(
        code(
            env.list_files(".", &bounded(Some(MAX_SAFE_INTEGER + 1), None), &cx())
                .await,
        ),
        Some(FileErrorCode::Invalid),
    )?;
    let fractional = ListFilesOptions {
        timeout: Some(Duration::from_micros(1500)),
        ..list_options()
    };
    equal(
        code(env.list_files(".", &fractional, &cx()).await),
        Some(FileErrorCode::Invalid),
    )
}

async fn list_files_failures(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "file.txt", "").await?;
    let code = |result: Result<noel_env::FileListing, noel_env::FileError>| {
        result.err().map(|error| error.code)
    };
    equal(
        code(env.list_files("missing", &list_options(), &cx()).await),
        Some(FileErrorCode::NotFound),
    )?;
    equal(
        code(env.list_files("file.txt", &list_options(), &cx()).await),
        Some(FileErrorCode::NotDirectory),
    )?;
    equal(
        code(env.list_files(".", &list_options(), &aborted()).await),
        Some(FileErrorCode::Aborted),
    )
}

async fn list_files_symlinks(env: &dyn ExecutionEnv) -> Result<(), String> {
    write(env, "dir/inner.txt", "").await?;
    let linked = env
        .exec(
            argv(
                "ln -s dir link && ln -s missing dangling && ln -s .. dir/loop",
                &[],
            ),
            ExecOptions::default(),
            &cx(),
        )
        .await;
    check(ok(linked, "ln")?.exit_code == 0, "ln -s")?;
    equal(
        listed(env, ".", list_options()).await?.0,
        lines(&[
            "l dangling",
            "d dir",
            "f dir/inner.txt",
            "l dir/loop",
            "l link",
        ]),
    )?;
    // The listed directory itself may be a link.
    equal(
        listed(env, "link", list_options()).await?.0,
        lines(&["f inner.txt", "l loop"]),
    )
}

/// Upstream's `listFiles` cases, for environments that list files (`listFiles: true`), in the suite's order.
pub const LIST_FILES_CASES: &[&str] = &[
    "listFiles lists files and directories depth first in name order",
    "listFiles skips what git ignores in work trees, .git, and excluded names",
    "listFiles stops at its bounds and reports truncated",
    "listFiles fails on missing directories, files, and aborts",
    "listFiles lists symbolic links without following them",
];

async fn run_list_files_case(name: &str, env: &dyn ExecutionEnv) -> Result<(), String> {
    match name {
        "listFiles lists files and directories depth first in name order" => {
            list_files_order(env).await
        }
        "listFiles skips what git ignores in work trees, .git, and excluded names" => {
            list_files_gitignore(env).await
        }
        "listFiles stops at its bounds and reports truncated" => list_files_bounds(env).await,
        "listFiles fails on missing directories, files, and aborts" => {
            list_files_failures(env).await
        }
        "listFiles lists symbolic links without following them" => list_files_symlinks(env).await,
        other => Err(format!("no port of {other:?}")),
    }
}

/// A fresh, empty, writable directory for one case, removed afterwards.
pub struct CaseDir(pub String);

impl CaseDir {
    pub async fn new() -> Self {
        let dir = LocalEnv::new("/")
            .create_temp_dir(Some("noel-env-conformance-"), &cx())
            .await
            .expect("a temporary directory");
        Self(dir)
    }
}

impl Drop for CaseDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the fixture is JSON")
}

/// The fixture is from the commit the Makefile pins, made with the options the ports assume.
pub fn check_fixture_source() {
    let fixture = fixture();
    let pin = MAKEFILE
        .lines()
        .find_map(|line| line.strip_prefix("CHORD_REFERENCE_REF ?= "))
        .expect("the Makefile pins the reference");
    assert_eq!(
        fixture["source"]["ref"], pin,
        "regenerate local-env.json after moving the pin"
    );
    assert_eq!(fixture["options"]["shell"], json!(shell()));
    assert_eq!(fixture["options"]["listFiles"], json!(false));
}

/// Whether the environment under test watches files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watches {
    /// Its `watch` must fail with `not_supported`.
    Refused,
    /// The watch cases run and must pass, as they did upstream.
    Run,
}

/// What a run of the fixture's cases found.
#[derive(Debug, Default)]
pub struct Report {
    /// Cases whose assertions were compared with the fixture's.
    pub compared: usize,
    /// Watch cases run and passed.
    pub watched: usize,
    /// Watch cases refused with `not_supported`.
    pub refused: usize,
    pub failures: Vec<String>,
}

impl Report {
    pub fn assert(&self, compared: usize, watched: usize, refused: usize) {
        assert!(self.failures.is_empty(), "{}", self.failures.join("\n"));
        assert_eq!(
            (self.compared, self.watched, self.refused),
            (compared, watched, refused),
            "cases compared, watched and refused"
        );
    }
}

/// The windowed exec case asserts once per skip that more than the window follows it. `NodeExecutionEnv`, which
/// made the recording, and `LocalEnv` never skip; a daemon skips when output outruns the window, so how many of these
/// checks run depends on timing. Each must pass; how many there are is not compared.
const SKIP_CHECK: &str = "a skip is followed by more than the window";

fn without_passed_skip_checks(log: &[Value]) -> Vec<Value> {
    log.iter()
        .filter(|entry| !(entry["message"] == SKIP_CHECK && entry["actual"] == true))
        .cloned()
        .collect()
}

/// Runs every case of the fixture against environments `make` creates at a fresh directory each.
pub async fn run_fixture_cases(
    make: &dyn Fn(&str) -> Box<dyn ExecutionEnv>,
    watches: Watches,
) -> Report {
    let fixture = fixture();
    let mut report = Report::default();
    for case in fixture["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("a case name");
        assert_eq!(case["outcome"], "pass", "{name} failed upstream");
        let dir = CaseDir::new().await;
        let env = make(&dir.0);
        if name.starts_with("watch ") {
            // Recorded as an outcome only.
            assert_eq!(case["recorded"], "outcome", "{name}");
            match watches {
                Watches::Refused => {
                    let watched = env
                        .watch(&[target("AGENTS.md")], Arc::new(|_| {}), &cx())
                        .await;
                    if watched.err().map(|error| error.code) == Some(FileErrorCode::NotSupported) {
                        report.refused += 1;
                    } else {
                        report
                            .failures
                            .push(format!("{name}: watch was not refused"));
                    }
                }
                Watches::Run => match run_watch_case(name, env.as_ref()).await {
                    Some(Ok(())) => report.watched += 1,
                    Some(Err(error)) => report.failures.push(format!("{name}: {error}")),
                    None => report
                        .failures
                        .push(format!("no port of upstream case {name:?}")),
                },
            }
            env.cleanup(&cx()).await;
            continue;
        }
        let mut recorder = Recorder {
            cwd: dir.0.clone(),
            log: Vec::new(),
        };
        assert!(
            run_case(name, env.as_ref(), &mut recorder).await,
            "no port of upstream case {name:?}"
        );
        env.cleanup(&cx()).await;
        report.compared += 1;
        let expected = case["assertions"].as_array().expect("assertions");
        if without_passed_skip_checks(&recorder.log) != without_passed_skip_checks(expected) {
            report.failures.push(format!(
                "{name}:\n  noel:     {}\n  upstream: {}",
                Value::Array(recorder.log),
                Value::Array(expected.clone())
            ));
        }
    }
    report
}

/// Runs upstream's `listFiles` cases; the failures, if any.
pub async fn run_list_files_cases(make: &dyn Fn(&str) -> Box<dyn ExecutionEnv>) -> Vec<String> {
    let mut failures = Vec::new();
    for name in LIST_FILES_CASES {
        let dir = CaseDir::new().await;
        let env = make(&dir.0);
        if let Err(error) = run_list_files_case(name, env.as_ref()).await {
            failures.push(format!("{name}: {error}"));
        }
    }
    failures
}
