//! `noel-env serve` against upstream's goldens (`tests/fixtures/ts-parity/pi-env.json`, made by
//! `scripts/ts-parity/pi-env-fixture.ts`): upstream pi-env's client talked to upstream's daemon, and a tap recorded
//! the frames both ways. Each scenario here starts `noel-env serve`, sends the client's frames in the recorded order
//! (each after the daemon frames that came before it in the recording), and compares what the daemon answers:
//!
//! - results, errors and events other than `output` and `change`, per request, in order and exactly, after the
//!   normalization the recorder applied (`<root>`, masked machine fields, sorted `readdir` entries);
//! - `output` events merged per request and stream, since how output is split into events depends on timing;
//! - `change` events by the paths they report, since how many a write causes depends on the native watcher;
//! - pings, where the recording kept one, by their bytes.

use noel_env::frame::{self, Frame, CANCEL, ERROR, EVENT, PING, REQUEST, RESULT};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/pi-env.json");
const MAKEFILE: &str = include_str!("../../../Makefile");
/// What the recorder masks in daemon frames (`MASKED` in pi-env-fixture.ts).
const MASKED: &[&str] = &[
    "version",
    "os",
    "arch",
    "home",
    "tmpdir",
    "cwd",
    "pid",
    "dev",
    "ino",
    "mtimeSec",
    "mtimeNsec",
];
/// How long the daemon may take to send what the recording shows.
const PATIENCE: Duration = Duration::from_secs(30);

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("pi-env.json parses")
}

fn kind_of(name: &str) -> u8 {
    match name {
        "request" => REQUEST,
        "result" => RESULT,
        "error" => ERROR,
        "event" => EVENT,
        "cancel" => CANCEL,
        "ping" => PING,
        other => panic!("unknown frame type {other}"),
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
    let unit = value["repeat"]
        .as_str()
        .expect("a payload encoding")
        .as_bytes();
    let length = value["length"].as_u64().unwrap() as usize;
    unit.iter().copied().cycle().take(length).collect()
}

/// Every string of `value` with `from` replaced by `to`.
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

/// The recorder's normalization of daemon JSON (`normalized` in pi-env-fixture.ts).
fn normalized(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalized).collect()),
        Value::Object(object) => {
            let mut result = Map::new();
            for (key, field) in object {
                let field = if MASKED.contains(&key.as_str()) {
                    Value::String(format!("<{key}>"))
                } else {
                    normalized(field)
                };
                result.insert(key.clone(), field);
            }
            if result.get("kind") == Some(&Value::from("directory")) && result.contains_key("size")
            {
                result.insert("size".into(), Value::from("<size>"));
            }
            if let Some(Value::Array(entries)) = result.get_mut("entries") {
                entries.sort_by(|a, b| {
                    let name =
                        |entry: &Value| entry["name"].as_str().unwrap_or_default().to_string();
                    name(a).as_bytes().cmp(name(b).as_bytes())
                });
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// A daemon frame as compared: its type, JSON and payload.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    kind: u8,
    json: Value,
    payload: String,
}

impl Seen {
    fn new(kind: u8, json: Value, payload: &[u8]) -> Seen {
        Seen {
            kind,
            json,
            payload: String::from_utf8_lossy(payload).into_owned(),
        }
    }
}

/// What the daemon sent, or what the recording says it sends, sorted as compared.
#[derive(Debug, Default)]
struct Answers {
    /// Results, errors and events other than output and change, per request, in order.
    frames: BTreeMap<u32, Vec<Seen>>,
    /// Output bytes per request and stream (`""` for a terminal).
    output: BTreeMap<(u32, String), Vec<u8>>,
    /// Paths `change` events reported, per request, and how many events there were.
    changes: BTreeMap<u32, (BTreeSet<String>, usize)>,
    pings: Vec<Seen>,
}

impl Answers {
    /// Note a daemon frame whose JSON is already normalized.
    fn add(&mut self, kind: u8, id: u32, json: Value, payload: &[u8]) {
        if kind == PING {
            self.pings.push(Seen::new(kind, json, payload));
            return;
        }
        if kind == EVENT && json["kind"] == "output" {
            let stream = json["stream"].as_str().unwrap_or_default().to_string();
            self.output
                .entry((id, stream))
                .or_default()
                .extend_from_slice(payload);
            return;
        }
        if kind == EVENT && json["kind"] == "change" {
            let (paths, events) = self.changes.entry(id).or_default();
            *events += 1;
            match json["paths"].as_array() {
                Some(reported) => paths.extend(
                    reported
                        .iter()
                        .filter_map(|path| path.as_str().map(str::to_string)),
                ),
                None => {
                    paths.insert(json.to_string());
                }
            }
            return;
        }
        self.frames
            .entry(id)
            .or_default()
            .push(Seen::new(kind, json, payload));
    }

    /// Whether everything in `expected` has arrived, pings only with `pings`. Output is not waited for: a request's
    /// result follows its output.
    fn covers(&self, expected: &Answers, pings: bool) -> bool {
        let frames = expected.frames.iter().all(|(id, frames)| {
            self.frames
                .get(id)
                .is_some_and(|seen| seen.len() >= frames.len())
        });
        let changes = expected.changes.iter().all(|(id, (paths, _))| {
            self.changes
                .get(id)
                .is_some_and(|(seen, _)| seen.is_superset(paths))
        });
        frames && changes && (!pings || self.pings.len() >= expected.pings.len())
    }
}

/// A scenario's directory, canonical so the daemon's resolved paths spell it the same way.
struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Root {
        let path =
            std::env::temp_dir().join(format!("noel-env-golden-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Root(path.canonicalize().unwrap())
    }

    fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Daemon {
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Receiver<Frame>,
}

impl Daemon {
    fn start(cwd: &Root) -> Daemon {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let token = format!("{:08x}{nanos:08x}", std::process::id());
        let mut child = Command::new(env!("CARGO_BIN_EXE_noel-env"))
            .args(["serve", "--token", &token])
            .current_dir(&cwd.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start noel-env serve");
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = Vec::new();
        stdout.read_until(b'\n', &mut line).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&line),
            frame::sync_line(&token),
            "the daemon's first output is the sync line"
        );
        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(Some(frame)) = frame::read_frame(&mut stdout) {
                if sender.send(frame).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Daemon {
            child,
            stdin,
            frames,
        }
    }

    fn send(&mut self, frame: &Frame) {
        frame::write_frame(self.stdin.as_mut().unwrap(), frame).expect("send a frame");
    }

    /// Collect frames into `answers` until it covers `expected`.
    fn await_answers(
        &self,
        answers: &mut Answers,
        expected: &Answers,
        pings: bool,
        root: &str,
        what: &str,
    ) {
        let deadline = Instant::now() + PATIENCE;
        while !answers.covers(expected, pings) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left) {
                Ok(frame) => {
                    let json = normalized(&replaced(&frame.json, root, "<root>"));
                    answers.add(frame.kind, frame.id, json, &frame.payload);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("{what}: timed out waiting for\n{expected:#?}\nreceived\n{answers:#?}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("{what}: the daemon stopped; received\n{answers:#?}")
                }
            }
        }
    }

    /// End stdin; the daemon stops everything it started and exits 0.
    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "the daemon exited with {status}");
                return;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("the daemon did not exit when its input ended");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Run one recorded scenario against `noel-env serve` and compare its answers.
fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scenario| scenario["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name} in pi-env.json"));
    let root = Root::new(name);
    let root_text = root.text();
    let mut daemon = Daemon::start(&root);
    let mut expected = Answers::default();
    let mut answers = Answers::default();
    for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        if let Some(receive) = step.get("receive") {
            expected.add(
                kind_of(receive["type"].as_str().unwrap()),
                receive["id"].as_u64().unwrap() as u32,
                receive["json"].clone(),
                &payload(&receive["payload"]),
            );
            continue;
        }
        let send = &step["send"];
        // The client sent this after the daemon frames before it; pings are waited for only at the end.
        let what = format!("{name} step {index}");
        daemon.await_answers(&mut answers, &expected, false, &root_text, &what);
        daemon.send(&Frame::with_payload(
            kind_of(send["type"].as_str().unwrap()),
            send["id"].as_u64().unwrap() as u32,
            replaced(&send["json"], "<root>", &root_text),
            payload(&send["payload"]),
        ));
    }
    daemon.await_answers(
        &mut answers,
        &expected,
        true,
        &root_text,
        &format!("{name} end"),
    );
    assert_eq!(
        answers.frames, expected.frames,
        "{name}: results, errors and events"
    );
    assert_eq!(
        printable(&answers.output),
        printable(&expected.output),
        "{name}: output per request and stream"
    );
    for (id, (paths, events)) in &expected.changes {
        let (seen, count) = &answers.changes[id];
        assert_eq!(
            seen, paths,
            "{name}: paths the changes of request {id} reported"
        );
        assert!(
            *count >= 1 && *events >= 1,
            "{name}: change events of request {id}"
        );
    }
    if let Some(ping) = expected.pings.first() {
        assert_eq!(&answers.pings[0], ping, "{name}: the daemon's ping");
    }
    daemon.finish();
}

