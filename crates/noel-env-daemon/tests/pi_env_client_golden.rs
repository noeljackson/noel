//! noel's client against upstream's goldens (`tests/fixtures/ts-parity/pi-env.json`): upstream's `Connection` and
//! `RemoteExecutionEnv` ran each scenario against upstream's daemon through a tap, and recorded the frames it sent.
//! Here `RemoteEnv` runs the same calls against a daemon through a tap of its own (`tee`), and every frame it sends
//! must equal the recorded one: type, id, JSON (objects compare by field, with the scenario's directory as
//! `<root>`) and payload. Pings are kept only in the liveness scenario, as the recording keeps them.
//!
//! The `pty` scenario is not replayed: noel's `ExecutionEnv` has no terminals, so the client has no `openTerminal`
//! (non-parity register, "noel-env Client").

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use noel_env::frame::{read_frame, Frame, PING};
use noel_env::{
    AbortController, BinaryReaderOptions, Connection, Context, CreateDirOptions, ExecOptions,
    ExecutionErrorCode, FileSystem, LineRange, ListFilesOptions, OutputChunk, RemoteEnv,
    RemoteEnvOptions, RemoveOptions, RequestOptions, Shell, ShellCommand, WatchChange, WatchTarget,
};
use serde_json::Value;
use support::{daemon, Scratch};

const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/pi-env.json");

fn cx() -> Context {
    Context::background()
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "request",
        2 => "result",
        3 => "error",
        4 => "event",
        5 => "cancel",
        6 => "ping",
        _ => "unknown",
    }
}

