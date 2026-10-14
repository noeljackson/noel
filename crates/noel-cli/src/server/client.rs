//! `noel client`: line mode over the server (spec §5). It connects to
//! `server.sock`, starting a detached `noel server` when none listens, and
//! holds the server's objects:
//!
//! - `ls`: `noel.sessions`' directory;
//! - `new [--cwd DIR] [--model M] [--name N] [--thinking L]`: `create`;
//! - `send <id> <text> [--steer|--follow-up] [--request-id R]`: `noel.chat`'s
//!   `submit`. A send that fails `unavailable` (its worker was lost, or is
//!   starting) is sent again with the same `requestId`, which reports the
//!   first submission if it got through (spec §8.1);
//! - `watch <id>`: the view, its snapshot then each change, as text (or
//!   JSON lines with `--json`). It holds again when the worker is lost, and
//!   reconnects (starting a server) when the server is;
//! - `abort <id>`, `resume <id> [<runId>]`, `withdraw <id> <submissionId>`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use noel_chord::delta::{Op, Seg};
use noel_chord::host::{Host, HostEvent, HostOptions, LinkOptions, RemoteRef, StateUpdate};
use noel_chord::link::{websocket, WebSocketOptions};
use noel_chord::{ChordError, ErrorCode};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::Notify;

use super::serve::{start_detached, Started};
use super::{chat_contract, new_request_id, peer_check, sessions_contract, RuntimeDir};

#[derive(Debug, Parser)]
#[command(
    name = "noel client",
    about = "Use noel server's sessions from the command line"
)]
pub(crate) struct ClientArgs {
    #[command(subcommand)]
    command: ClientCommand,
}

