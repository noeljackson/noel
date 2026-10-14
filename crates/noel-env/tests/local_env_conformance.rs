//! `LocalEnv` against upstream's goldens
//! (`tests/fixtures/ts-parity/local-env.json`, made by
//! `scripts/ts-parity/local-env-fixture.ts`): upstream Durable's env
//! conformance cases, ported one for one, make the same assertions here, and
//! each one's actual value must equal what `NodeExecutionEnv` gave.

use std::time::Duration;

use noel_env::{
    AbortController, AbortSignal, BinaryReaderOptions, Context, CreateDirOptions, ExecOptions,
    ExecutionEnv, FileErrorCode, FileInfo, FileSystem, LineRange, LocalEnv, OutputStream,
    OutputWindow, RemoveOptions, Shell, ShellCommand, WatchTarget,
};
use serde::Serialize;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/local-env.json");
const MAKEFILE: &str = include_str!("../../../Makefile");

/// Cases `LocalEnv` does not run, with the reason. Each is in the
/// non-parity register ("Workspaces").
const NOT_RUN: &[(&str, &str)] = &[(
    "watch ",
    "LocalEnv's watch returns not_supported until file watching is built",
)];

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

async fn binary_reader_reads_byte_ranges(env: &LocalEnv, r: &mut Recorder) {
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

async fn binary_reader_scans_lines(env: &LocalEnv, r: &mut Recorder) {
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

async fn binary_reader_keeps_reading_after_rename(env: &LocalEnv, r: &mut Recorder) {
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

async fn binary_reader_refuses(env: &LocalEnv, r: &mut Recorder) {
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
async fn read_all(env: &LocalEnv, path: &str, max_entries: u64) -> (Vec<Vec<FileInfo>>, bool) {
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

async fn directory_reader_pages(env: &LocalEnv, r: &mut Recorder) {
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

async fn directory_reader_reports_the_end(env: &LocalEnv, r: &mut Recorder) {
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

async fn directory_reader_refuses(env: &LocalEnv, r: &mut Recorder) {
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

async fn directory_reader_skips_removed(env: &LocalEnv, r: &mut Recorder) {
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

async fn argv_exec_passes_arguments(env: &LocalEnv, r: &mut Recorder) {
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

async fn exec_reports_streams(env: &LocalEnv, r: &mut Recorder) {
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

async fn argv_exec_honors_cwd(env: &LocalEnv, r: &mut Recorder) {
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

async fn argv_exec_spawn_errors(env: &LocalEnv, r: &mut Recorder) {
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

async fn windowed_exec(env: &LocalEnv, r: &mut Recorder) {
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
        r.ok(check, "a skip is followed by more than the window");
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

async fn argv_exec_timeout_and_abort(env: &LocalEnv, r: &mut Recorder) {
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

async fn binary_reader_symlinks(env: &LocalEnv, r: &mut Recorder) {
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
async fn run_case(name: &str, env: &LocalEnv, r: &mut Recorder) -> bool {
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

/// A fresh, empty, writable directory for one case, removed afterwards.
struct CaseDir(String);

impl CaseDir {
    async fn new() -> Self {
        let dir = LocalEnv::new("/")
            .create_temp_dir(Some("noel-local-env-"), &cx())
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

#[test]
fn the_fixture_is_from_the_pinned_upstream_commit() {
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

#[tokio::test(flavor = "multi_thread")]
async fn local_env_makes_the_observations_node_execution_env_makes() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().expect("cases");
    let mut compared = 0;
    let mut not_run = Vec::new();
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().expect("a case name");
        assert_eq!(case["outcome"], "pass", "{name} failed upstream");
        if let Some((_, reason)) = NOT_RUN.iter().find(|(prefix, _)| name.starts_with(prefix)) {
            // Recorded as an outcome only, and refused here as documented.
            assert_eq!(case["recorded"], "outcome", "{name}");
            let dir = CaseDir::new().await;
            let env = LocalEnv::new(&dir.0);
            let target = WatchTarget {
                path: "AGENTS.md".to_string(),
                ..Default::default()
            };
            let watched = env
                .watch(&[target], std::sync::Arc::new(|_| {}), &cx())
                .await;
            assert_eq!(
                watched.err().map(|error| error.code),
                Some(FileErrorCode::NotSupported),
                "{name}: {reason}"
            );
            not_run.push(name.to_string());
            continue;
        }
        let dir = CaseDir::new().await;
        let env = LocalEnv::new(&dir.0);
        let mut recorder = Recorder {
            cwd: dir.0.clone(),
            log: Vec::new(),
        };
        assert!(
            run_case(name, &env, &mut recorder).await,
            "no port of upstream case {name:?}"
        );
        compared += 1;
        let expected = case["assertions"].as_array().expect("assertions");
        if recorder.log != *expected {
            failures.push(format!(
                "{name}:\n  noel:     {}\n  upstream: {}",
                Value::Array(recorder.log),
                Value::Array(expected.clone())
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(compared, 15, "cases compared");
    assert_eq!(not_run.len(), 9, "watch cases: {not_run:?}");
}
