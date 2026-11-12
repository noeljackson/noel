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

    /// `noel client <args>` with `input` on its stdin, which must succeed.
    fn client_with_input(&self, args: &[&str], input: &str) -> String {
        use std::io::Write as _;
        let mut child = self
            .command(&[&["client"], args].concat())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        eprintln!(
            "$ noel client {} -> {}\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "noel client {args:?} failed");
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
        // A failed test shows what the server said before its directory goes.
        if std::thread::panicking()
            && let Ok(log) = fs::read_to_string(self.run_dir().join("server.log"))
        {
            eprintln!("--- server.log ---\n{log}--- end of server.log ---");
        }
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

#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !fs::read_to_string(format!("/proc/{pid}/stat"))
            .unwrap_or_default()
            .contains(") Z ")
}

#[cfg(not(target_os = "linux"))]
fn alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && !String::from_utf8_lossy(&output.stdout)
                    .trim_start()
                    .starts_with('Z')
        })
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

/// The messages of the view's entries (each entry's `model[0]`).
fn entries(view: &Value) -> Vec<Value> {
    view["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["model"].get(0).cloned())
        .collect()
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
    // The journal is written a moment before the view: the snapshot has the
    // partial, at most one delta behind.
    assert!(
        "Hello there, ".starts_with(&partial(&view)) && !partial(&view).is_empty(),
        "{}",
        partial(&view)
    );
    let view = watch.settle("the committed partial", |view| {
        partial(view) == "Hello there, "
    });
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
    let stderr = String::from_utf8_lossy(&busy.stderr);
    assert!(stderr.contains("busy: "), "{stderr}");

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
    // A child another test forks meanwhile may still hold the copy open for
    // writing (until its exec closes it): running it then fails "text file
    // busy" for a moment.
    let started = loop {
        match mode2.command_of(&copy, &["server"]).output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(20));
            }
            started => break started.unwrap(),
        }
    };
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

/// `noel.sessions.create` with a workspace (spec §10): refused, naming the
/// trust command, while noel does not trust the host; once it does, the
/// session is made with its `workspace` record and listed with its URL.
/// Nothing connects: no prompt is sent.
#[test]
fn a_session_is_created_in_a_workspace_only_on_a_trusted_host() {
    let mode2 = Mode2::new(
        "ws",
        json!([]),
        json!({"idleExitMs": 2500, "workerIdleExitMs": 1500}),
    );
    let url = "ssh://me@box:2222/work";
    let refused = mode2.noel(&["client", "new", "--workspace", url, "--request-id", "ws-1"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("noel workspace trust ssh://box:2222"),
        "{stderr}"
    );
    let known_hosts = mode2.root.join("agent/workspaces/known_hosts");
    fs::create_dir_all(known_hosts.parent().unwrap()).unwrap();
    fs::write(
        &known_hosts,
        "noel-env-[box]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEkTfD9MHTJ17XaccHgmbzbKL4eXWllTUvlX6Loix89\n",
    )
    .unwrap();
    let session = mode2
        .client(&["new", "--workspace", url, "--request-id", "ws-2"])
        .trim()
        .to_owned();
    let journal = mode2.journal(&session);
    let record: Value = serde_json::from_str(journal.lines().nth(1).unwrap()).unwrap();
    assert_eq!(record["type"], "workspace");
    assert_eq!(record["id"], "ssh://box:2222");
    assert_eq!(record["cwd"], "/work");
    let listed: Value = serde_json::from_str(&mode2.client(&["ls", "--json"])).unwrap();
    assert_eq!(listed["sessions"][0]["workspace"], url);
    assert!(mode2.client(&["ls"]).contains(url));
}

#[test]
fn a_continue_answers_after_a_failed_run_and_when_idle_reject_never_starts_one() {
    let mut mode2 = Mode2::new(
        "continue",
        json!([[{"error": "provider down"}], [{"text": "Recovered"}]]),
        json!({}),
    );
    let session = mode2.new_session("continue");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");
    // Idle, an input with whenIdle reject is refused and writes nothing.
    let idle = mode2.noel(&[
        "client",
        "send",
        &session,
        "only if busy",
        "--when-idle",
        "reject",
    ]);
    assert!(!idle.status.success());
    let stderr = String::from_utf8_lossy(&idle.stderr);
    assert!(stderr.contains("idle: "), "{stderr}");
    assert!(!mode2.journal(&session).contains("only if busy"));

    mode2.client(&["send", &session, "work"]);
    let view = watch.settle("the run failed", |view| {
        last_run_status(view).as_deref() == Some("failed")
    });
    let failure = &view["docs"]["pi.live"]["runFailure"];
    assert_eq!(failure["reason"], "model_error", "{failure}");

    // A continue runs on the context as it is: the prompt is asked again.
    let continued = mode2.client(&["continue", &session, "--request-id", "c1"]);
    assert!(continued.starts_with("started run "), "{continued}");
    let view = watch.settle("the continue answered", |view| {
        last_run_status(view).as_deref() == Some("completed")
    });
    assert!(view["docs"]["pi.live"].get("runFailure").is_none());
    assert_eq!(
        text_of(&entries(&view).last().cloned().unwrap()),
        "Recovered"
    );
    let requests = mode2.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);

    // Now the context ends in an answer: a continue has nothing to answer.
    mode2.client(&["continue", &session]);
    let view = watch.settle("nothing to answer", |view| {
        view["docs"]["pi.live"]["runFailure"]["reason"] == "nothing_to_answer"
    });
    assert_eq!(last_run_status(&view).as_deref(), Some("failed"));
    assert_eq!(mode2.requests().len(), 2);
}