#[derive(Debug, Subcommand)]
enum ClientCommand {
    /// List the sessions.
    Ls {
        /// Print the directory as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a session and print its id.
    New {
        /// Its working directory (default: this one).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Its model, `provider/id`.
        #[arg(long)]
        model: Option<String>,
        /// Its name.
        #[arg(long)]
        name: Option<String>,
        /// Its thinking level.
        #[arg(long)]
        thinking: Option<String>,
        /// The create's id; a repeat returns the same session.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Send a prompt (or with --steer or --follow-up, queue it while a run
    /// is active).
    Send {
        session: String,
        text: String,
        #[arg(long, conflicts_with = "follow_up")]
        steer: bool,
        #[arg(long)]
        follow_up: bool,
        /// The submission's id; a repeat reports the first submission.
        #[arg(long)]
        request_id: Option<String>,
        /// Print the result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the session's view, then its changes, until interrupted.
    Watch {
        session: String,
        /// Print each update as a JSON line.
        #[arg(long)]
        json: bool,
    },
    /// Abort the active run; queued submissions are withdrawn.
    Abort {
        session: String,
        #[arg(long)]
        expected_run_id: Option<String>,
    },
    /// Continue the session's interrupted last run.
    Resume {
        session: String,
        /// The run (default: the last run).
        run_id: Option<String>,
    },
    /// Withdraw a queued submission.
    Withdraw {
        session: String,
        submission_id: String,
    },
}

pub(crate) async fn run(args: ClientArgs) -> i32 {
    match command(args.command).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("noel client: {error:#}");
            1
        }
    }
}

async fn command(command: ClientCommand) -> Result<i32> {
    let client = Client::connect().await?;
    match command {
        ClientCommand::Ls { json } => client.ls(json).await,
        ClientCommand::New {
            cwd,
            model,
            name,
            thinking,
            request_id,
        } => {
            let cwd = match cwd {
                Some(cwd) => std::path::absolute(cwd)?,
                None => std::env::current_dir()?,
            };
            let mut args = json!({
                "requestId": request_id.unwrap_or_else(new_request_id),
                "cwd": cwd.display().to_string(),
            });
            for (key, value) in [
                ("model", model),
                ("name", name),
                ("thinkingLevel", thinking),
            ] {
                if let Some(value) = value {
                    args[key] = json!(value);
                }
            }
            let sessions = client.host.get(sessions_contract(), json!({}), "server");
            let result = call(&sessions, "create", args).await?;
            println!("{}", result["sessionId"].as_str().unwrap_or_default());
            Ok(0)
        }
        ClientCommand::Send {
            session,
            text,
            steer,
            follow_up,
            request_id,
            json,
        } => {
            let when_busy = if steer {
                "steer"
            } else if follow_up {
                "followUp"
            } else {
                "reject"
            };
            let request_id = request_id.unwrap_or_else(new_request_id);
            let args = json!({"requestId": request_id, "text": text, "whenBusy": when_busy});
            let result = client.chat_call(&session, "submit", args, true).await?;
            if json {
                println!("{}", json!({"requestId": request_id, "result": result}));
            } else {
                println!("{}", describe_submit(&result, &request_id));
            }
            Ok(0)
        }
        ClientCommand::Watch { session, json } => client.watch(&session, json).await,
        ClientCommand::Abort {
            session,
            expected_run_id,
        } => {
            let mut args = json!({});
            if let Some(run_id) = expected_run_id {
                args["expectedRunId"] = json!(run_id);
            }
            let result = client.chat_call(&session, "abort", args, false).await?;
            let withdrawn = result["withdrawn"].as_u64().unwrap_or(0);
            match result["runId"].as_str() {
                Some(run_id) => println!("aborted run {run_id}; withdrew {withdrawn} queued"),
                None => println!("no run was active; withdrew {withdrawn} queued"),
            }
            Ok(0)
        }
        ClientCommand::Resume { session, run_id } => {
            let run_id = match run_id {
                Some(run_id) => run_id,
                None => {
                    let view = client.view(&session).await?;
                    view["lastRun"]["runId"]
                        .as_str()
                        .ok_or_else(|| anyhow!("session {session} has no run"))?
                        .to_owned()
                }
            };
            let args = json!({"expectedRunId": run_id});
            let result = client.chat_call(&session, "resume", args, false).await?;
            println!(
                "resumed run {}",
                result["runId"].as_str().unwrap_or_default()
            );
            Ok(0)
        }
        ClientCommand::Withdraw {
            session,
            submission_id,
        } => {
            let args = json!({"submissionId": submission_id});
            let result = client.chat_call(&session, "withdraw", args, false).await?;
            println!("{}", result["outcome"].as_str().unwrap_or_default());
            Ok(0)
        }
    }
}

fn describe_submit(result: &Value, request_id: &str) -> String {
    let id = result["submissionId"].as_str().unwrap_or_default();
    match result["disposition"].as_str() {
        Some("started") => format!("started run {id} (request {request_id})"),
        Some("queued") => format!("queued submission {id} (request {request_id})"),
        _ => {
            let status = result["status"].as_str().unwrap_or("unknown");
            let run = result["runId"]
                .as_str()
                .map(|run| format!(" run {run}"))
                .unwrap_or_default();
            format!("repeated submission {id}: {status}{run} (request {request_id})")
        }
    }
}

/// A connection to the server, reconnected (and the server started) when
/// it drops.
struct Client {
    host: Host,
    lost: Arc<Notify>,
}

impl Client {
    async fn connect() -> Result<Self> {
        let lost = Arc::new(Notify::new());
        let signal = lost.clone();
        let host = Host::new(HostOptions {
            on_error: Some(Arc::new(|error| {
                if std::env::var_os("NOEL_CLIENT_DEBUG").is_some() {
                    eprintln!("noel client: chord: {error}");
                }
            })),
            observer: Some(Arc::new(move |event| {
                if matches!(event, HostEvent::LinkClosed { .. }) {
                    signal.notify_one();
                }
            })),
            ..HostOptions::client("client")
        });
        let stream = dial(true).await?;
        host.connect(link(stream).await?, LinkOptions::default());
        Ok(Self { host, lost })
    }

