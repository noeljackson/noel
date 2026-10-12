//! `pi --mode rpc` against upstream's RPC goldens
//! (`tests/fixtures/ts-parity/rpc-protocol.json`), and the protocol's own
//! contract: framing, errors, run fencing, abort, shutdown and settling, and
//! runs that outlive the process that started them.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A pi in RPC mode answering from a faux script, with its own home, agent
/// dir and cwd under `root`.
struct Pi {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    records: Vec<Value>,
    root: PathBuf,
    args: Vec<String>,
}

/// A new directory for one pi, with `cwd`, `agent` and `home` inside.
fn test_root(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("rpc-{name}-{}-{nanos}", std::process::id()));
    for dir in ["cwd", "agent", "home"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    root
}

impl Pi {
    /// pi on `script` (the faux/script responses); `args` follow
    /// `--mode rpc`.
    fn start(name: &str, script: Value, args: &[&str]) -> Self {
        Self::spawn(test_root(name), script, args, json!({}))
    }

    fn spawn(root: PathBuf, script: Value, args: &[&str], mut settings: Value) -> Self {
        settings["modelRefresh"] = json!({ "enabled": false });
        fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
        fs::write(root.join("script.json"), script.to_string()).unwrap();
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let (child, stdin, lines) = launch(&root, &args);
        Self {
            child,
            stdin,
            lines,
            records: Vec::new(),
            root,
            args,
        }
    }

    /// pi on `script` in the session `durable`, which starts as `journal`
    /// when one is given, with retries off so a scripted error fails at
    /// once. [`Pi::restart`] starts a new pi in the same session.
    fn session(name: &str, script: Value, journal: Option<&[Value]>) -> Self {
        let root = test_root(name);
        let sessions = root.join("sessions");
        if let Some(records) = journal {
            fs::create_dir_all(&sessions).unwrap();
            let started =
                json!({"type": "started", "session_id": SESSION, "cwd": root.join("cwd")});
            let lines = std::iter::once(&started).chain(records);
            let journal = lines
                .map(|record| format!("{record}\n"))
                .collect::<String>();
            fs::write(sessions.join(format!("{SESSION}.jsonl")), journal).unwrap();
        }
        let sessions = sessions.display().to_string();
        let args = [
            "--no-context-files",
            "--session-dir",
            &sessions,
            "--session-id",
            SESSION,
        ];
        Self::spawn(root, script, &args, json!({"retry": {"enabled": false}}))
    }

    /// Kills pi at once (SIGKILL), as a crash or a lost host would, and
    /// starts a new one on the same files. Records start over.
    fn restart(&mut self) {
        let _ = self.child.kill();
        self.child.wait().unwrap();
        let (child, stdin, lines) = launch(&self.root, &self.args);
        (self.child, self.stdin, self.lines) = (child, stdin, lines);
        self.records.clear();
    }

    /// The session's journal, one record per line.
    fn journal(&self) -> Vec<Value> {
        let path = self.root.join(format!("sessions/{SESSION}.jsonl"));
        let journal = fs::read_to_string(path).unwrap();
        journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// `get_state.lastRun`, asked as `id`.
    fn last_run(&mut self, id: &str) -> Value {
        self.command(json!({"id": id, "type": "get_state"}));
        self.response(id)["data"]["lastRun"].clone()
    }

    /// Writes `bytes` to stdin as they are.
    fn write(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(bytes).unwrap();
        stdin.flush().unwrap();
    }

    fn send(&mut self, line: &str) {
        self.write(format!("{line}\n").as_bytes());
    }

    fn command(&mut self, command: Value) {
        self.send(&command.to_string());
    }

    /// Reads records until `done` holds for all of them so far.
    fn wait_until(&mut self, what: &str, done: impl Fn(&[Value]) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(&self.records) {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.push(line),
                Err(error) => panic!(
                    "{} waiting for {what}; records:\n{}",
                    match error {
                        RecvTimeoutError::Timeout => "timed out",
                        RecvTimeoutError::Disconnected => "pi exited",
                    },
                    transcript(&self.records)
                ),
            }
        }
    }

    fn push(&mut self, line: String) {
        let record = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"));
        self.records.push(record);
    }

    /// The response to the command with `id`, once it arrived.
    fn response(&mut self, id: &str) -> Value {
        self.wait_until(&format!("response {id}"), |records| {
            records.iter().any(|record| is_response(record, id))
        });
        let response = self.records.iter().find(|record| is_response(record, id));
        response.cloned().unwrap()
    }

    /// Waits for `count` records of type `kind`.
    fn wait_for(&mut self, kind: &str, count: usize) {
        self.wait_until(&format!("{count} {kind}"), |records| {
            of_type(records, kind).count() >= count
        });
    }

    /// Closes stdin and waits for pi to exit.
    fn close(&mut self) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "pi did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(1)) {
            self.push(line);
        }
        status
    }

    /// Lets a held faux response continue.
    fn release(&self, file: &str) {
        fs::write(self.root.join(file), "").unwrap();
    }

    /// Every model request, as the faux script logged it.
    fn logged_requests(&self) -> Vec<Value> {
        let log = fs::read_to_string(self.root.join("requests.jsonl")).unwrap_or_default();
        let requests = log.lines().map(|line| serde_json::from_str(line).unwrap());
        requests.collect()
    }

    /// What each model request contained, summarized as upstream's
    /// fixture summarizes them: its messages, without the system prompt.
    fn model_requests(&self) -> Vec<Value> {
        self.logged_requests()
            .iter()
            .map(|request| {
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(summarize_request_message)
                    .collect()
            })
            .collect()
    }
}