#[test]
fn slash_commands_run_on_the_worker_as_the_tui_types_them() {
    let mut mode2 = Mode2::new("commands", json!([[{"text": "Hello there"}]]), json!({}));
    let session = mode2.new_session("commands");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");

    // A command that changes the session shows in its view.
    let named = mode2.client(&["command", &session, "/name release notes"]);
    assert!(named.contains("release notes"), "{named}");
    watch.settle("the name", |view| {
        view["conversation"]["name"] == "release notes"
    });
    let shown = mode2.client(&["command", &session, "/session"]);
    assert!(shown.contains(&session), "{shown}");
    assert_eq!(
        mode2.client(&["command", &session, "/queue"]),
        "queue is empty\n"
    );
    // A ! command runs in the session's cwd, on the worker.
    let shell = mode2.client(&["command", &session, "!echo from-the-worker && pwd"]);
    assert!(shell.contains("from-the-worker"), "{shell}");
    assert!(shell.contains("/cwd"), "{shell}");

    // A prompt starts a run.
    let started = mode2.client(&["command", &session, "say hello"]);
    assert!(started.starts_with("started run "), "{started}");
    watch.settle("the answer", |view| {
        last_run_status(view).as_deref() == Some("completed")
    });

    // A fork is another session, for the client to attach to.
    let forked = mode2.client(&["command", &session, "/fork"]);
    let fork_id = forked
        .lines()
        .find_map(|line| line.strip_prefix("session "))
        .unwrap_or_else(|| panic!("{forked}"))
        .to_owned();
    assert_ne!(fork_id, session);
    // The directory reads the new journal soon after; attaching to it by id
    // does not wait for that.
    wait_until("the fork listed", || {
        mode2.client(&["ls"]).contains(&fork_id)
    });
    let forked_view = mode2.client(&["command", &fork_id, "/session"]);
    assert!(forked_view.contains(&fork_id), "{forked_view}");

    // A selector comes back as data; an unknown command is an error.
    let selector = mode2.client(&["command", &session, "/selector settings"]);
    assert!(selector.lines().count() > 1, "{selector}");
    let unknown = mode2.noel(&["client", "command", &session, "/model nope/nothing"]);
    assert!(unknown.status.success());
}