fn printable(output: &BTreeMap<(u32, String), Vec<u8>>) -> BTreeMap<(u32, String), String> {
    output
        .iter()
        .map(|(key, bytes)| (key.clone(), String::from_utf8_lossy(bytes).into_owned()))
        .collect()
}

#[test]
fn the_goldens_are_from_the_pinned_upstream_commit() {
    let fixture = fixture();
    let pin = MAKEFILE
        .lines()
        .find_map(|line| line.strip_prefix("CHORD_REFERENCE_REF ?= "))
        .expect("CHORD_REFERENCE_REF in the Makefile");
    assert_eq!(fixture["source"]["ref"], pin.trim());
    let names: Vec<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scenario| scenario["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["files", "exec", "pty", "watch", "liveness"]);
}

#[test]
fn files() {
    replay("files");
}

#[test]
fn exec() {
    replay("exec");
}

#[test]
fn pty() {
    replay("pty");
}

#[test]
fn watch() {
    replay("watch");
}

#[test]
fn liveness() {
    replay("liveness");
}

#[test]
fn reports_its_version_and_usage() {
    let version = Command::new(env!("CARGO_BIN_EXE_noel-env"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("noel-env {}\n", env!("CARGO_PKG_VERSION"))
    );
    let usage = Command::new(env!("CARGO_BIN_EXE_noel-env"))
        .arg("serve")
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&usage.stderr),
        "usage: noel-env serve --token <hex>\n"
    );
}

#[test]
fn hello_reports_this_daemon() {
    let root = Root::new("hello");
    let mut daemon = Daemon::start(&root);
    daemon.send(&Frame::new(
        REQUEST,
        1,
        serde_json::json!({ "op": "hello", "protocol": 1 }),
    ));
    let reply = daemon.frames.recv_timeout(PATIENCE).unwrap();
    assert_eq!((reply.kind, reply.id), (RESULT, 1));
    assert_eq!(reply.json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(reply.json["os"], std::env::consts::OS);
    assert_eq!(reply.json["arch"], std::env::consts::ARCH);
    assert_eq!(reply.json["pid"], daemon.child.id());
    assert_eq!(reply.json["cwd"], root.text());
    daemon.finish();
}
