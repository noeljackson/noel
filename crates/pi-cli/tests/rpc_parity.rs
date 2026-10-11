//! `pi --mode rpc` against upstream's RPC goldens
//! (`tests/fixtures/ts-parity/rpc-protocol.json`), and the protocol's own
//! contract: framing, errors, run fencing, abort, shutdown and settling.

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
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            records: Vec::new(),
            root,
        }
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

    /// What each model request contained, summarized as upstream's
    /// fixture summarizes them.
    fn model_requests(&self) -> Vec<Value> {
        let log = fs::read_to_string(self.root.join("requests.jsonl")).unwrap_or_default();
        log.lines()
            .map(|line| {
                let request: Value = serde_json::from_str(line).unwrap();
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
        object.remove("runId");
        object.remove("rpcExtensions");
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
    assert_eq!(state["messageCount"], 1);
    assert_eq!(state["rpcExtensions"], json!(["run-id", "expected-run-id"]));
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