/// A payload as the recorder wrote it: `{utf8}`, `{hex}`, or `{repeat, length}`.
fn payload(value: &Value) -> Vec<u8> {
    if value.is_null() {
        return Vec::new();
    }
    if let Some(text) = value["utf8"].as_str() {
        return text.as_bytes().to_vec();
    }
    if let Some(hex) = value["hex"].as_str() {
        return (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect();
    }
    let unit = value["repeat"].as_str().unwrap().as_bytes();
    let length = value["length"].as_u64().unwrap() as usize;
    unit.iter().copied().cycle().take(length).collect()
}

fn replaced(value: &Value, from: &str, to: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(from, to)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| replaced(item, from, to)).collect())
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, field)| (key.clone(), replaced(field, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A sent frame as compared: type name, id, JSON, payload.
type Sent = (String, u32, Value, Vec<u8>);

fn recorded_sends(name: &str) -> Vec<Sent> {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scenario| scenario["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    scenario["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|step| step.get("send"))
        .map(|send| {
            (
                send["type"].as_str().unwrap().to_string(),
                send["id"].as_u64().unwrap() as u32,
                send["json"].clone(),
                payload(&send["payload"]),
            )
        })
        .collect()
}

/// A connection whose daemon's input is also written to `log`.
fn tapped(log: &Path) -> Connection {
    Connection::command(vec![
        "sh".into(),
        "-c".into(),
        r#"log=$1; daemon=$2; shift 2; tee "$log" | "$daemon" "$@""#.into(),
        "tap".into(),
        log.to_string_lossy().into_owned(),
        daemon(),
    ])
}

/// The frames noel's client sent, as the recording keeps them.
fn sends(log: &Path, root: &str, keep_pings: bool) -> Vec<Sent> {
    let bytes = std::fs::read(log).unwrap();
    let mut input = bytes.as_slice();
    let mut frames = Vec::new();
    let mut pinged = false;
    while let Some(Frame {
        kind,
        id,
        json,
        payload,
    }) = read_frame(&mut input).unwrap()
    {
        if kind == PING {
            if !keep_pings || pinged {
                continue;
            }
            pinged = true;
        }
        frames.push((
            kind_name(kind).to_string(),
            id,
            replaced(&json, root, "<root>"),
            payload,
        ));
    }
    frames
}

fn compare(name: &str, noel: Vec<Sent>) {
    let upstream = recorded_sends(name);
    for (index, (noel, upstream)) in noel.iter().zip(&upstream).enumerate() {
        assert_eq!(noel, upstream, "{name}: frame {index}");
    }
    assert_eq!(noel.len(), upstream.len(), "{name}: frames sent");
}

/// Runs `scenario` on a tapped connection in a fresh directory and compares what the client sent.
async fn replay<F>(name: &str, keep_pings: bool, scenario: F)
where
    F: AsyncFnOnce(&RemoteEnv, &Connection, &str),
{
    let root = Scratch::new(&format!("client-golden-{name}"));
    let logs = Scratch::new(&format!("client-golden-log-{name}"));
    let log = logs.join("client.log");
    let connection = tapped(&log);
    let env = RemoteEnv::new(RemoteEnvOptions::new(
        connection.clone(),
        "pi-env:golden",
        root.text(),
    ));
    connection.info().await.unwrap();
    scenario(&env, &connection, &root.text()).await;
    connection.close();
    // The tap writes what it reads at once; give the daemon a moment to see the end of its input.
    tokio::time::sleep(Duration::from_millis(300)).await;
    compare(name, sends(&log, &root.text(), keep_pings));
}

#[tokio::test(flavor = "multi_thread")]
async fn files() {
    replay("files", false, async |env, connection, _root| {
        env.write_file("a.txt", b"hello\nworld\n", &cx())
            .await
            .unwrap();
        env.file_info("a.txt", &cx()).await.unwrap();
        assert_eq!(
            env.read_binary_file("a.txt", &cx()).await.unwrap(),
            b"hello\nworld\n"
        );
        // More than one 512 KiB write: `write` keeps the file open for `writeChunk`.
        let big = "0123456789abcdef\n".repeat(35_295)[..600_000].to_string();
        env.write_file("big.txt", big.as_bytes(), &cx())
            .await
            .unwrap();
        let mut reader = env
            .open_binary_reader("big.txt", BinaryReaderOptions::default(), &cx())
            .await
            .unwrap();
        assert_eq!(
            reader.read(524_280, 16, &cx()).await.unwrap(),
            big.as_bytes()[524_280..524_296]
        );
        reader.info(&cx()).await.unwrap();
        let range = LineRange {
            start_line: 30_000,
            end_line: Some(30_002),
        };
        reader.scan_lines(range, &cx()).await.unwrap();
        reader.close(&cx()).await;
        let recursive = CreateDirOptions { recursive: true };
        env.create_dir("d/e", recursive, &cx()).await.unwrap();
        env.write_file("d/e/f.txt", b"f", &cx()).await.unwrap();
        env.list_dir(".", &cx()).await.unwrap();
        env.rename_file("d/e/f.txt", "d/g.txt", &cx())
            .await
            .unwrap();
        env.truncate_file("d/g.txt", 0, &cx()).await.unwrap();
        env.flush_file("a.txt", &cx()).await.unwrap();
        env.canonical_path("d/../a.txt", &cx()).await.unwrap();
        env.list_files(".", &ListFilesOptions::default(), &cx())
            .await
            .unwrap();
        let remove = RemoveOptions {
            recursive: true,
            force: false,
        };
        env.remove("d/e", remove, &cx()).await.unwrap();
        // Errors.
        assert!(env.file_info("missing", &cx()).await.is_err());
        assert!(env.read_binary_file("d", &cx()).await.is_err());
        assert!(env
            .open_binary_reader("d", BinaryReaderOptions::default(), &cx())
            .await
            .is_err());
        assert!(env.flush_file("d", &cx()).await.is_err());
        assert!(env
            .remove("d", RemoveOptions::default(), &cx())
            .await
            .is_err());
        let bare = || RequestOptions::default();
        assert!(connection
            .request("bogus", Default::default(), bare())
            .await
            .is_err());
        assert!(connection
            .request("lstat", Default::default(), bare())
            .await
            .is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exec() {
    replay("exec", false, async |env, connection, root| {
        let output = Mutex::new(String::new());
        let mut on_output = |chunk: OutputChunk<'_>| {
            output.lock().unwrap().push_str(chunk.text);
            Ok(())
        };
        let done = env
            .exec(
                "echo out; echo err >&2".into(),
                ExecOptions {
                    on_output: Some(&mut on_output),
                    ..ExecOptions::default()
                },
                &cx(),
            )
            .await
            .unwrap();
        assert_eq!(done.exit_code, 0);
        assert!(output.lock().unwrap().contains("out\n"));
        let argv =
            |argv: &[&str]| ShellCommand::Argv(argv.iter().map(|arg| arg.to_string()).collect());
        let exited = env
            .exec(
                argv(&["sh", "-c", r"printf 'a\377b'; exit 3"]),
                ExecOptions {
                    on_output: Some(&mut on_output),
                    ..ExecOptions::default()
                },
                &cx(),
            )
            .await
            .unwrap();
        assert_eq!(exited.exit_code, 3);
        // Abort while it runs: a cancel.
        let controller = AbortController::new();
        let context = Context::with_abort_signal(controller.signal());
        let sleeping = env.exec(argv(&["sleep", "30"]), ExecOptions::default(), &context);
        let abort = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            controller.abort();
        };
        let (aborted, ()) = tokio::join!(sleeping, abort);
        assert_eq!(aborted.unwrap_err().code, ExecutionErrorCode::Aborted);
        // cleanup(): a cancel with mode kill; the command settles with its status.
        let background = cx();
        let killed = env.exec(argv(&["sleep", "30"]), ExecOptions::default(), &background);
        let cleanup = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            env.cleanup(&cx()).await;
        };
        let (settled, ()) = tokio::join!(killed, cleanup);
        assert_eq!(settled.unwrap().exit_code, 137);
        let timed_out = env
            .exec(
                argv(&["sleep", "5"]),
                ExecOptions {
                    timeout: Some(Duration::from_millis(300)),
                    ..ExecOptions::default()
                },
                &cx(),
            )
            .await;
        assert_eq!(timed_out.unwrap_err().code, ExecutionErrorCode::Timeout);
        let missing_cwd = env
            .exec(
                "true".into(),
                ExecOptions {
                    cwd: Some("missing".to_string()),
                    ..ExecOptions::default()
                },
                &cx(),
            )
            .await;
        assert_eq!(
            missing_cwd.unwrap_err().code,
            ExecutionErrorCode::SpawnError
        );
        let missing_program = env
            .exec(
                argv(&["/nonexistent/program"]),
                ExecOptions::default(),
                &cx(),
            )
            .await;
        assert_eq!(
            missing_program.unwrap_err().code,
            ExecutionErrorCode::SpawnError
        );
        let no_shell = RemoteEnv::new(RemoteEnvOptions {
            shell_path: Some("/nonexistent/sh".to_string()),
            ..RemoteEnvOptions::new(connection.clone(), "pi-env:golden", root)
        });
        let unavailable = no_shell
            .exec("true".into(), ExecOptions::default(), &cx())
            .await;
        assert_eq!(
            unavailable.unwrap_err().code,
            ExecutionErrorCode::ShellUnavailable
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch() {
    replay("watch", false, async |env, _connection, root| {
        let written = format!("{root}/w.txt");
        let (seen, mut changed) = tokio::sync::watch::channel(false);
        let seen = Arc::new(seen);
        let on_change = {
            let seen = seen.clone();
            Arc::new(move |change: WatchChange| {
                if let WatchChange::Paths(paths) = change {
                    if paths.contains(&written) {
                        seen.send_replace(true);
                    }
                }
            })
        };
        let target = WatchTarget {
            path: ".".to_string(),
            recursive: true,
            exclude: None,
        };
        let mut watcher = env.watch(&[target], on_change, &cx()).await.unwrap();
        env.write_file("w.txt", b"w", &cx()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), changed.wait_for(|seen| *seen))
            .await
            .expect("a change within 5 s")
            .unwrap();
        watcher.close(&cx()).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn liveness() {
    replay("liveness", true, async |_env, _connection, _root| {
        tokio::time::sleep(Duration::from_millis(5_500)).await;
    })
    .await;
}

#[test]
fn the_pty_scenario_is_the_only_one_not_replayed() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let names: Vec<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scenario| scenario["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["files", "exec", "pty", "watch", "liveness"]);
}