/// `/undo` and `/rewind` (spec §9.2) on the worker: the checkpoints with
/// the files changed since, the choice of what to restore, and the files
/// put back or left.
#[test]
fn an_undo_and_a_rewind_restore_the_files_the_dropped_turns_changed() {
    let script = json!([
        [{"text": "first reply"}],
        [{"tool_call": {"name": "write", "arguments": {"path": "made.txt", "content": "new\n"}}}],
        [{"text": "wrote it"}],
    ]);
    let mode2 = Mode2::new("rewind", script, json!({}));
    let session = mode2.new_session("rewind");
    let file = |name: &str| mode2.root.join("cwd").join(name);
    fs::write(file("kept.txt"), "original").unwrap();
    let runs = || mode2.journal(&session).matches("\"run_finished\"").count();
    let command = |line: &str| mode2.client(&["command", &session, line]);

    mode2.client(&["send", &session, "one"]);
    wait_until("the first run", || runs() == 1);
    mode2.client(&["send", &session, "two"]);
    wait_until("the second run", || runs() == 2);
    command("/write kept.txt typed");
    wait_until("the typed write", || runs() == 3);
    assert!(file("made.txt").exists());

    // The checkpoints name the files the turns from each on changed.
    let checkpoints = command("/rewind");
    assert!(
        checkpoints.contains("two · 2 files: made.txt, kept.txt"),
        "{checkpoints}"
    );
    // A checkpoint with file changes after it asks what to restore.
    let choice = command("/select rewind m2");
    assert!(
        choice.contains("m2:all") && choice.contains("m2:chat"),
        "{choice}"
    );
    assert_eq!(
        command("/select rewind m2:chat"),
        "rewound; the files stay as they are\n"
    );
    assert!(file("made.txt").exists());
    assert_eq!(fs::read_to_string(file("kept.txt")).unwrap(), "typed");

    // An undo is the rewind to the last prompt, files included.
    command("/write kept.txt again");
    wait_until("the second typed write", || runs() == 4);
    assert_eq!(fs::read_to_string(file("kept.txt")).unwrap(), "again");
    assert_eq!(command("/undo"), "undid the last turn; 1 file restored\n");
    assert_eq!(fs::read_to_string(file("kept.txt")).unwrap(), "typed");

    // The changes the conversation-only rewind kept still belong to the
    // first prompt, and go back with it.
    assert_eq!(
        command("/select rewind m0:all"),
        "rewound; 2 files restored\n"
    );
    assert!(!file("made.txt").exists());
    assert_eq!(fs::read_to_string(file("kept.txt")).unwrap(), "original");
    assert_eq!(command("/rewind"), "nothing to rewind to\n");
    assert_eq!(command("/undo"), "nothing to undo\n");
}

