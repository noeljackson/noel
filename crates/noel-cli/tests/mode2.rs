//! Mode 2 end to end (spec §5–§8): `noel client` against a `noel server` it
//! starts, with workers running `faux/script`, all as subprocesses in a
//! scratch home and runtime directory.
//!
//! Each test kills every server and worker it started when it ends, and
//! removes its directory.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(40);

/// A scratch noel: home, agent dir, a working directory and a runtime
/// directory, under a short path (socket paths are limited).
struct Mode2 {
    root: PathBuf,
    watches: Vec<Child>,
}

impl Mode2 {
    fn new(name: &str, script: Value, server: Value) -> Self {
        let root = std::env::temp_dir().join(format!("m2{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "agent", "cwd"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let settings = json!({
            "modelRefresh": {"enabled": false},
            "retry": {"enabled": false},
            "server": server,
        });
        fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
        fs::write(root.join("script.json"), script.to_string()).unwrap();
        Self {
            root,
            watches: Vec::new(),
        }
    }

    fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_of(Path::new(env!("CARGO_BIN_EXE_noel")), args)
    }

    /// `args` run by the noel binary at `program`.
    fn command_of(&self, program: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(self.root.join("cwd"))
            .env("HOME", self.root.join("home"))
            .env("NOEL_DIR", self.root.join("agent"))
            .env("NOEL_FAUX_SCRIPT", self.root.join("script.json"))
            .env("NOEL_FAUX_SCRIPT_LOG", self.root.join("requests.jsonl"))
            .env("NOEL_OFFLINE", "1")
            .env("NOEL_RUNTIME_DIR", self.run_dir())
            .env_remove("NOEL_SESSION_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .args(args)
            .stdin(Stdio::null());
        command
    }

    /// Runs `noel <args>` to its end.
    fn noel(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().expect("run noel");
        eprintln!(
            "$ noel {} -> {}\n{}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// `noel client <args>`, which must succeed; its stdout.
    fn client(&self, args: &[&str]) -> String {
        let mut full = vec!["client"];
        full.extend_from_slice(args);
        let output = self.noel(&full);
        assert!(output.status.success(), "noel {full:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }

    fn new_session(&self, request_id: &str) -> String {
        self.client(&["new", "--model", "faux/script", "--request-id", request_id])
            .trim()
            .to_owned()
    }

    /// `noel client watch <id> --json`, read line by line.
    fn watch(&mut self, session: &str) -> Watch {
        let mut child = self
            .command(&["client", "watch", session, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
        self.watches.push(child);
        Watch {
            lines,
            seen: Vec::new(),
            value: Value::Null,
        }
    }

    fn record(&self, name: &str) -> Option<Value> {
        let text = fs::read_to_string(self.run_dir().join(name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn server_pid(&self) -> u32 {
        self.record("server.json").unwrap()["pid"].as_u64().unwrap() as u32
    }

    fn worker_pid(&self, session: &str) -> u32 {
        self.record(&format!("workers/{session}.json")).unwrap()["pid"]
            .as_u64()
            .unwrap() as u32
    }

    fn release(&self, name: &str) {
        fs::write(self.root.join(name), "").unwrap();
    }

    fn journal(&self, session: &str) -> String {
        fs::read_to_string(self.root.join(format!("agent/sessions/{session}.jsonl"))).unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Mode2 {
    fn drop(&mut self) {
        for watch in &mut self.watches {
            let _ = watch.kill();
            let _ = watch.wait();
        }
        let mut records = vec![self.run_dir().join("server.json")];
        if let Ok(entries) = fs::read_dir(self.run_dir().join("workers")) {
            records.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json")),
            );
        }
        for record in records {
            let pid = fs::read_to_string(&record)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|record| record["pid"].as_u64());
            if let Some(pid) = pid {
                kill(pid as u32, "KILL");
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn kill(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status();
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A watch's JSON lines: `{"snapshot"}`, `{"reset"}`, `{"ops", "value"}`
/// and `{"note"}`.
struct Watch {
    lines: Receiver<String>,
    seen: Vec<Value>,
    /// The view after the last update.
    value: Value,
}

impl Watch {
    /// Reads lines until `done` holds for one, which it returns.
    fn until(&mut self, what: &str, done: impl Fn(&Value, &Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
                Err(_) => panic!(
                    "timed out waiting for {what}; saw:\n{}",
                    self.seen
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            };
            let update: Value = serde_json::from_str(&line).unwrap();
            if let Some(value) = update
                .get("snapshot")
                .or(update.get("reset"))
                .or(update.get("value"))
            {
                self.value = value.clone();
            }
            self.seen.push(update.clone());
            if done(&update, &self.value) {
                return update;
            }
        }
    }

    /// The next snapshot (or reset), and the view it holds.
    fn snapshot(&mut self, what: &str) -> Value {
        self.until(what, |update, _| {
            update.get("snapshot").is_some() || update.get("reset").is_some()
        });
        self.value.clone()
    }

    /// The view once `done` holds for it, which may be now.
    fn settle(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        if !done(&self.value) {
            self.until(what, |_, value| done(value));
        }
        self.value.clone()
    }
}

fn entries(view: &Value) -> Vec<Value> {
    view["entries"].as_array().cloned().unwrap_or_default()
}

fn text_of(entry: &Value) -> String {
    entry["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn partial(view: &Value) -> String {
    text_of(&view["docs"]["pi.live"]["generation"]["message"])
}

fn last_run_status(view: &Value) -> Option<String> {
    view["lastRun"]["status"].as_str().map(str::to_owned)
}

#[test]
fn a_client_creates_a_session_sends_and_a_late_watcher_sees_the_partial_then_deltas() {
    let mut mode2 = Mode2::new(
        "w",
        json!([[
            {"text": "Hello "},
            {"wait_for_file": "release-1"},
            {"text": "there, "},
            {"wait_for_file": "release-2"},
            {"text": "world"},
            {"wait_for_file": "release-3"},
        ]]),
        json!({"idleExitMs": 2500, "workerIdleExitMs": 1500}),
    );
    // The empty state: no server runs yet, so the client starts one.
    assert_eq!(mode2.client(&["ls"]), "no sessions\n");
    let server = mode2.server_pid();
    let session = mode2.new_session("create-1");
    // A repeated create names the same session and makes no other.
    assert_eq!(mode2.new_session("create-1"), session);
    let listed = mode2.client(&["ls", "--json"]);
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["sessionId"], json!(session));
    assert_eq!(listed["sessions"][0]["model"], "faux/script");

    let sent = mode2.client(&["send", &session, "hi", "--request-id", "send-1"]);
    assert!(sent.starts_with("started run "), "{sent}");
    let run_id = sent.split_whitespace().nth(2).unwrap().to_owned();
    // The reply streams "Hello " and waits; the partial is in the journal.
    wait_until("the first partial saved", || {
        mode2.journal(&session).contains("\"type\":\"partial\"")
    });
    mode2.release("release-1");
    wait_until("the second partial saved", || {
        mode2.journal(&session).contains("there, ")
    });

    // A watcher that joins now gets the view with the committed partial.
    let mut watch = mode2.watch(&session);
    let view = watch.snapshot("the snapshot");
    assert_eq!(partial(&view), "Hello there, ");
    assert_eq!(view["docs"]["pi.live"]["run"]["runId"], json!(run_id));
    assert_eq!(entries(&view).len(), 1);
    assert_eq!(text_of(&entries(&view)[0]), "hi");

    // Then deltas: the rest of the reply as an append, then the reply as an
    // entry and the run settled.
    mode2.release("release-2");
    let append = watch.until("the append", |update, _| {
        update["ops"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|op| op[0] == "a" && op[2] == "world")
    });
    assert!(append["ops"].is_array());
    mode2.release("release-3");
    let view = watch.settle("the run to settle", |value| {
        last_run_status(value).as_deref() == Some("completed")
    });
    assert_eq!(entries(&view).len(), 2);
    assert_eq!(text_of(&entries(&view)[1]), "Hello there, world");
    assert_eq!(view["docs"]["pi.live"], json!({}));

    // A sent submission's requestId again: the first submission, settled.
    let repeated = mode2.client(&["send", &session, "hi", "--request-id", "send-1"]);
    assert!(
        repeated.starts_with(&format!("repeated submission {run_id}: completed")),
        "{repeated}"
    );
    assert_eq!(mode2.requests().len(), 1);

    // Nothing holds the session once the watcher goes: the worker idles
    // out, then the server.
    let worker = mode2.worker_pid(&session);
    for mut watch in mode2.watches.drain(..) {
        let _ = watch.kill();
        let _ = watch.wait();
    }
    wait_until("the worker to idle out", || !alive(worker));
    wait_until("the server to idle out", || !alive(server));
    assert!(!mode2.run_dir().join("server.sock").exists());
    assert!(!mode2
        .run_dir()
        .join(format!("workers/{session}.sock"))
        .exists());
}

#[test]
fn an_abort_withdraws_the_queue_and_the_next_prompt_starts_without_it() {
    let mut mode2 = Mode2::new(
        "a",
        json!([
            [{"text": "Working"}, {"wait_for_file": "never"}],
            [{"text": "Fresh start"}],
        ]),
        json!({}),
    );
    let session = mode2.new_session("abort");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");
    mode2.client(&["send", &session, "start", "--request-id", "p1"]);
    watch.until("the partial", |_, view| partial(view) == "Working");
    let steer = mode2.client(&[
        "send",
        &session,
        "steer me",
        "--steer",
        "--request-id",
        "s1",
    ]);
    let follow = mode2.client(&["send", &session, "later", "--follow-up"]);
    assert!(steer.starts_with("queued submission "), "{steer}");
    assert!(follow.starts_with("queued submission "), "{follow}");
    let steer_id = steer.split_whitespace().nth(2).unwrap().to_owned();
    let view = watch.settle("two queued", |view| {
        view["docs"]["pi.inbox"]["items"]
            .as_array()
            .is_some_and(|items| items.len() == 2)
    });
    assert_eq!(view["docs"]["pi.inbox"]["items"][0]["mode"], "steer");
    assert_eq!(view["docs"]["pi.inbox"]["items"][1]["mode"], "followUp");
    // A prompt while busy is refused, not queued.
    let busy = mode2.noel(&["client", "send", &session, "too soon"]);
    assert!(!busy.status.success());
    assert!(String::from_utf8_lossy(&busy.stderr).contains("invalid/busy"));

    let aborted = mode2.client(&["abort", &session]);
    assert!(aborted.starts_with("aborted run "), "{aborted}");
    assert!(aborted.contains("withdrew 2 queued"), "{aborted}");
    let view = watch.settle("the run aborted", |view| {
        last_run_status(view).as_deref() == Some("aborted")
    });
    assert_eq!(view["docs"]["pi.inbox"]["items"], json!([]));
    // The aborted reply stays in the transcript.
    let last = entries(&view).last().cloned().unwrap();
    assert_eq!(last["stopReason"], "aborted");
    // The withdrawn steer has ended unanswered.
    assert_eq!(
        mode2.client(&["withdraw", &session, &steer_id]),
        "settled\n"
    );
    // Its requestId reports it withdrawn.
    let repeated = mode2.client(&[
        "send",
        &session,
        "steer me",
        "--steer",
        "--request-id",
        "s1",
    ]);
    assert!(repeated.contains(": withdrawn"), "{repeated}");

    // The next prompt starts with an empty queue: only it reaches the model.
    mode2.client(&["send", &session, "again"]);
    watch.until("the second run", |_, view| {
        entries(view)
            .last()
            .is_some_and(|entry| text_of(entry) == "Fresh start")
    });
    let requests = mode2.requests();
    assert_eq!(requests.len(), 2);
    let second = requests[1].to_string();
    assert!(second.contains("again") && !second.contains("steer me") && !second.contains("later"));
}

#[test]
fn a_killed_worker_is_started_again_and_resumes_its_run_with_the_interrupted_output() {
    let mut mode2 = Mode2::new(
        "k",
        json!([
            [{"text": "Running."}, {"tool_call": {"name": "bash", "arguments": {"command": "echo started; sleep 3; echo finished"}}}],
            [{"text": "Resumed after the crash"}],
        ]),
        json!({}),
    );
    let session = mode2.new_session("crash");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");
    mode2.client(&["send", &session, "run it"]);
    watch.until("the command's output", |_, view| {
        view["docs"]["pi.live"]["tools"][0]["output"]
            .as_str()
            .is_some_and(|output| output.contains("started"))
    });
    let worker = mode2.worker_pid(&session);
    kill(worker, "KILL");
    // The watcher holds again and sees the resumed run: the call's
    // interrupted result with the output it saved, then the new reply.
    watch.until("the view gone", |update, _| update.get("note").is_some());
    watch.snapshot("the snapshot from the new worker");
    let view = watch.settle("the resumed reply", |view| {
        last_run_status(view).as_deref() == Some("completed")
    });
    let entries = entries(&view);
    let result = &entries[2];
    assert_eq!(result["role"], "toolResult");
    assert_eq!(result["isError"], true);
    let output = text_of(result);
    assert!(output.starts_with("started\n"), "{output}");
    assert!(
        output.contains("[error] Tool bash was interrupted and may have partially run"),
        "{output}"
    );
    assert!(!output.contains("finished"), "{output}");
    assert_eq!(text_of(&entries[3]), "Resumed after the crash");
    // A new worker, of the next generation.
    let record = mode2.record(&format!("workers/{session}.json")).unwrap();
    assert_ne!(record["pid"].as_u64().unwrap() as u32, worker);
    assert_eq!(record["generation"], 2);
    let log = fs::read_to_string(mode2.run_dir().join("server.log")).unwrap();
    assert!(log.contains("resuming run"), "{log}");
}

#[test]
fn a_killed_server_leaves_its_worker_running_and_a_new_server_adopts_it() {
    let mut mode2 = Mode2::new(
        "s",
        json!([[{"text": "Part one, "}, {"wait_for_file": "release"}, {"text": "part two."}]]),
        json!({}),
    );
    let session = mode2.new_session("adopt");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");
    mode2.client(&["send", &session, "go"]);
    watch.until("the partial", |_, view| partial(view) == "Part one, ");
    let (server, worker) = (mode2.server_pid(), mode2.worker_pid(&session));
    kill(server, "KILL");
    wait_until("the server gone", || !alive(server));
    // The run finishes without a server.
    mode2.release("release");
    wait_until("the run finished in the journal", || {
        mode2.journal(&session).contains("\"run_finished\"")
    });
    assert!(alive(worker));
    // The watcher reconnects, starting a server, which adopts the worker,
    // and holds the view again: a new snapshot, which shows the run
    // finished (or will).
    watch.until("the reconnect", |update, _| {
        update["note"]
            .as_str()
            .is_some_and(|note| note.starts_with("reconnected"))
    });
    watch.snapshot("the snapshot after reconnecting");
    let view = watch.settle("the finished run", |view| {
        last_run_status(view).as_deref() == Some("completed")
    });
    assert_eq!(text_of(&entries(&view)[1]), "Part one, part two.");
    assert_ne!(mode2.server_pid(), server);
    assert_eq!(mode2.worker_pid(&session), worker);
    let log = fs::read_to_string(mode2.run_dir().join("server.log")).unwrap();
    assert!(log.contains(&format!("adopted worker {session}")), "{log}");
    // A second server cannot start while this one runs.
    let second = mode2.noel(&["server", "--foreground"]);
    assert_eq!(second.status.code(), Some(75));
    let detached = mode2.noel(&["server"]);
    assert_eq!(detached.status.code(), Some(75));
}

#[test]
fn a_server_of_another_build_shuts_down_the_old_worker_once_its_run_settles() {
    let mode2 = Mode2::new(
        "b",
        json!([
            [{"text": "Old build, "}, {"wait_for_file": "release"}, {"text": "done."}],
            [{"text": "New build."}],
        ]),
        json!({}),
    );
    let session = mode2.new_session("build");
    mode2.client(&["send", &session, "first"]);
    wait_until("the first partial", || {
        mode2.journal(&session).contains("Old build, ")
    });
    let (server, worker) = (mode2.server_pid(), mode2.worker_pid(&session));
    kill(server, "KILL");
    wait_until("the server gone", || !alive(server));
    // The same program as another file is another build.
    let copy = mode2.root.join("noel-copy");
    fs::copy(env!("CARGO_BIN_EXE_noel"), &copy).unwrap();
    let started = mode2.command_of(&copy, &["server"]).output().unwrap();
    assert!(started.status.success(), "{started:?}");
    let log = || fs::read_to_string(mode2.run_dir().join("server.log")).unwrap();
    wait_until("the refused adoption", || {
        log().contains(&format!("did not adopt worker {session}"))
    });
    assert!(log().contains("told it to shut down"), "{}", log());
    // It finishes its run first, then exits.
    assert!(alive(worker));
    mode2.release("release");
    wait_until("the old worker to exit", || !alive(worker));
    assert!(mode2.journal(&session).contains("\"status\":\"completed\""));
    // The new server starts a worker of its own build for the next hold.
    let sent = mode2.client(&["send", &session, "second"]);
    assert!(sent.starts_with("started run "), "{sent}");
    wait_until("the new reply", || {
        mode2.journal(&session).contains("New build.")
    });
    let record = mode2.record(&format!("workers/{session}.json")).unwrap();
    assert_ne!(record["pid"].as_u64().unwrap() as u32, worker);
    assert_eq!(record["generation"], 2);
}