impl Drop for Pi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Starts pi under `root`, and a thread that reads its stdout lines.
fn launch(root: &Path, args: &[String]) -> (Child, Option<ChildStdin>, Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pi"))
        .current_dir(root.join("cwd"))
        .env("HOME", root.join("home"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_FAUX_SCRIPT", root.join("script.json"))
        .env("PI_FAUX_SCRIPT_LOG", root.join("requests.jsonl"))
        .args(["--offline", "--mode", "rpc", "--model", "faux/script"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn pi");
    let stdout = child.stdout.take().unwrap();
    let (sender, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let stdin = child.stdin.take();
    (child, stdin, lines)
}

/// A faux script step holding its response until `release-<index>` exists.
fn held_until(command_index: usize) -> Value {
    json!({ "wait_for_file": format!("release-{command_index}") })
}

/// A faux script step that holds its response until pi drops the request.
fn held() -> Value {
    json!({ "wait_for_file": "never" })
}

fn text(text: &str) -> Value {
    json!({ "text": text })
}

fn is_response(record: &Value, id: &str) -> bool {
    record["type"] == "response" && record["id"] == id
}

fn of_type<'a>(records: &'a [Value], kind: &'a str) -> impl Iterator<Item = &'a Value> {
    records.iter().filter(move |record| record["type"] == kind)
}

fn transcript(records: &[Value]) -> String {
    let lines = records.iter().map(Value::to_string).collect::<Vec<_>>();
    lines.join("\n")
}

fn summarize_request_message(message: &Value) -> Value {
    let mut summary = json!({ "role": message["role"] });
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        summary["text"] = json!(text);
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        let calls = calls.iter().map(|call| {
            json!(format!(
                "{} {}",
                call["id"].as_str().unwrap(),
                call["name"].as_str().unwrap()
            ))
        });
        summary["toolCalls"] = calls.collect();
    }
    if let Some(id) = message["tool_call_id"].as_str() {
        summary["toolCallId"] = json!(id);
    }
    summary
}

// Upstream's goldens.

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/ts-parity/rpc-protocol.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Runs the golden scenario `name` with `script` standing in for upstream's
/// stub model: sends the scenario's commands at the same points, then
/// compares the records and model requests with upstream's.
///
/// Where upstream's stub held a response while a command arrived, `script`
/// holds it with [`held_until`] that command's index: the release is written
/// once the command's response arrived.
fn golden(name: &str, script: Value) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scenario| scenario["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let placeholders = fixture["placeholders"].as_object().unwrap();
    let upstream = scenario["records"].as_array().unwrap();
    let golden = normalize(upstream);

    let root = test_root(name);
    for (path, content) in fixture["setup"]["files"].as_object().unwrap() {
        let path = path.replace("<tmp>", &root.display().to_string());
        fs::write(path, content.as_str().unwrap()).unwrap();
    }
    let args = upstream_args(scenario, &root);
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let mut pi = Pi::spawn(root, script, &args, json!({}));

    for (index, command) in scenario["commands"].as_array().unwrap().iter().enumerate() {
        let after = command["afterRecords"].as_u64().unwrap() as usize;
        let before = normalize(&upstream[..after]).len();
        pi.wait_until(
            &format!("{before} records before command {index}"),
            |records| normalize(records).len() >= before,
        );
        let responses = of_type(&pi.records, "response").count();
        match &command["command"] {
            Value::String(line) => pi.send(line),
            command => pi.command(command.clone()),
        }
        pi.wait_until(&format!("the response to command {index}"), |records| {
            of_type(records, "response").count() > responses
        });
        pi.release(&format!("release-{index}"));
    }
    // Every run settles and every command is answered.
    let ends = |records: &[Value]| {
        of_type(records, "response").count() + of_type(records, "agent_settled").count()
    };
    pi.wait_until("the last record", |records| ends(records) >= ends(&golden));
    assert!(pi.close().success());

    let expected = expected(upstream);
    let actual = normalize(&pi.records).into_iter().map(without_extensions);
    let actual = actual.collect::<Vec<_>>();
    let mut differences = Vec::new();
    for (index, (expected, actual)) in expected.iter().zip(&actual).enumerate() {
        compare(
            &format!("[{index}]"),
            expected,
            actual,
            placeholders,
            &mut differences,
        );
    }
    if expected.len() != actual.len() {
        differences.push(format!(
            "{} records, upstream has {}",
            actual.len(),
            expected.len()
        ));
    }
    assert!(
        differences.is_empty(),
        "{name} differs from upstream:\n{}\n\nexpected:\n{}\n\nactual:\n{}",
        differences.join("\n"),
        transcript(&expected),
        transcript(&actual)
    );
    assert_eq!(
        pi.model_requests(),
        *scenario["modelRequests"].as_array().unwrap(),
        "{name}: model requests"
    );
    // Upstream's requests lead with its system prompt, which the goldens
    // record as `<text>` sections; pi's lead with its own.
    for request in pi.logged_requests() {
        let prompt = request["system_prompt"].as_str().unwrap_or_default();
        assert!(
            prompt.starts_with("You are an expert coding assistant") && prompt.contains("<cwd>"),
            "{name}: system prompt {prompt:?}"
        );
    }
}

/// The scenario's arguments after `--mode rpc` and the model, with `<tmp>`
/// as `root`.
fn upstream_args(scenario: &Value, root: &Path) -> Vec<String> {
    let mut args = scenario["args"].as_array().unwrap().iter().map(|arg| {
        let arg = arg.as_str().unwrap();
        arg.replace("<tmp>", &root.display().to_string())
    });
    let mut out = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--mode" || arg == "--model" {
            args.next();
        } else {
            out.push(arg);
        }
    }
    out
}

/// Records as compared. Consecutive deltas of one content block collapse
/// into one, and system message records go: pi has no system messages in
/// the transcript (upstream #9548 is not ported).
fn normalize(records: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for record in records {
        if record["message"]["role"] == "system" {
            continue;
        }
        let delta = &record["assistantMessageEvent"];
        let kind = delta["type"].as_str().unwrap_or_default();
        if let Some(last) = out.last_mut() {
            let previous = &last["assistantMessageEvent"];
            if kind.ends_with("_delta")
                && previous["type"] == kind
                && previous["contentIndex"] == delta["contentIndex"]
            {
                let joined = format!(
                    "{}{}",
                    previous["delta"].as_str().unwrap(),
                    delta["delta"].as_str().unwrap()
                );
                last["assistantMessageEvent"]["delta"] = json!(joined);
                continue;
            }
        }
        out.push(record.clone());
    }
    out
}

/// Upstream's records as pi writes them. Besides the stub model's identity
/// (pi runs `faux/script`), each change is a difference recorded in
/// `docs/rust-rewrite/non-parity-register.md` under "RPC Mode":
///
/// - Messages pi does not keep: `messageCount` leaves out upstream's
///   system message and aborted replies.
/// - Queue modes: pi delivers queued messages together (`all`).
/// - Model object: pi has no `input`, `cost` or `maxTokens`, and no
///   `baseUrl` unless one is configured.
/// - Assistant messages have no `responseId`; usage has no `reasoning`.
/// - Tool results are text: no `structuredContent`.
/// - `get_messages` reports what the session saves: no timestamps, and
///   replies without model, usage or timings.
fn expected(upstream: &[Value]) -> Vec<Value> {
    // Upstream's messages that pi does not keep, so far.
    let mut unkept = 0;
    let records = upstream
        .iter()
        .map(|record| {
            let mut record = record.clone();
            let message = &record["message"];
            if record["type"] == "message_end"
                && (message["role"] == "system" || message["stopReason"] == "aborted")
            {
                unkept += 1;
            }
            visit(&mut record, &mut |object| {
                for (key, from, to) in [
                    ("provider", "stub", "faux"),
                    ("model", "stub-model", "script"),
                    ("api", "openai-completions", "faux"),
                    ("id", "stub-model", "script"),
                    ("name", "Stub Model", "Faux Script"),
                ] {
                    if object.get(key).is_some_and(|value| value == from) {
                        object.insert(key.into(), json!(to));
                    }
                }
                if object.contains_key("contextWindow") {
                    for key in ["input", "cost", "maxTokens", "baseUrl"] {
                        object.remove(key);
                    }
                }
                if object.get("role").is_some_and(|role| role == "assistant") {
                    object.remove("responseId");
                }
                if let Some(Value::Object(usage)) = object.get_mut("usage") {
                    usage.remove("reasoning");
                }
                if object
                    .get("type")
                    .is_some_and(|kind| kind == "tool_execution_end")
                {
                    object["result"]
                        .as_object_mut()
                        .unwrap()
                        .remove("structuredContent");
                }
            });
            if record["command"] == "get_state" {
                let state = record.pointer_mut("/data").unwrap();
                state["steeringMode"] = json!("all");
                state["followUpMode"] = json!("all");
                state["messageCount"] = json!(state["messageCount"].as_u64().unwrap() - unkept);
            }
            if record["command"] == "get_messages" {
                let messages = record.pointer_mut("/data/messages").unwrap();
                for message in messages.as_array_mut().unwrap() {
                    let message = message.as_object_mut().unwrap();
                    for key in [
                        "timestamp",
                        "api",
                        "provider",
                        "model",
                        "usage",
                        "rawStopReason",
                        "durationMs",
                        "thinkingLevel",
                    ] {
                        message.remove(key);
                    }
                }
            }
            record
        })
        .collect::<Vec<_>>();
    normalize(&records)
}

/// Calls `f` on every object in `value`, after removing system messages
/// from arrays.
fn visit(value: &mut Value, f: &mut impl FnMut(&mut Map<String, Value>)) {
    match value {
        Value::Object(object) => {
            f(object);
            for value in object.values_mut() {
                visit(value, f);
            }
        }
        Value::Array(values) => {
            values.retain(|value| value["role"] != "system");
            for value in values {
                visit(value, f);
            }
        }
        _ => {}
    }
}

/// Pi's records without the extensions upstream lacks.
fn without_extensions(mut record: Value) -> Value {
    visit(&mut record, &mut |object| {
        for key in ["runId", "requestId", "lastRun", "rpcExtensions"] {
            object.remove(key);
        }
    });
    record
}

/// Compares a record with upstream's. A golden string that is one
/// placeholder (`<timestamp>`) matches any value; one with text around
/// placeholders (`<tmp>/sessions/<session-file>`) matches a string with
/// that text.
fn compare(
    path: &str,
    expected: &Value,
    actual: &Value,
    placeholders: &Map<String, Value>,
    differences: &mut Vec<String>,
) {
    match (expected, actual) {
        (Value::String(pattern), _) if placeholders.keys().any(|key| pattern.contains(key)) => {
            if !matches_pattern(pattern, actual, placeholders) {
                differences.push(format!("{path}: {actual} does not match {pattern:?}"));
            }
        }
        (Value::Object(expected), Value::Object(actual)) => {
            for key in expected.keys().filter(|key| !actual.contains_key(*key)) {
                differences.push(format!("{path}.{key}: missing"));
            }
            for key in actual.keys().filter(|key| !expected.contains_key(*key)) {
                differences.push(format!("{path}.{key}: not in upstream's record"));
            }
            for (key, value) in expected {
                if let Some(actual) = actual.get(key) {
                    compare(
                        &format!("{path}.{key}"),
                        value,
                        actual,
                        placeholders,
                        differences,
                    );
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) if expected.len() == actual.len() => {
            for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
                compare(
                    &format!("{path}[{index}]"),
                    expected,
                    actual,
                    placeholders,
                    differences,
                );
            }
        }
        _ if expected != actual => {
            differences.push(format!("{path}: {actual}, upstream {expected}"))
        }
        _ => {}
    }
}

fn matches_pattern(pattern: &str, actual: &Value, placeholders: &Map<String, Value>) -> bool {
    if placeholders.contains_key(pattern) {
        return !actual.is_null();
    }
    let Some(mut rest) = actual.as_str() else {
        return false;
    };
    // The text between placeholders must appear in order, the first part
    // at the start and the last at the end.
    let mut parts = vec![pattern.to_owned()];
    for placeholder in placeholders.keys() {
        parts = parts
            .iter()
            .flat_map(|part| {
                part.split(placeholder.as_str())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    let (first, last) = (&parts[0], &parts[parts.len() - 1]);
    if !rest.starts_with(first.as_str()) || !rest.ends_with(last.as_str()) {
        return false;
    }
    rest = &rest[first.len()..];
    parts[1..parts.len() - 1]
        .iter()
        .all(|part| match rest.find(part.as_str()) {
            Some(at) => {
                rest = &rest[at + part.len()..];
                true
            }
            None => false,
        })
}

#[test]
fn golden_prompt_text() {
    golden("prompt-text", json!([[text("Hello"), text(" there")]]));
}

#[test]
fn golden_tool_round() {
    golden(
        "tool-round",
        json!([
            [
                {"tool_call": {"id": "call_read_1", "name": "read", "arguments": {"path": "notes.txt"}}},
                {"stop": "tool_calls"}
            ],
            [text("The file lists alpha and beta.")]
        ]),
    );
}

#[test]
fn golden_steer() {
    golden(
        "steer",
        json!([[held_until(1), text("One, two")], [text("Stopped.")]]),
    );
}

#[test]
fn golden_follow_up() {
    golden(
        "follow-up",
        json!([
            [held_until(1), text("Roses are red.")],
            [text("Les roses sont rouges.")]
        ]),
    );
}

#[test]
fn golden_abort() {
    golden("abort", json!([[text("Once"), held()]]));
}

#[test]
fn golden_abort_with_queue() {
    golden(
        "abort-with-queue",
        json!([
            [text("Once"), held()],
            [text("Shorter.")],
            [text("Summary.")]
        ]),
    );
}

#[test]
fn golden_prompt_while_busy() {
    golden(
        "prompt-while-busy",
        json!([
            [held_until(4), text("First.")],
            [text("Steered.")],
            [text("Followed.")]
        ]),
    );
}

#[test]
fn golden_state() {
    golden("state", json!([[text("Hi.")]]));
}

#[test]
fn golden_clear_queue() {
    golden("clear-queue", json!([[held_until(3), text("Working.")]]));
}

#[test]
fn golden_malformed_and_unknown() {
    golden("malformed-and-unknown", json!([]));
}

#[test]
fn golden_queue_modes() {
    golden("queue-modes", json!([]));
}

// The protocol's own contract.

fn settled(records: &[Value]) -> Vec<&Value> {
    of_type(records, "agent_settled").collect()
}

#[test]
fn lines_split_on_lf_only_and_a_last_line_without_lf_counts() {
    let mut pi = Pi::start("framing", json!([[text("ok")]]), &["--no-session"]);
    // CRLF: the CR is not part of the command.
    pi.write(b"{\"id\":\"crlf\",\"type\":\"get_state\"}\r\n");
    assert_eq!(pi.response("crlf")["success"], true);
    // U+2028 and U+2029 are valid inside JSON strings, not line breaks.
    let message = "one\u{2028}two\u{2029}three";
    pi.command(json!({"id": "p", "type": "prompt", "message": message}));
    pi.wait_for("agent_settled", 1);
    let user = of_type(&pi.records, "message_end")
        .find(|record| record["message"]["role"] == "user")
        .unwrap();
    assert_eq!(user["message"]["content"][0]["text"], message);
    // A last line without LF is still a command.
    pi.write(b"{\"id\":\"last\",\"type\":\"get_last_assistant_text\"}");
    assert!(pi.close().success());
    let last = pi.records.iter().find(|record| is_response(record, "last"));
    assert_eq!(last.unwrap()["data"]["text"], "ok");
}

#[test]
fn commands_that_fail_report_why() {
    let mut pi = Pi::start("errors", json!([]), &["--no-session"]);
    pi.send("{\"id\":");
    pi.command(json!({"id": "1", "type": "bash", "command": "ls"}));
    pi.command(json!({"id": "2", "type": "prompt"}));
    pi.command(json!({"id": "3", "type": "set_steering_mode", "mode": "one-at-a-time"}));
    pi.command(json!({"id": "4", "type": "set_model", "provider": "faux", "modelId": "missing"}));
    pi.command(json!({"id": "5", "type": "new_session", "parentSession": "/elsewhere.jsonl"}));
    pi.command(json!({"id": "6", "type": "steer", "message": "look", "images": [{"type": "image", "data": "", "mimeType": "image/png"}]}));
    pi.response("6");
    assert!(pi.close().success());
    let errors = pi
        .records
        .iter()
        .map(|record| {
            assert_eq!(record["success"], false, "{record}");
            (
                record["command"].clone(),
                record["error"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(errors[0].0, "parse");
    assert!(
        errors[0].1.starts_with("Failed to parse command: "),
        "{}",
        errors[0].1
    );
    assert_eq!(
        errors[1],
        (json!("bash"), "Unsupported command: bash".into())
    );
    assert_eq!(errors[2].0, "prompt");
    assert!(
        errors[2].1.contains("missing field `message`"),
        "{}",
        errors[2].1
    );
    assert!(errors[3].1.contains("only \"all\""), "{}", errors[3].1);
    assert_eq!(errors[4].1, "Model not found: faux/missing");
    assert!(errors[5].1.contains("parentSession"), "{}", errors[5].1);
    assert!(errors[6].1.contains("text only"), "{}", errors[6].1);
    assert_eq!(errors.len(), 7);
}

#[test]
fn a_run_refuses_commands_that_change_the_session() {
    let mut pi = Pi::start(
        "busy",
        json!([[held_until(1), text("done")]]),
        &["--no-session"],
    );
    pi.command(json!({"id": "run", "type": "prompt", "message": "work"}));
    pi.wait_for("turn_start", 1);
    for (index, command) in [
        json!({"type": "set_model", "provider": "faux", "modelId": "echo"}),
        json!({"type": "set_thinking_level", "level": "off"}),
        json!({"type": "compact"}),
        json!({"type": "new_session"}),
        json!({"type": "switch_session", "sessionPath": "other"}),
        json!({"type": "set_session_name", "name": "busy"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut command = command;
        command["id"] = json!(index.to_string());
        let kind = command["type"].as_str().unwrap().to_owned();
        pi.command(command);
        let response = pi.response(&index.to_string());
        assert_eq!(
            response["error"],
            format!("Cannot {kind} while the agent is running; wait for agent_settled")
        );
    }
    // Reads still answer, from the run's current state.
    pi.command(json!({"id": "state", "type": "get_state"}));
    let state = pi.response("state")["data"].clone();
    let run_id = pi.response("run")["data"]["runId"].clone();
    assert_eq!(state["isStreaming"], true);
    assert_eq!(state["runId"], run_id);
    assert_eq!(
        state["lastRun"],
        json!({"runId": run_id, "status": "running"})
    );
    assert_eq!(state["messageCount"], 1);
    assert_eq!(
        state["rpcExtensions"],
        json!(["run-id", "expected-run-id", "request-id", "resume-run"])
    );
    pi.release("release-1");
    pi.wait_for("agent_settled", 1);
    pi.command(json!({"id": "idle", "type": "get_state"}));
    let idle = pi.response("idle")["data"].clone();
    assert_eq!(idle["isStreaming"], false);
    assert!(idle.get("runId").is_none());
    assert_eq!(idle["messageCount"], 2);
    assert!(pi.close().success());
}

#[test]
fn run_ids_fence_steer_follow_up_and_abort() {
    let mut pi = Pi::start(
        "fence",
        json!([[held_until(4), text("first")], [text("steered")]]),
        &["--no-session"],
    );
    // With no run active, a fenced command changes nothing.
    pi.command(json!({"id": "0", "type": "steer", "message": "early", "expectedRunId": "nope"}));
    assert_eq!(
        pi.response("0")["error"],
        "expectedRunId nope does not match: no run is active"
    );
    pi.command(json!({"id": "1", "type": "prompt", "message": "go"}));
    let run_id = pi.response("1")["data"]["runId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(run_id.len(), 36);
    pi.wait_for("turn_start", 1);
    assert_eq!(
        of_type(&pi.records, "agent_start").next().unwrap()["runId"],
        run_id
    );
    for (id, kind) in [("2", "follow_up"), ("3", "abort")] {
        pi.command(json!({"id": id, "type": kind, "message": "x", "expectedRunId": "other"}));
        assert_eq!(
            pi.response(id)["error"],
            format!("expectedRunId other does not match the active run {run_id}")
        );
    }
    assert_eq!(of_type(&pi.records, "queue_update").count(), 0);
    pi.command(json!({"id": "4", "type": "steer", "message": "steer", "expectedRunId": run_id}));
    assert_eq!(pi.response("4")["data"], json!({"disposition": "queued"}));
    pi.release("release-4");
    pi.wait_for("agent_settled", 1);
    assert!(pi.close().success());
    let settled = settled(&pi.records);
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0]["runId"], run_id);
    assert_eq!(settled[0]["aborted"], false);
    assert_eq!(
        pi.model_requests()[1][2],
        json!({"role": "user", "text": "steer"})
    );
}

#[test]
fn abort_during_a_tool_kills_its_process_group_and_settles_once() {
    let mut pi = Pi::start(
        "abort-tool",
        json!([[
            {"tool_call": {"name": "bash", "arguments": {"command": "echo $$ > ../sleeper.pid; exec sleep 600"}}}
        ]]),
        &["--no-session"],
    );
    pi.command(json!({"id": "run", "type": "prompt", "message": "sleep"}));
    let run_id = pi.response("run")["data"]["runId"]
        .as_str()
        .unwrap()
        .to_owned();
    pi.wait_for("tool_execution_start", 1);
    let pid = wait_for_pid(&pi.root.join("sleeper.pid"));
    pi.command(json!({"id": "abort", "type": "abort", "expectedRunId": run_id}));
    let abort = pi.response("abort");
    assert_eq!(
        abort,
        json!({"id": "abort", "type": "response", "command": "abort", "success": true})
    );
    let settled_at = pi
        .records
        .iter()
        .position(|record| record["type"] == "agent_settled")
        .unwrap();
    let abort_at = pi
        .records
        .iter()
        .position(|record| is_response(record, "abort"))
        .unwrap();
    assert!(
        settled_at < abort_at,
        "abort answered before the run settled"
    );
    assert_eq!(pi.records[settled_at]["aborted"], true);
    let end = of_type(&pi.records, "tool_execution_end").next().unwrap();
    assert_eq!(end["isError"], true);
    assert_eq!(end["result"]["content"][0]["text"], "Operation aborted");
    let last_reply = of_type(&pi.records, "message_end")
        .filter(|record| record["message"]["role"] == "assistant")
        .last()
        .unwrap();
    assert_eq!(last_reply["message"]["stopReason"], "aborted");
    assert!(
        process_ended(pid),
        "the tool's process {pid} survived the abort"
    );
    assert!(pi.close().success());
    assert_eq!(settled(&pi.records).len(), 1);
}

#[test]
fn a_failed_run_settles_once_with_its_error() {
    let mut pi = Pi::spawn(
        test_root("failed"),
        json!([[{"error": "overloaded"}]]),
        &["--no-session"],
        json!({"retry": {"enabled": false}}),
    );
    pi.command(json!({"id": "run", "type": "prompt", "message": "hi"}));
    pi.wait_for("agent_settled", 1);
    assert!(pi.close().success());
    let reply = of_type(&pi.records, "message_end")
        .find(|record| record["message"]["role"] == "assistant")
        .unwrap();
    assert_eq!(reply["message"]["stopReason"], "error");
    assert!(
        reply["message"]["errorMessage"]
            .as_str()
            .unwrap()
            .contains("overloaded"),
        "{reply}"
    );
    let kinds = pi
        .records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            "response",
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled"
        ]
    );
    assert_eq!(settled(&pi.records)[0]["aborted"], false);
}

#[test]
fn a_retried_request_reports_its_retry_inside_the_turn() {
    let mut pi = Pi::spawn(
        test_root("retry"),
        json!([[{"error": "overloaded"}], [text("ok")]]),
        &["--no-session"],
        json!({"retry": {"baseDelayMs": 1}}),
    );
    pi.command(json!({"id": "run", "type": "prompt", "message": "hi"}));
    pi.wait_for("agent_settled", 1);
    assert!(pi.close().success());
    let kinds = pi
        .records
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    let start = kinds
        .iter()
        .position(|kind| *kind == "auto_retry_start")
        .unwrap();
    let end = kinds
        .iter()
        .position(|kind| *kind == "auto_retry_end")
        .unwrap();
    // pi retries before the reply starts; upstream ends the turn with an
    // error reply first (see the non-parity register).
    assert_eq!(kinds[start - 1], "message_end");
    assert_eq!(kinds[start + 1], "message_start");
    assert_eq!(kinds[end - 1], "message_end");
    assert_eq!(kinds[end + 1], "turn_end");
    let retry = &pi.records[start];
    assert_eq!(retry["attempt"], 1);
    assert_eq!(retry["maxAttempts"], 3);
    assert!(
        retry["errorMessage"]
            .as_str()
            .unwrap()
            .contains("overloaded"),
        "{retry}"
    );
    assert_eq!(
        pi.records[end],
        json!({"type": "auto_retry_end", "success": true, "attempt": 1})
    );
    assert_eq!(settled(&pi.records).len(), 1);
}

#[test]
fn stdin_closing_during_a_run_stops_it_and_its_tools_without_settling() {
    let mut pi = Pi::start(
        "eof",
        json!([[
            {"tool_call": {"name": "bash", "arguments": {"command": "echo $$ > ../sleeper.pid; exec sleep 600"}}}
        ]]),
        &["--no-session"],
    );
    pi.command(json!({"id": "run", "type": "prompt", "message": "sleep"}));
    pi.wait_for("tool_execution_start", 1);
    let pid = wait_for_pid(&pi.root.join("sleeper.pid"));
    assert!(pi.close().success());
    assert!(process_ended(pid), "the tool's process {pid} survived EOF");
    for kind in ["tool_execution_end", "agent_end", "agent_settled"] {
        assert_eq!(of_type(&pi.records, kind).count(), 0, "{kind} after EOF");
    }
}

#[test]
fn set_model_switches_the_session_and_leaves_settings_alone() {
    let mut pi = Pi::start("set-model", json!([]), &["--no-session"]);
    let settings = pi.root.join("agent/settings.json");
    let before = fs::read(&settings).unwrap();
    pi.command(json!({"id": "1", "type": "set_model", "provider": "faux", "modelId": "echo"}));
    let model = pi.response("1")["data"].clone();
    assert_eq!(model["id"], "echo");
    assert_eq!(model["provider"], "faux");
    pi.command(json!({"id": "2", "type": "prompt", "message": "hi"}));
    pi.wait_for("agent_settled", 1);
    pi.command(json!({"id": "3", "type": "get_state"}));
    assert_eq!(pi.response("3")["data"]["model"]["id"], "echo");
    assert!(pi.close().success());
    let reply = of_type(&pi.records, "message_end").last().unwrap();
    assert_eq!(reply["message"]["model"], "echo");
    assert_eq!(reply["message"]["content"][0]["text"], "[faux/echo] hi");
    assert_eq!(fs::read(&settings).unwrap(), before);
}

#[test]
fn rpc_mode_takes_no_prompt_arguments() {
    let root = test_root("arguments");
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .env("HOME", root.join("home"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .args([
            "--offline",
            "--no-session",
            "--mode",
            "rpc",
            "--model",
            "faux/echo",
            "hello",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("takes no prompt arguments"));
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(root).unwrap();
}

fn wait_for_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(pid) = fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` is gone or a zombie, waiting a little for the kill.
fn process_ended(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        // The state follows the parenthesized command name.
        let ended = state.as_deref().is_none_or(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))
        });
        if ended || Instant::now() >= deadline {
            return ended;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Runs that outlive the process that started them.

const SESSION: &str = "durable";

/// The prompt that started run `run-1` for request `m-1`, as the journal
/// saves it: one record.
fn started_run(prompt: &str) -> Value {
    json!({
        "type": "message",
        "message": {"role": "user", "content": prompt},
        "run": {"id": "run-1", "request_id": "m-1"}
    })
}

/// A saved reply that calls `calls`: (id, tool, arguments).
fn saved_reply(text: &str, calls: &[(&str, &str, Value)]) -> Value {
    let calls = calls.iter().map(
        |(id, name, arguments)| json!({"id": id, "name": name, "arguments": arguments.to_string()}),
    );
    json!({
        "type": "message",
        "message": {"role": "assistant", "content": text, "tool_calls": calls.collect::<Vec<_>>()}
    })
}

/// `run-1`'s `lastRun`, with `more` fields.
fn run_1(status: &str, more: Value) -> Value {
    let mut run = json!({"runId": "run-1", "requestId": "m-1", "status": status});
    run.as_object_mut()
        .unwrap()
        .extend(more.as_object().unwrap().clone());
    run
}

/// The placeholder result the model gets for a call that did not finish.
fn interrupted(id: &str, tool: &str) -> Value {
    let text = format!("tool call {tool} did not complete before the session continued");
    json!({"role": "tool", "text": text, "toolCallId": id})
}

fn resume(pi: &mut Pi, run_id: &Value) -> Value {
    pi.command(json!({"id": "resume", "type": "resume_run", "expectedRunId": run_id}));
    pi.response("resume")
}

/// The session's queue as last saved, without the record's type.
fn saved_queue(pi: &Pi) -> Value {
    let mut journal = pi.journal();
    journal.retain(|record| record["type"] == "queued_messages_snapshot");
    let mut queue = journal.pop().unwrap();
    queue.as_object_mut().unwrap().remove("type");
    queue
}

/// Waits until `path` holds `count` lines.
fn wait_for_lines(path: &Path, count: usize) {
    let deadline = Instant::now() + TIMEOUT;
    while fs::read_to_string(path).map_or(0, |text| text.lines().count()) < count {
        assert!(
            Instant::now() < deadline,
            "{} lines in {}",
            count,
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_interrupted_run_says_where_it_stopped_and_resumes_under_its_run_id() {
    let bash = ("b1", "bash", json!({"command": "echo ran >> ../ran.txt"}));
    let write = (
        "w1",
        "write",
        json!({"path": "written.txt", "content": "x"}),
    );
    let read = ("r1", "read", json!({"path": "notes.txt"}));
    let ls = ("l1", "ls", json!({}));
    let pending = |calls: &[(&str, &str, bool)]| {
        let calls = calls.iter().map(
            |(id, name, replay_safe)| json!({"id": id, "name": name, "replaySafe": replay_safe}),
        );
        json!({"phase": "tools", "pendingTools": calls.collect::<Vec<_>>()})
    };
    let result = |id: &str, text: &str| json!({"role": "tool", "text": text, "toolCallId": id});
    let user = json!({"role": "user", "text": "work"});
    let reply = |calls: &[&str]| json!({"role": "assistant", "toolCalls": calls});
    let cases = [
        (
            "model",
            vec![],
            json!({"phase": "model"}),
            vec![],
            vec![user.clone()],
        ),
        (
            "safe",
            vec![read.clone(), ls],
            pending(&[("r1", "read", true), ("l1", "ls", true)]),
            vec!["read", "ls"],
            vec![
                user.clone(),
                reply(&["r1 read", "l1 ls"]),
                result("r1", "alpha\n"),
                result("l1", "notes.txt"),
            ],
        ),
        (
            "unsafe",
            vec![bash.clone(), write],
            pending(&[("b1", "bash", false), ("w1", "write", false)]),
            vec![],
            vec![
                user.clone(),
                reply(&["b1 bash", "w1 write"]),
                interrupted("b1", "bash"),
                interrupted("w1", "write"),
            ],
        ),
        (
            "mixed",
            vec![bash, read],
            pending(&[("b1", "bash", false), ("r1", "read", true)]),
            vec!["read"],
            vec![
                user,
                reply(&["b1 bash", "r1 read"]),
                interrupted("b1", "bash"),
                result("r1", "alpha\n"),
            ],
        ),
    ];
    for (name, calls, stopped, rerun, told) in cases {
        let mut journal = vec![started_run("work")];
        if !calls.is_empty() {
            journal.push(saved_reply("", &calls));
        }
        let script = json!([[text("done")]]);
        let mut pi = Pi::session(&format!("resume-{name}"), script, Some(&journal));
        fs::write(pi.root.join("cwd/notes.txt"), "alpha\n").unwrap();
        assert_eq!(
            pi.last_run("before"),
            run_1("interrupted", stopped),
            "{name}"
        );
        assert_eq!(
            resume(&mut pi, &json!("run-1"))["data"],
            json!({"disposition": "started", "runId": "run-1"})
        );
        pi.wait_for("agent_settled", 1);
        assert_eq!(
            pi.last_run("after"),
            run_1("completed", json!({})),
            "{name}"
        );
        assert!(pi.close().success());

        let starts = of_type(&pi.records, "agent_start").collect::<Vec<_>>();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0]["runId"], "run-1");
        assert_eq!(settled(&pi.records)[0]["runId"], "run-1");
        let ran = of_type(&pi.records, "tool_execution_start")
            .map(|record| record["toolName"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ran, rerun, "{name}");
        if !rerun.is_empty() {
            // The calls run again in the turn of the saved reply they
            // belong to, which the run's records announce again.
            let ends = of_type(&pi.records, "turn_end").collect::<Vec<_>>();
            let message = &ends[0]["message"];
            assert_eq!(message["stopReason"], "toolUse", "{name}");
            assert_eq!(message["content"].as_array().unwrap().len(), calls.len());
            let results = ends[0]["toolResults"].as_array().unwrap();
            assert_eq!(results.len(), rerun.len(), "{name}");
        }
        assert_eq!(pi.model_requests(), [Value::Array(told)], "{name}");
        assert!(!pi.root.join("ran.txt").exists(), "{name}: bash ran again");
        assert!(
            !pi.root.join("cwd/written.txt").exists(),
            "{name}: write ran again"
        );
        let ends = pi
            .journal()
            .into_iter()
            .filter(|record| record["type"] == "run_finished")
            .collect::<Vec<_>>();
        assert_eq!(
            ends,
            [json!({"type": "run_finished", "run_id": "run-1", "status": "completed"})]
        );
    }
}

#[test]
fn a_finished_run_reports_how_it_ended_and_cannot_resume() {
    for status in ["completed", "failed", "aborted"] {
        let ended = json!({"type": "run_finished", "run_id": "run-1", "status": status});
        let journal = [started_run("work"), saved_reply("answer", &[]), ended];
        let mut pi = Pi::session(&format!("finished-{status}"), json!([]), Some(&journal));
        // The settle was missed: the driver reads how the run ended.
        assert_eq!(pi.last_run("state"), run_1(status, json!({})));
        assert_eq!(
            resume(&mut pi, &json!("run-1"))["error"],
            "run run-1 is not interrupted"
        );
        pi.command(json!({"id": "again", "type": "prompt", "message": "work", "requestId": "m-1"}));
        assert_eq!(
            pi.response("again")["data"],
            json!({"disposition": "handled", "runId": "run-1"})
        );
        pi.command(json!({"id": "text", "type": "get_last_assistant_text"}));
        assert_eq!(pi.response("text")["data"]["text"], "answer");
        assert!(pi.close().success());
        assert_eq!(of_type(&pi.records, "agent_start").count(), 0);
    }
}

#[test]
fn a_repeated_request_id_starts_nothing_and_a_new_prompt_ends_an_interrupted_run() {
    let journal = [started_run("work")];
    let mut pi = Pi::session("repeat", json!([[text("done")]]), Some(&journal));
    pi.command(json!({"id": "again", "type": "prompt", "message": "work", "requestId": "m-1"}));
    assert_eq!(
        pi.response("again")["data"],
        json!({"disposition": "started", "runId": "run-1"})
    );
    pi.command(json!({"id": "new", "type": "prompt", "message": "other", "requestId": "m-2"}));
    let run_id = pi.response("new")["data"]["runId"].clone();
    assert_ne!(run_id, "run-1");
    pi.wait_for("agent_settled", 1);
    // The new prompt ended the interrupted run, so its request is handled.
    pi.command(json!({"id": "first", "type": "prompt", "message": "work", "requestId": "m-1"}));
    assert_eq!(
        pi.response("first")["data"],
        json!({"disposition": "handled", "runId": "run-1"})
    );
    assert!(pi.close().success());
    assert_eq!(of_type(&pi.records, "agent_start").count(), 1);
    assert_eq!(pi.model_requests().len(), 1);
    let ends = pi
        .journal()
        .into_iter()
        .filter(|record| record["type"] == "run_finished")
        .map(|record| (record["run_id"].clone(), record["status"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        ends,
        [
            (json!("run-1"), json!("aborted")),
            (run_id, json!("completed"))
        ]
    );
}

#[test]
fn abort_ends_an_interrupted_run_while_none_is_active() {
    let journal = [started_run("work")];
    let mut pi = Pi::session("abort-interrupted", json!([]), Some(&journal));
    pi.command(json!({"id": "other", "type": "abort", "expectedRunId": "other"}));
    assert_eq!(
        pi.response("other")["error"],
        "expectedRunId other does not match the interrupted run run-1"
    );
    pi.command(json!({"id": "abort", "type": "abort", "expectedRunId": "run-1"}));
    assert_eq!(pi.response("abort")["success"], true);
    assert_eq!(pi.last_run("state"), run_1("aborted", json!({})));
    assert_eq!(
        resume(&mut pi, &json!("run-1"))["error"],
        "run run-1 is not interrupted"
    );
    assert!(pi.close().success());
    let journal = pi.journal();
    assert_eq!(
        journal.last().unwrap(),
        &json!({"type": "run_finished", "run_id": "run-1", "status": "aborted"})
    );
}

#[test]
fn a_journal_from_before_runs_has_no_last_run() {
    let old_prompt = json!({"type": "message", "message": {"role": "user", "content": "old"}});
    let journal = [old_prompt, saved_reply("answer", &[])];
    let mut pi = Pi::session("old-journal", json!([[text("new")]]), Some(&journal));
    pi.command(json!({"id": "state", "type": "get_state"}));
    let state = pi.response("state")["data"].clone();
    assert_eq!(state["messageCount"], 2);
    assert!(state.get("lastRun").is_none(), "{state}");
    pi.command(json!({"id": "bare", "type": "resume_run"}));
    assert!(
        pi.response("bare")["error"]
            .as_str()
            .unwrap()
            .contains("missing field `expectedRunId`"),
        "{}",
        pi.response("bare")
    );
    assert_eq!(
        resume(&mut pi, &json!("run-1"))["error"],
        "expectedRunId run-1 does not match: the session has no run"
    );
    pi.command(json!({"id": "abort", "type": "abort"}));
    assert_eq!(pi.response("abort")["success"], true);
    pi.command(json!({"id": "run", "type": "prompt", "message": "next"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    pi.wait_for("agent_settled", 1);
    assert_eq!(
        pi.last_run("after"),
        json!({"runId": run_id, "status": "completed"})
    );
    assert!(pi.close().success());
}

#[test]
fn a_model_request_cut_off_by_a_crash_is_sent_again_on_resume() {
    let mut pi = Pi::session("crash-model", json!([[held()], [text("done")]]), None);
    pi.command(json!({"id": "run", "type": "prompt", "message": "work", "requestId": "m-1"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    wait_for_lines(&pi.root.join("requests.jsonl"), 1);
    pi.restart();

    // The driver's recovery: check the run, clear the queue, resume.
    let stopped =
        json!({"runId": run_id, "requestId": "m-1", "status": "interrupted", "phase": "model"});
    assert_eq!(pi.last_run("state"), stopped);
    pi.command(json!({"id": "clear", "type": "clear_queue"}));
    pi.response("clear");
    assert_eq!(resume(&mut pi, &run_id)["success"], true);
    pi.wait_for("agent_settled", 1);
    assert_eq!(pi.last_run("after")["status"], "completed");
    pi.command(json!({"id": "text", "type": "get_last_assistant_text"}));
    assert_eq!(pi.response("text")["data"]["text"], "done");
    assert!(pi.close().success());
    let log = fs::read_to_string(pi.root.join("requests.jsonl")).unwrap();
    let requests = log.lines().collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1], "the resumed request differs");
}

#[test]
fn a_command_cut_off_by_a_crash_never_runs_again() {
    let command = "echo once >> ../counter.txt; echo $$ > ../sleeper.pid; exec sleep 600";
    let call =
        json!({"tool_call": {"id": "b1", "name": "bash", "arguments": {"command": command}}});
    let mut pi = Pi::session("crash-bash", json!([[call], [text("done")]]), None);
    pi.command(json!({"id": "run", "type": "prompt", "message": "count", "requestId": "m-1"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    let sleeper = wait_for_pid(&pi.root.join("sleeper.pid"));
    pi.restart();
    // SIGKILL left pi no chance to stop the command; stop it here.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(sleeper),
        nix::sys::signal::Signal::SIGKILL,
    );

    let bash = json!({"id": "b1", "name": "bash", "replaySafe": false});
    let stopped = json!({"runId": run_id, "requestId": "m-1", "status": "interrupted", "phase": "tools", "pendingTools": [bash]});
    assert_eq!(pi.last_run("state"), stopped);
    assert_eq!(resume(&mut pi, &run_id)["success"], true);
    pi.wait_for("agent_settled", 1);
    assert_eq!(pi.last_run("after")["status"], "completed");
    assert!(pi.close().success());
    assert_eq!(
        fs::read_to_string(pi.root.join("counter.txt")).unwrap(),
        "once\n"
    );
    assert_eq!(of_type(&pi.records, "tool_execution_start").count(), 0);
    assert_eq!(pi.model_requests()[1][2], interrupted("b1", "bash"));
}

#[test]
fn a_read_cut_off_by_a_crash_runs_again() {
    let call =
        json!({"tool_call": {"id": "r1", "name": "read", "arguments": {"path": "notes.txt"}}});
    let mut pi = Pi::session("crash-read", json!([[call], [text("done")]]), None);
    // Reading a FIFO waits for a writer, so the read is still running when
    // pi dies.
    let notes = pi.root.join("cwd/notes.txt");
    let fifo = Command::new("mkfifo").arg(&notes).status().unwrap();
    assert!(fifo.success());
    pi.command(json!({"id": "run", "type": "prompt", "message": "read", "requestId": "m-1"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    pi.wait_for("tool_execution_start", 1);
    pi.restart();
    fs::remove_file(&notes).unwrap();
    fs::write(&notes, "alpha\n").unwrap();

    let read = json!({"id": "r1", "name": "read", "replaySafe": true});
    let stopped = json!({"runId": run_id, "requestId": "m-1", "status": "interrupted", "phase": "tools", "pendingTools": [read]});
    assert_eq!(pi.last_run("state"), stopped);
    assert_eq!(resume(&mut pi, &run_id)["success"], true);
    pi.wait_for("agent_settled", 1);
    assert_eq!(pi.last_run("after")["status"], "completed");
    assert!(pi.close().success());
    let end = of_type(&pi.records, "tool_execution_end").next().unwrap();
    assert_eq!(end["result"]["content"][0]["text"], "alpha\n");
    assert_eq!(
        pi.model_requests()[1][2],
        json!({"role": "tool", "text": "alpha\n", "toolCallId": "r1"})
    );
}

#[test]
fn a_request_id_names_its_run_after_a_restart() {
    let mut pi = Pi::session("request-id", json!([[text("done")]]), None);
    pi.command(json!({"id": "run", "type": "prompt", "message": "work", "requestId": "m-1"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    pi.wait_for("agent_settled", 1);
    pi.restart();
    pi.command(json!({"id": "again", "type": "prompt", "message": "work", "requestId": "m-1"}));
    assert_eq!(
        pi.response("again")["data"],
        json!({"disposition": "handled", "runId": run_id})
    );
    pi.command(json!({"id": "messages", "type": "get_messages"}));
    let messages = pi.response("messages")["data"]["messages"].clone();
    assert_eq!(messages.as_array().unwrap().len(), 2);
    assert!(pi.close().success());
    assert_eq!(of_type(&pi.records, "agent_start").count(), 0);
    assert_eq!(pi.model_requests().len(), 1);
}

#[test]
fn how_a_run_ends_is_saved_and_a_closed_stdin_leaves_it_interrupted() {
    let script = json!([
        [text("Once"), held()],
        [{"error": "overloaded"}],
        [held()]
    ]);
    let mut pi = Pi::session("run-ends", script, None);
    // An abort.
    pi.command(json!({"id": "1", "type": "prompt", "message": "one", "requestId": "m-1"}));
    let aborted = pi.response("1")["data"]["runId"].clone();
    pi.wait_for("message_update", 1);
    pi.command(json!({"id": "abort", "type": "abort", "expectedRunId": aborted}));
    pi.response("abort");
    assert_eq!(pi.last_run("state-1")["status"], "aborted");
    // A failure.
    pi.command(json!({"id": "2", "type": "prompt", "message": "two", "requestId": "m-2"}));
    let failed = pi.response("2")["data"]["runId"].clone();
    pi.wait_for("agent_settled", 2);
    assert_eq!(pi.last_run("state-2")["status"], "failed");
    // A driver that goes away mid-run.
    pi.command(json!({"id": "3", "type": "prompt", "message": "three", "requestId": "m-3"}));
    let lost = pi.response("3")["data"]["runId"].clone();
    wait_for_lines(&pi.root.join("requests.jsonl"), 3);
    assert!(pi.close().success());
    assert_eq!(settled(&pi.records).len(), 2);

    pi.restart();
    let stopped =
        json!({"runId": lost, "requestId": "m-3", "status": "interrupted", "phase": "model"});
    assert_eq!(pi.last_run("state"), stopped);
    for (id, run_id, disposition) in [
        ("m-1", aborted, "handled"),
        ("m-2", failed, "handled"),
        ("m-3", lost, "started"),
    ] {
        pi.command(json!({"id": id, "type": "prompt", "message": "again", "requestId": id}));
        assert_eq!(
            pi.response(id)["data"],
            json!({"disposition": disposition, "runId": run_id})
        );
    }
    assert!(pi.close().success());
}

#[test]
fn sigterm_ends_pi_and_leaves_its_run_interrupted() {
    let mut pi = Pi::session("sigterm", json!([[held()]]), None);
    pi.command(json!({"id": "run", "type": "prompt", "message": "work", "requestId": "m-1"}));
    let run_id = pi.response("run")["data"]["runId"].clone();
    wait_for_lines(&pi.root.join("requests.jsonl"), 1);
    let pid = nix::unistd::Pid::from_raw(i32::try_from(pi.child.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    assert_eq!(pi.child.wait().unwrap().code(), Some(143));
    pi.restart();
    let stopped =
        json!({"runId": run_id, "requestId": "m-1", "status": "interrupted", "phase": "model"});
    assert_eq!(pi.last_run("state"), stopped);
    assert!(pi.close().success());
}

#[test]
fn the_queue_is_saved_with_the_session_before_it_is_acknowledged() {
    // An older pi saved queued messages as plain strings: follow-ups.
    let journal = [json!({"type": "queued_messages_snapshot", "messages": ["old"]})];
    let script = json!([[{"wait_for_file": "release"}, text("first")], [text("second")]]);
    let mut pi = Pi::session("queue", script, Some(&journal));
    pi.command(json!({"id": "steer", "type": "steer", "message": "look"}));
    assert_eq!(
        pi.response("steer")["data"],
        json!({"disposition": "queued"})
    );
    assert_eq!(
        saved_queue(&pi),
        json!({"messages": ["look", "old"], "steering": 1})
    );

    pi.restart();
    pi.command(json!({"id": "state", "type": "get_state"}));
    assert_eq!(pi.response("state")["data"]["pendingMessageCount"], 2);
    pi.command(json!({"id": "clear", "type": "clear_queue"}));
    assert_eq!(
        pi.response("clear")["data"],
        json!({"steering": ["look"], "followUp": ["old"]})
    );
    assert_eq!(saved_queue(&pi), json!({"messages": []}));

    // Steering queued during a run leaves the saved queue once delivered.
    pi.command(json!({"id": "run", "type": "prompt", "message": "work"}));
    pi.wait_for("turn_start", 1);
    pi.command(json!({"id": "more", "type": "steer", "message": "more"}));
    pi.response("more");
    assert_eq!(
        saved_queue(&pi),
        json!({"messages": ["more"], "steering": 1})
    );
    pi.release("release");
    pi.wait_for("agent_settled", 1);
    assert_eq!(saved_queue(&pi), json!({"messages": []}));
    assert!(pi.close().success());
    assert_eq!(
        pi.model_requests()[1].as_array().unwrap().last().unwrap(),
        &json!({"role": "user", "text": "more"})
    );

    pi.restart();
    pi.command(json!({"id": "state", "type": "get_state"}));
    assert_eq!(pi.response("state")["data"]["pendingMessageCount"], 0);
    assert!(pi.close().success());
}