    /// Connects again after the server was lost, starting one if needed.
    async fn reconnect(&self) {
        let mut delay = Duration::from_millis(100);
        loop {
            match dial(true).await {
                Ok(stream) => match link(stream).await {
                    Ok(link) => {
                        self.host.connect(link, LinkOptions::default());
                        return;
                    }
                    Err(error) => eprintln!("noel client: reconnect: {error:#}"),
                },
                Err(error) => eprintln!("noel client: reconnect: {error:#}"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    fn chat(&self, session: &str) -> RemoteRef {
        self.host.get(
            chat_contract(),
            json!({ "session": session }),
            &format!("session:{session}"),
        )
    }

    /// Calls a `noel.chat` method. With `retry`, a call or hold that fails
    /// `unavailable` is made again, with the same arguments, a few times.
    async fn chat_call(
        &self,
        session: &str,
        method: &str,
        args: Value,
        retry: bool,
    ) -> Result<Value> {
        let mut delay = Duration::from_millis(200);
        let mut attempt = 1;
        loop {
            let chat = self.chat(session);
            let outcome = call(&chat, method, args.clone()).await;
            chat.release();
            match outcome {
                Err(error)
                    if retry
                        && attempt < 10
                        && error
                            .downcast_ref::<ChordError>()
                            .is_some_and(|error| error.code == ErrorCode::Unavailable) =>
                {
                    eprintln!("noel client: {method}: {error}; sending again ({attempt})");
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                outcome => return outcome,
            }
        }
    }

    /// The session's view as it is now.
    async fn view(&self, session: &str) -> Result<Value> {
        let chat = self.chat(session);
        let mut updates = chat.subscribe("view");
        chat.ready().await?;
        loop {
            match tokio::time::timeout(Duration::from_secs(30), updates.next()).await {
                Ok(Some(StateUpdate::Snapshot(value) | StateUpdate::Reset(value))) => {
                    return Ok(value)
                }
                Ok(Some(StateUpdate::Error(error))) => return Err(error.into()),
                Ok(Some(StateUpdate::Ops { .. })) => {}
                Ok(None) => return Err(anyhow!("the session's view ended")),
                Err(_) => return Err(anyhow!("no view within 30 s")),
            }
        }
    }

    async fn ls(&self, json: bool) -> Result<i32> {
        let sessions = self.host.get(sessions_contract(), json!({}), "server");
        let mut updates = sessions.subscribe("directory");
        sessions.ready().await?;
        let directory = loop {
            match tokio::time::timeout(Duration::from_secs(30), updates.next()).await {
                Ok(Some(StateUpdate::Snapshot(value) | StateUpdate::Reset(value))) => break value,
                Ok(Some(StateUpdate::Error(error))) => return Err(error.into()),
                Ok(Some(StateUpdate::Ops { .. })) => {}
                Ok(None) => return Err(anyhow!("the directory ended")),
                Err(_) => return Err(anyhow!("no directory within 30 s")),
            }
        };
        if json {
            println!("{directory}");
            return Ok(0);
        }
        let list = directory["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if list.is_empty() {
            println!("no sessions");
        }
        for session in list {
            println!("{}", describe_session(&session));
        }
        Ok(0)
    }

    /// Prints the view's snapshot, then each change, holding again when the
    /// worker is lost and reconnecting when the server is.
    async fn watch(&self, session: &str, json: bool) -> Result<i32> {
        let mut first = true;
        loop {
            let chat = self.chat(session);
            let mut updates = chat.subscribe("view");
            match chat.ready().await {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::Unavailable && !first => {
                    eprintln!("noel client: watch: {error}; holding again");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
            first = false;
            loop {
                tokio::select! {
                    update = updates.next() => match update {
                        Some(update) => print_update(&update, json),
                        None => break,
                    },
                    () = self.lost.notified() => {
                        print_note("server link lost; reconnecting", json);
                        self.reconnect().await;
                        print_note("reconnected; holding again", json);
                    }
                }
            }
            let why = match chat.status() {
                noel_chord::RefStatus::Gone(error) => error.to_string(),
                other => format!("{other:?}"),
            };
            print_note(&format!("view ended ({why}); holding again"), json);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

async fn call(reference: &RemoteRef, method: &str, args: Value) -> Result<Value> {
    reference.ready().await?;
    let result = reference.call(method, Some(args)).await?;
    Ok(result.unwrap_or(Value::Null))
}

/// The server's socket, checked: the runtime directory is this user's and
/// so is the process listening. Starts a server when none listens.
async fn dial(autostart: bool) -> Result<UnixStream> {
    let root = RuntimeDir::locate();
    let connect = |root: PathBuf| async move {
        let dir = RuntimeDir::check(root)?;
        let stream = UnixStream::connect(dir.server_socket()).await?;
        peer_check(&stream).map_err(|reason| anyhow!("the server's socket: {reason}"))?;
        anyhow::Ok(stream)
    };
    match connect(root.clone()).await {
        Ok(stream) => Ok(stream),
        Err(error) if !autostart => Err(error),
        Err(_) => {
            match start_detached().await? {
                Started::Running(pid) => eprintln!("noel client: started noel server (pid {pid})"),
                Started::AlreadyRunning(_) => {}
            }
            connect(root).await
        }
    }
}

async fn link(stream: UnixStream) -> Result<noel_chord::Link> {
    let (socket, _) = tokio_tungstenite::client_async("ws://localhost/chord", stream)
        .await
        .map_err(|error| anyhow!("websocket upgrade: {error}"))?;
    Ok(websocket(socket, WebSocketOptions::default()))
}

fn describe_session(session: &Value) -> String {
    let live = &session["live"];
    let state = if live["busy"] == true {
        "running"
    } else if live["worker"] == true {
        "open"
    } else {
        "-"
    };
    let title = session["name"]
        .as_str()
        .or(session["firstPrompt"].as_str())
        .map(|text| one_line(text, 60))
        .unwrap_or_default();
    let last = session["lastRun"]["status"].as_str().unwrap_or("-");
    format!(
        "{}  {state:7}  {:>4} msgs  last run {last:11}  {}  {}",
        session["sessionId"].as_str().unwrap_or_default(),
        session["messageCount"],
        session["cwd"].as_str().unwrap_or_default(),
        title
    )
}

fn print_note(note: &str, json: bool) {
    if json {
        println!("{}", json!({ "note": note }));
    } else {
        println!("== {note}");
    }
}

fn print_update(update: &StateUpdate, json: bool) {
    if json {
        let line = match update {
            StateUpdate::Snapshot(value) => json!({ "snapshot": value }),
            StateUpdate::Reset(value) => json!({ "reset": value }),
            StateUpdate::Ops { ops, value } => json!({
                "ops": ops.iter().map(Op::to_json).collect::<Vec<_>>(),
                "value": value,
            }),
            StateUpdate::Error(error) => json!({ "error": error.to_string() }),
        };
        println!("{line}");
        return;
    }
    match update {
        StateUpdate::Snapshot(value) | StateUpdate::Reset(value) => {
            let kind = if matches!(update, StateUpdate::Snapshot(_)) {
                "snapshot"
            } else {
                "reset"
            };
            println!("== {kind}");
            for line in render_view(value) {
                println!("{line}");
            }
            println!("== end of {kind}");
        }
        StateUpdate::Ops { ops, .. } => {
            for op in ops {
                println!("{}", render_op(op));
            }
        }
        StateUpdate::Error(error) => println!("== error: {error}"),
    }
}

/// The view as text: the conversation, its entries, what runs and what
/// waits.
fn render_view(view: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let conversation = &view["conversation"];
    let agent = &view["docs"]["pi.agent"];
    lines.push(format!(
        "session {} in {} (model {}, thinking {})",
        conversation["id"].as_str().unwrap_or_default(),
        conversation["cwd"].as_str().unwrap_or_default(),
        agent["model"].as_str().unwrap_or("-"),
        agent["thinkingLevel"].as_str().unwrap_or("-"),
    ));
    for (index, entry) in view["entries"].as_array().into_iter().flatten().enumerate() {
        lines.push(format!("[{}] {}", index + 1, render_entry(entry)));
    }
    let live = &view["docs"]["pi.live"];
    if let Some(run) = live.get("run") {
        lines.push(format!(
            "live: run {} (inputs {})",
            run["runId"].as_str().unwrap_or_default(),
            run["inputs"].as_array().map_or(0, Vec::len)
        ));
    }
    if let Some(generation) = live.get("generation") {
        let partial = message_text(&generation["message"]);
        lines.push(format!(
            "live: reply attempt {}: {}",
            generation["attempt"],
            if partial.is_empty() {
                "(nothing yet)".to_owned()
            } else {
                format!("partial {:?}", one_line(&partial, 200))
            }
        ));
        if let Some(retry) = generation.get("retry") {
            lines.push(format!(
                "live: retrying: {}",
                retry["error"].as_str().unwrap_or_default()
            ));
        }
    }
    for tool in live["tools"].as_array().into_iter().flatten() {
        let output = tool["output"].as_str().unwrap_or_default();
        lines.push(format!(
            "live: tool {} {} {}{}",
            tool["name"].as_str().unwrap_or_default(),
            tool["callId"].as_str().unwrap_or_default(),
            tool["status"].as_str().unwrap_or_default(),
            if output.is_empty() {
                String::new()
            } else {
                format!(": output {:?}", one_line(output, 200))
            }
        ));
    }
    if let Some(compaction) = live.get("compaction") {
        lines.push(format!(
            "live: compacting ({})",
            compaction["reason"].as_str().unwrap_or_default()
        ));
    }
    for item in view["docs"]["pi.inbox"]["items"]
        .as_array()
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "inbox: {} {}: {}",
            item["mode"].as_str().unwrap_or_default(),
            item["id"].as_str().unwrap_or_default(),
            one_line(item["content"].as_str().unwrap_or_default(), 120)
        ));
    }
    if let Some(last) = view.get("lastRun") {
        lines.push(format!(
            "last run: {} {}",
            last["runId"].as_str().unwrap_or_default(),
            last["status"].as_str().unwrap_or_default()
        ));
    }
    lines
}

fn render_entry(entry: &Value) -> String {
    let role = entry["role"].as_str().unwrap_or_default();
    match role {
        "toolResult" => format!(
            "tool {} ({}){}: {}",
            entry["toolName"].as_str().unwrap_or_default(),
            entry["toolCallId"].as_str().unwrap_or_default(),
            if entry["isError"] == true {
                " error"
            } else {
                ""
            },
            one_line(&message_text(entry), 200)
        ),
        "assistant" => {
            let calls = entry["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "toolCall")
                .map(|block| {
                    format!(
                        " -> {}({})",
                        block["name"].as_str().unwrap_or_default(),
                        block["arguments"]
                    )
                })
                .collect::<String>();
            let stop = match entry["stopReason"].as_str() {
                Some("aborted") => " [aborted]",
                _ => "",
            };
            format!(
                "assistant{stop}: {}{calls}",
                one_line(&message_text(entry), 200)
            )
        }
        role => format!("{role}: {}", one_line(&message_text(entry), 200)),
    }
}

/// The text and thinking of a message's content.
fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().map(str::to_owned),
                Some("thinking") => block["thinking"]
                    .as_str()
                    .map(|thinking| format!("(thinking: {thinking})")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// An op as a line: its verb, its path, and what it carries.
fn render_op(op: &Op) -> String {
    let path = op
        .path()
        .map(|path| {
            path.iter()
                .map(|seg| match seg {
                    Seg::Key(key) => key.clone(),
                    Seg::Index(index) => index.to_string(),
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();
    let value = |value: &Value| one_line(&value.to_string(), 160);
    match op {
        Op::Replace(value_) => format!("~ replace {}", value(value_)),
        Op::Set(_, value_) => format!("~ set {path} = {}", value(value_)),
        Op::Delete(_) => format!("~ delete {path}"),
        Op::Append(_, text) => format!("~ append {path} += {:?}", one_line(text, 160)),
        Op::Trim(_, count) => format!("~ trim {path} -{count}"),
        Op::Splice(_, index, remove, items) => {
            let mut line = format!("~ splice {path} at {index} remove {remove}");
            if path == "entries" {
                for item in items {
                    line.push_str(&format!("\n~   + {}", render_entry(item)));
                }
            } else if !items.is_empty() {
                line.push_str(&format!(
                    " insert {}",
                    one_line(&Value::Array(items.clone()).to_string(), 160)
                ));
            }
            line
        }
        Op::Permute(_, order) => format!("~ permute {path} {order:?}"),
    }
}

/// `text` on one line, at most `limit` characters.
fn one_line(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', "\\n");
    if flat.chars().count() <= limit {
        return flat;
    }
    let kept: String = flat.chars().take(limit).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_renders_its_entries_partial_tools_and_queue() {
        let view = json!({
            "conversation": {"id": "s1", "cwd": "/w"},
            "entries": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}}], "stopReason": "toolUse"},
            ],
            "docs": {
                "pi.agent": {"model": "faux/script", "thinkingLevel": "off", "cwd": "/w"},
                "pi.live": {
                    "run": {"runId": "r1", "inputs": ["r1"]},
                    "generation": {"attempt": 1, "message": {"role": "assistant", "content": [{"type": "text", "text": "Hel"}]}},
                    "tools": [{"callId": "c1", "name": "bash", "status": "running", "output": "a\nb"}],
                },
                "pi.inbox": {"items": [{"id": "q1", "mode": "steer", "content": "faster"}]},
            },
            "lastRun": {"runId": "r1", "status": "running"},
        });
        let lines = render_view(&view);
        assert_eq!(
            lines,
            [
                "session s1 in /w (model faux/script, thinking off)",
                "[1] user: hi",
                "[2] assistant:  -> bash({\"command\":\"ls\"})",
                "live: run r1 (inputs 1)",
                "live: reply attempt 1: partial \"Hel\"",
                "live: tool bash c1 running: output \"a\\\\nb\"",
                "inbox: steer q1: faster",
                "last run: r1 running",
            ]
        );
        let op = Op::Append(
            vec![
                Seg::Key("docs".into()),
                Seg::Key("pi.live".into()),
                Seg::Key("generation".into()),
            ],
            "lo".into(),
        );
        assert_eq!(render_op(&op), "~ append docs/pi.live/generation += \"lo\"");
    }
}