/// A goal (spec §9.3) starts the work when it is set, and after each turn
/// that settles the evaluator, the session's own model without tools,
/// says whether it is met: not met sends the work on, met clears the goal.
#[test]
fn a_goal_keeps_the_session_working_until_its_evaluator_says_it_is_met() {
    let script = json!([
        [{"text": "I will start on it."}],
        [{"text": "NOT MET\nthe file does not exist yet"}],
        [{"tool_call": {"name": "write", "arguments": {"path": "goal.txt", "content": "done\n"}}}],
        [{"text": "The file is written; the goal is met."}],
        [{"text": "MET\nthe transcript shows the file written"}],
    ]);
    let mode2 = Mode2::new("goal", script, json!({}));
    let session = mode2.new_session("goal");
    let unset = mode2.client(&["command", &session, "/goal"]);
    assert!(unset.contains("/goal <condition>"), "{unset}");

    let set = mode2.client(&["command", &session, "/goal goal.txt exists"]);
    assert!(set.starts_with("goal set: goal.txt exists\n"), "{set}");
    wait_until("the goal met and cleared", || {
        mode2.journal(&session).contains("\"goal_cleared\"")
    });
    let journal = mode2.journal(&session);
    assert!(journal.contains(r#""reason":"condition met""#), "{journal}");
    assert_eq!(journal.matches("\"run_finished\"").count(), 2, "{journal}");
    assert_eq!(
        fs::read_to_string(mode2.root.join("cwd/goal.txt")).unwrap(),
        "done\n"
    );
    // The goal's own prompt, the evaluator, the continuation's two rounds,
    // the evaluator again; the evaluator is offered no tools.
    let requests = mode2.requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
    for (index, tools) in [(0, true), (1, false), (2, true), (4, false)] {
        let offered = requests[index]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        assert_eq!(offered, tools, "request {index}: {}", requests[index]);
    }
    let after = mode2.client(&["command", &session, "/goal"]);
    assert!(after.contains("/goal <condition>"), "{after}");

    // One set on a session at rest is cleared without a turn.
    let again = mode2.client(&["command", &session, "/goal clear"]);
    assert_eq!(again, "no goal is set\n");
}

/// The worker expands typed input with the session's configuration, as the
/// TUI's prompts were expanded in process: a prompt template, whether it
/// starts a run or is queued during one.
#[test]
fn the_worker_expands_templates_in_prompts_and_queued_follow_ups() {
    let mode2 = Mode2::new(
        "expand",
        json!([
            [{"text": "first, "}, {"wait_for_file": "release"}, {"text": "done."}],
            [{"text": "second."}],
        ]),
        json!({}),
    );
    fs::create_dir_all(mode2.root.join("agent/prompts")).unwrap();
    fs::write(mode2.root.join("agent/prompts/fix.md"), "Fix issue $1.\n").unwrap();
    let session = mode2.new_session("expand");
    let started = mode2.client(&["send", &session, "/fix 1"]);
    assert!(started.starts_with("started run "), "{started}");
    wait_until("the first partial saved", || {
        mode2.journal(&session).contains("\"type\":\"partial\"")
    });
    let queued = mode2.client(&["send", &session, "/fix 2", "--follow-up"]);
    assert!(queued.contains("queued"), "{queued}");
    mode2.release("release");
    wait_until("both turns answered", || {
        mode2.journal(&session).contains("second.")
    });
    let journal = mode2.journal(&session);
    for prompt in ["Fix issue 1.", "Fix issue 2."] {
        assert!(
            journal.contains(&format!("\"role\":\"user\",\"content\":\"{prompt}\\n\"")),
            "{prompt} in {journal}"
        );
    }
}

/// The server's objects for what is not a session's (spec §7.2): session
/// management, the models, accounts with a relayed login, and settings.
#[test]
fn the_server_manages_sessions_accounts_and_settings_for_a_client() {
    let mode2 = Mode2::new("objects", json!([[{"text": "Hello."}]]), json!({}));
    // noel.sessions: rename, a clone, a fork, delete, import.
    let session = mode2.new_session("objects");
    mode2.client(&["rename", &session, "release notes"]);
    wait_until("the name listed", || {
        mode2.client(&["ls"]).contains("release notes")
    });
    let clone = mode2
        .client(&["fork", &session, "--clone"])
        .trim()
        .to_owned();
    let fork = mode2.client(&["fork", &session]).trim().to_owned();
    assert_ne!(clone, fork);
    assert!(mode2.journal(&fork).contains(&session));
    mode2.client(&["delete", &clone]);
    wait_until("the clone gone", || !mode2.client(&["ls"]).contains(&clone));
    assert!(!mode2
        .root
        .join(format!("agent/sessions/{clone}.jsonl"))
        .exists());
    // A journal from elsewhere on the server's machine (only read).
    let export = mode2.root.join("elsewhere.jsonl");
    fs::write(
        &export,
        format!(
            "{{\"type\":\"started\",\"session_id\":\"from-elsewhere\",\"cwd\":\"{}\"}}\n\
             {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"imported prompt\",\"tool_calls\":[],\"media\":[]}}}}\n",
            mode2.root.join("cwd").display()
        ),
    )
    .unwrap();
    let imported = mode2
        .client(&["import", export.to_str().unwrap()])
        .trim()
        .to_owned();
    assert_ne!(imported, session);
    wait_until("the import listed", || {
        mode2.client(&["ls"]).contains("imported prompt")
    });
    assert!(mode2.journal(&imported).contains("imported prompt"));

    // noel.models: what a session can use, each saying whether it can.
    let models: Value = serde_json::from_str(&mode2.client(&["models", "--json"])).unwrap();
    let script = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["provider"] == "faux" && model["modelId"] == "script")
        .unwrap_or_else(|| panic!("{models}"));
    assert_eq!(script["available"], true);
    assert_eq!(script["scoped"], false);

    // noel.accounts: an API-key login relayed from the client, which only
    // hands over the key; the auth file is the server's.
    assert_eq!(mode2.client(&["accounts"]), "no accounts\n");
    let login = mode2.client_with_input(
        &["login", "openai", "--account", "work"],
        "sk-relayed-key\n",
    );
    assert_eq!(login, "logged in openai as account work\n");
    assert_eq!(mode2.client(&["accounts"]), "openai work api_key\n");
    let auth = fs::read_to_string(mode2.root.join("agent/auth.json")).unwrap();
    assert!(auth.contains("sk-relayed-key"), "{auth}");
    let listed = mode2.client(&["accounts", "--json"]);
    assert!(!listed.contains("sk-relayed-key"), "{listed}");
    assert_eq!(
        mode2.client(&["logout", "openai", "--account", "work"]),
        "logged out openai account work\n"
    );
    assert_eq!(mode2.client(&["accounts"]), "no accounts\n");

    // noel.settings: read and write the server machine's settings.
    mode2.client(&["settings", "theme", "\"dark\""]);
    assert_eq!(mode2.client(&["settings", "theme"]), "\"dark\"\n");
    let settings = fs::read_to_string(mode2.root.join("agent/settings.json")).unwrap();
    assert!(settings.contains("\"theme\": \"dark\""), "{settings}");
    mode2.client(&["settings", "theme", "null"]);
    assert_eq!(mode2.client(&["settings", "theme"]), "null\n");
    let settings = fs::read_to_string(mode2.root.join("agent/settings.json")).unwrap();
    assert!(
        settings.contains("modelRefresh") && !settings.contains("theme"),
        "{settings}"
    );
}

/// A session its worker holds is renamed and deleted through that worker:
/// a rename at once, a delete once no client holds it and it runs nothing.
#[test]
fn an_open_session_is_renamed_and_deleted_through_its_worker() {
    let mut mode2 = Mode2::new("open", json!([[{"text": "Hello."}]]), json!({}));
    let session = mode2.new_session("open");
    let mut watch = mode2.watch(&session);
    watch.snapshot("the snapshot");
    let worker = mode2.worker_pid(&session);
    mode2.client(&["rename", &session, "renamed while open"]);
    watch.settle("the name", |view| {
        view["conversation"]["name"] == "renamed while open"
    });
    // A client holds it: not deleted.
    let refused = mode2.noel(&["client", "delete", &session]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("open in a client"),
        "{refused:?}"
    );
    assert!(mode2
        .root
        .join(format!("agent/sessions/{session}.jsonl"))
        .exists());
    // With the client gone, the worker (still running) ends and it goes.
    let mut child = mode2.watches.pop().unwrap();
    let _ = child.kill();
    let _ = child.wait();
    assert!(alive(worker));
    mode2.client(&["delete", &session]);
    assert!(!mode2
        .root
        .join(format!("agent/sessions/{session}.jsonl"))
        .exists());
    wait_until("the worker gone", || !alive(worker));
    wait_until("the session unlisted", || {
        !mode2.client(&["ls"]).contains(&session)
    });
}
