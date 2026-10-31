//! `noel worker --session <id> --generation <g> [--resume <runId>]`: the host
//! at `Session({id})` (spec §5), which the server starts and dials.
//!
//! - It opens the session's journal under its lock (`fatal`, reason
//!   `locked`, exit 75 if another process holds it), binds
//!   `workers/<id>.sock` (0600), writes `workers/<id>.json`, and prints one
//!   control line, `{"chord2control": {"type": "ready", ...}}`, on stdout.
//! - Each connection to its socket (from this user only) is its parent: it
//!   sends `ready` again (`pid`, `build`, `session`, `generation`), then
//!   speaks Chord v2 in framed messages, with tagged control frames for
//!   `status` (worker to server: `viewers`, `running`, `queued`, `runId`),
//!   `shutdown` (server to worker: exit once no run is active; worker to
//!   server: this worker is exiting). A new connection replaces the parent.
//! - It serves `noel.chat` (spec §7.2) backed by a session controller whose
//!   abort withdraws the queue (spec §3.3), and publishes the view
//!   ([`super::view`]).
//! - With no holds, no run and nothing queued for `server.workerIdleExitMs`
//!   (default 60 s) it exits. It outlives its server: a run finishes, and a
//!   new server can adopt it until it idles out.
//! - `--resume` continues that interrupted run once ready: the server's crash
//!   recovery (spec §8.3).

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine;
use clap::Parser;
use noel_ai::{MediaInput, ModelRef};
use noel_chord::v2::host::{Host, HostEvent, HostOptions, LinkOptions, RemoteRef, StateUpdate};
use noel_chord::v2::object::{BoxFuture, CallContext, Cell, Implementation};
use noel_chord::v2::transport::{framed, ControlSender};
use noel_chord::v2::{ChordError, WireError};
use noel_client::session_address;
use noel_config::{load_config_with_resources, project_is_trusted, LoadedConfig};
use noel_core::controller::{
    AbortPolicy, Busy, ControllerError, Enqueued, Event, Host as ControllerHost, Image, Repeat,
    SessionController, Start, Submission, SubmissionState, SubmitError, Submitted, WhenBusy,
    Withdrawal,
};
use noel_core::plugins::{PluginTool, PluginToolHost, PluginToolOutput};
use noel_core::{
    expand_input, ReloadableSystems, RunStatus, Runtime, SessionError, SessionStore, SubmissionKind,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Notify};

use super::protocol::{self, AbortArgs, CommandArgs, SubmitArgs, SubmitType, WithdrawArgs};

use super::errors::{forbidden, refused, unavailable};
use super::view::{self, View};
use super::{
    build_id, chat_contract, chat_variant, control_line, peer_check, remove_stale_socket, set_mode,
    valid_session_id, write_record, RuntimeDir, DEFAULT_WORKER_IDLE_MS, EXIT_LOCKED,
};
use crate::session_commands::{self, CommandOutcome, CommandUi, SessionSwitch};

#[derive(Debug, Parser)]
#[command(
    name = "noel worker",
    about = "Run one session for noel server (the server starts workers)"
)]
pub(crate) struct WorkerArgs {
    /// The session to run.
    #[arg(long)]
    session: String,
    /// Which start of this session's worker this is, from 1.
    #[arg(long)]
    generation: u64,
    /// The session's journal when it is kept outside the session directory.
    #[arg(long, value_name = "PATH")]
    journal: Option<std::path::PathBuf>,
    /// Continue this interrupted run once ready.
    #[arg(long, value_name = "RUN_ID")]
    resume: Option<String>,
}

/// Why a worker could not start: reported as a `fatal` control line.
struct Fatal {
    message: String,
    reason: Option<&'static str>,
    code: i32,
}

impl Fatal {
    fn new(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
            reason: None,
            code: 1,
        }
    }
}

impl<E: std::fmt::Display> From<E> for Fatal {
    fn from(error: E) -> Self {
        Fatal::new(error)
    }
}

pub(crate) async fn run(args: WorkerArgs) -> i32 {
    match start(args).await {
        Ok(code) => code,
        Err(fatal) => {
            let mut message = json!({"type": "fatal", "message": fatal.message});
            if let Some(reason) = fatal.reason {
                message["reason"] = json!(reason);
            }
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(control_line(&message).as_bytes());
            let _ = stdout.flush();
            tracing::info!("noel worker: {}", fatal.message);
            fatal.code
        }
    }
}

/// Serves the session until the supervisor ends the worker: its exit code.
async fn start(args: WorkerArgs) -> Result<i32, Fatal> {
    let session_id = args.session.clone();
    if !valid_session_id(&session_id) {
        return Err(Fatal::new(format!("invalid session id {session_id:?}")));
    }
    let dir = RuntimeDir::check(RuntimeDir::locate())?;
    dir.ensure_workers()?;
    // The server names its session directory in NOEL_SESSION_DIR.
    let here = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let session_dir = crate::config_paths(here, None)?.session_dir;
    let journal = args
        .journal
        .clone()
        .unwrap_or_else(|| session_dir.join(format!("{session_id}.jsonl")));
    if !journal.exists() {
        return Err(Fatal::new(format!("no session {session_id}")));
    }
    let (store, mut state) = match SessionStore::open(journal) {
        Ok(opened) => opened,
        Err(error @ SessionError::Locked { .. }) => {
            return Err(Fatal {
                message: error.to_string(),
                reason: Some("locked"),
                code: EXIT_LOCKED,
            })
        }
        Err(error) => return Err(Fatal::new(error)),
    };

    // The session's configuration, as `noel` started in its directory
    // builds it: with the flags of the `noel` that opened it, when one left
    // them (they hold while this worker runs), else with none.
    // The environment comes with them, as `noel` would have run in it (a
    // key exported in that shell, a provider setting).
    let launched = fs::read_to_string(dir.worker_args(&session_id))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or_default();
    let _ = fs::remove_file(dir.worker_args(&session_id));
    for (key, value) in launched["env"].as_object().into_iter().flatten() {
        if let Some(value) = value.as_str() {
            // SAFETY: the worker sets its launch environment first, before
            // it starts any task or thread that could read it.
            unsafe { std::env::set_var(key, value) };
        }
    }
    let launch: Vec<String> = serde_json::from_value(launched["args"].clone()).unwrap_or_default();
    let cli = crate::Cli::try_parse_from(std::iter::once("noel".to_owned()).chain(launch.clone()))
        .unwrap_or_else(|_| crate::Cli::parse_from(["noel"]));
    let _ = fs::write(
        dir.worker_applied_args(&session_id),
        serde_json::to_string(&launch).unwrap_or_default(),
    );
    // A relative directory is the opening `noel`'s, as there.
    if state.cwd.is_relative()
        && let Some(base) = launched["cwd"].as_str()
    {
        state.cwd = std::path::Path::new(base).join(&state.cwd);
    }
    let cwd = state.cwd.clone();
    if let Err(error) = std::env::set_current_dir(&cwd) {
        tracing::warn!(
            "noel worker: the session's directory {}: {error}",
            cwd.display()
        );
    }
    let paths = crate::config_paths(cwd.clone(), Some(session_dir))?;
    let trusted = if cli.approve {
        true
    } else if cli.no_approve {
        false
    } else {
        project_is_trusted(&paths.agent_dir, &cwd).unwrap_or(false)
    };
    let resources = crate::resource_commands::resource_options(&cli);
    let mut config = load_config_with_resources(paths, trusted, &resources)?;
    crate::apply_cli_overrides(&cli, &cwd, &mut config)?;
    let offline = crate::offline_enabled(cli.offline);
    crate::apply_cached_claude_code_version(&config);
    let mut selection = crate::resolve_cli_selection(&cli, &mut config, offline)?;
    // A session the client switched to keeps its model, as `/resume` does.
    if launched["switched"] == true {
        selection.model = None;
        selection.thinking = None;
    }
    let mut systems = ReloadableSystems::from_config(&config, 1);
    systems.offline = offline;
    let idle_limit = Duration::from_millis(
        config
            .settings
            .server
            .as_ref()
            .and_then(|server| server.worker_idle_exit_ms)
            .unwrap_or(DEFAULT_WORKER_IDLE_MS),
    );

    // The view hears each partial and tool output once it is saved.
    let saved = SessionStore::read_history(store.path())?;
    let view = Arc::new(Mutex::new(View::new(&state, &saved)));
    let heard = view.clone();
    let store = store.observe_progress(Arc::new(move |saved| lock(&heard).saved(saved)));
    let mut runtime = Runtime::with_store(state, systems, store);
    // A session in a workspace on another machine opens it at its first
    // tool call; one that cannot be opened fails those calls, saying why.
    crate::workspace::install(&mut runtime, &config.paths.agent_dir, true);
    crate::install_subagent_provider_factory(&mut runtime, &config, offline);
    crate::select_initial_model(&mut runtime, &config, &selection, offline)?;
    if !runtime.session().disabled_tools.is_empty() {
        runtime.set_disabled_tools(BTreeSet::new())?;
    }

    // The socket the server dials.
    let socket = dir.worker_socket(&session_id);
    remove_stale_socket(&socket)?;
    let listener = UnixListener::bind(&socket)
        .map_err(|error| Fatal::new(format!("bind {}: {error}", socket.display())))?;
    set_mode(&socket, 0o600)?;
    let build = build_id();
    let ready = json!({
        "type": "ready",
        "pid": std::process::id(),
        "build": build,
        "session": session_id,
        "generation": args.generation,
    });
    let record = dir.worker_record(&session_id);
    write_record(
        &record,
        &json!({
            "pid": std::process::id(),
            "session": session_id,
            "generation": args.generation,
            "build": build,
            "socket": socket.display().to_string(),
            "startedAt": super::now_ms(),
        }),
    )?;

    let status = Arc::new(StatusBoard::default());
    let host = chat_host(&session_id, status.clone());
    let (commands, inbox) = mpsc::unbounded_channel::<Command>();
    serve_chat(&host, &session_id, view.clone(), commands.clone());
    let clients = view.clone();

    let plugins = Arc::new(host.get(noel_client::contracts::plugins(), json!({})));
    follow_plugin_tools(plugins.clone(), commands.clone());
    let worker = WorkerHost {
        config,
        offline,
        view,
        status: status.clone(),
        aborts: Vec::new(),
        plugin_host: Arc::new(ServerPluginTools(plugins)),
        pending_plugin_tools: None,
        compaction: None,
    };
    let mut controller = SessionController::new(runtime, worker, AbortPolicy::WithdrawQueue);
    refresh(&mut controller);

    let parent: Arc<Mutex<Option<ControlSender>>> = Arc::default();
    tokio::spawn(accept_parents(
        listener,
        host.clone(),
        ready.clone(),
        parent.clone(),
        status.clone(),
    ));
    announce_ready(&ready);
    tracing::info!(
        "noel worker: session {session_id} generation {} ready (pid {})",
        args.generation,
        std::process::id()
    );
    let supervisor = supervise(Supervision {
        status,
        parent,
        idle_limit,
        host,
        files: Files { socket, record },
        view: clients,
    });
    tokio::pin!(supervisor);
    // The supervisor decides when the worker ends (#179): it returns the
    // exit code, and the session loop is dropped here, its controller and
    // journal with it, before `main` returns. The loop itself only ends
    // when every command sender is gone, which the served object keeps
    // alive: then the supervisor still decides.
    Ok(tokio::select! {
        code = &mut supervisor => code,
        () = drive(controller, inbox, args.resume) => supervisor.await,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `ready` line on stdout, then stdout goes to `/dev/null`: the server
/// reads only that line and may be gone before this worker is.
fn announce_ready(ready: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(control_line(ready).as_bytes());
    let _ = stdout.flush();
    drop(stdout);
    if let Ok(null) = File::options().write(true).open("/dev/null") {
        // SAFETY: both descriptors are open; dup2 replaces fd 1 atomically.
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) };
    }
}

// ─── status ─────────────────────────────────────────────────────────────────

/// What the server and the idle rule need to know.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Status {
    /// Holds on `noel.chat`.
    holds: usize,
    /// What the session loop has in hand.
    slot: Slot,
    queued: usize,
    /// Whether the worker serves new work or is on its way out.
    phase: Phase,
    /// A manual compaction runs: an abort cancels it.
    compacting: bool,
}

/// What the worker's session loop has in hand (#134). `Working` and
/// `Starting` block an exit as `Running` does: a start accepted and not yet
/// running is not idle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Slot {
    #[default]
    Idle,
    /// A command is in hand (a compaction, a configure, a slash command).
    Working,
    /// A command accepted a start; its run has not reported starting.
    Starting,
    /// Run `0` is active.
    Running(String),
}

/// Whether the worker takes new work (#134).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Serving,
    /// The server asked it to shut down once nothing is in hand; a new
    /// server connection makes it serve again.
    Draining,
    /// The last client quit (its extension hosts are shut down): it takes
    /// no new work and exits once no client holds the session.
    Leaving,
}

impl Status {
    fn idle(&self) -> bool {
        self.holds == 0 && self.slot == Slot::Idle && self.queued == 0
    }

    /// Why the worker exits now, if it does: the one place that decides.
    fn exit_reason(
        &self,
        idle_for: Option<Duration>,
        idle_limit: Duration,
    ) -> Option<&'static str> {
        if self.slot != Slot::Idle {
            return None;
        }
        match self.phase {
            Phase::Draining => Some("shut down by the server"),
            // Once the quitting client let go: a hold left would bind the
            // session's next worker.
            Phase::Leaving if self.holds == 0 => Some("its last client quit"),
            _ if idle_for.is_some_and(|idle| idle >= idle_limit) => Some("idle"),
            _ => None,
        }
    }

    /// The `status` line the server reads ([`WorkerStatus`]).
    fn control(&self) -> Value {
        let status = protocol::WorkerStatus {
            viewers: self.holds as u64,
            running: matches!(self.slot, Slot::Running(_)),
            queued: self.queued as u64,
            working: matches!(self.slot, Slot::Working | Slot::Starting),
            run_id: match &self.slot {
                Slot::Running(run_id) => Some(run_id.clone()),
                _ => None,
            },
        };
        let mut message = protocol::to_args(&status);
        message["type"] = json!("status");
        message
    }
}

struct StatusBoard {
    status: Mutex<(Status, Option<tokio::time::Instant>)>,
    changed: Notify,
}

impl Default for StatusBoard {
    fn default() -> Self {
        Self {
            status: Mutex::new((Status::default(), Some(tokio::time::Instant::now()))),
            changed: Notify::new(),
        }
    }
}

impl StatusBoard {
    /// Changes the status, noting when it last became idle: a hold taken
    /// and released between two looks still counts.
    fn update(&self, change: impl FnOnce(&mut Status)) {
        let mut guard = lock(&self.status);
        let (status, idle_since) = &mut *guard;
        change(status);
        *idle_since = status.idle().then(tokio::time::Instant::now);
        drop(guard);
        self.changed.notify_one();
    }

    fn get(&self) -> Status {
        lock(&self.status).0.clone()
    }

    /// Since when the worker has been idle, if it is.
    fn idle_since(&self) -> Option<tokio::time::Instant> {
        lock(&self.status).1
    }
}

// ─── the chord side ─────────────────────────────────────────────────────────

fn chat_host(session_id: &str, status: Arc<StatusBoard>) -> Host {
    let host = Host::new(HostOptions {
        address: Some(session_address(session_id)),
        ..HostOptions::default()
    });
    host.observe(move |event| match event {
        HostEvent::Held { type_name, .. } if type_name == "noel.chat" => {
            status.update(|status| status.holds += 1)
        }
        HostEvent::Released { type_name, .. } if type_name == "noel.chat" => {
            status.update(|status| status.holds = status.holds.saturating_sub(1))
        }
        HostEvent::Closed { .. } => tracing::info!("noel worker: link closed"),
        HostEvent::ProtocolError { error, .. } => tracing::warn!("noel worker: chord: {error}"),
        _ => {}
    });
    host
}

type Reply = oneshot::Sender<Result<Value, WireError>>;

/// A `noel.chat` method call, for the task that drives the session.
enum Command {
    Submit(Value, Reply),
    Withdraw(Value, Reply),
    Abort(Value, Reply),
    Compact(Value, Reply),
    Configure(Value, Reply),
    Resume(Value, Reply),
    /// A slash command or prompt, as the TUI types it.
    Run(Value, Reply),
    /// The plugin tools the server's directory offers now.
    PluginTools(Vec<PluginTool>),
}

fn serve_chat(
    host: &Host,
    session_id: &str,
    view: Arc<Mutex<View>>,
    commands: mpsc::UnboundedSender<Command>,
) {
    let session_id = session_id.to_owned();
    let cell = lock(&view).cell().clone();
    host.serve(chat_contract(), move |input, principal| {
        if input.get("session").and_then(Value::as_str) != Some(session_id.as_str()) {
            return Err(refused(
                "session",
                format!("this host serves session {session_id}"),
            ));
        }
        match chat_variant(principal, &session_id) {
            Some("participant") => {
                Ok(participant(&cell, &commands).method("entries", entries_method(&view)))
            }
            Some(_) => Ok(Implementation::new("viewer")
                .state("view", &cell)
                .method("entries", entries_method(&view))),
            None => Err(forbidden(format!(
                "session {session_id} is not for this principal"
            ))),
        }
    });
}

/// Entries before the view's window, for scrolling back: `entries({before,
/// limit?})` gives up to `limit` (default and at most 100) entries just
/// before entry `before`, and the entry before those.
fn entries_method(
    view: &Arc<Mutex<View>>,
) -> impl Fn(Value, CallContext) -> BoxFuture<Result<Value, WireError>> + Send + Sync + 'static + use<>
{
    let view = view.clone();
    move |args, _ctx| {
        let page = match args.get("before").and_then(Value::as_u64) {
            Some(before) => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map_or(super::view::WINDOW_MAX, |limit| limit as usize)
                    .clamp(1, super::view::WINDOW_MAX);
                Ok(lock(&view).page(before, limit))
            }
            None => Err(ChordError::invalid("args", "`before` must be an entry id").into()),
        };
        Box::pin(async move { page })
    }
}

fn participant(cell: &Cell, commands: &mpsc::UnboundedSender<Command>) -> Implementation {
    fn method(
        commands: &mpsc::UnboundedSender<Command>,
        make: fn(Value, Reply) -> Command,
    ) -> impl Fn(Value, CallContext) -> BoxFuture<Result<Value, WireError>> + Send + Sync + 'static + use<>
    {
        let commands = commands.clone();
        move |args, _ctx| {
            let commands = commands.clone();
            Box::pin(async move {
                let (reply, answer) = oneshot::channel();
                let closing = || WireError::from(unavailable("the session is closing"));
                let args = if args.is_null() { json!({}) } else { args };
                commands.send(make(args, reply)).map_err(|_| closing())?;
                answer.await.unwrap_or_else(|_| Err(closing()))
            })
        }
    }
    Implementation::new("participant")
        .state("view", cell)
        .method("submit", method(commands, Command::Submit))
        .method("withdraw", method(commands, Command::Withdraw))
        .method("abort", method(commands, Command::Abort))
        .method("compact", method(commands, Command::Compact))
        .method("configure", method(commands, Command::Configure))
        .method("resume", method(commands, Command::Resume))
        .method("command", method(commands, Command::Run))
}

/// Takes each connection to the worker's socket as its parent.
async fn accept_parents(
    listener: UnixListener,
    host: Host,
    ready: Value,
    parent: Arc<Mutex<Option<ControlSender>>>,
    status: Arc<StatusBoard>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!("noel worker: accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(reason) = peer_check(&stream) {
            tracing::warn!("noel worker: refused a connection: {reason}");
            continue;
        }
        let (read, mut write) = stream.into_split();
        if write
            .write_all(control_line(&ready).as_bytes())
            .await
            .is_err()
        {
            continue;
        }
        let (control, mut controls) = mpsc::unbounded_channel();
        let (link, sender) = framed(read, write, Some(control));
        sender.send(&status.get().control());
        *lock(&parent) = Some(sender);
        host.connect(
            link,
            LinkOptions {
                trusted: true,
                ..LinkOptions::new(json!({"kind": "server"}))
            },
        );
        tracing::info!("noel worker: a server connected");
        // A server that connects again wants the worker: a drain asked by
        // the one before it is over.
        status.update(|status| {
            if status.phase == Phase::Draining {
                status.phase = Phase::Serving;
            }
        });
        let status = status.clone();
        tokio::spawn(async move {
            while let Some(message) = controls.recv().await {
                if message.get("type").and_then(Value::as_str) == Some("shutdown") {
                    tracing::info!("noel worker: asked to shut down once no run is active");
                    status.update(|status| {
                        if status.phase == Phase::Serving {
                            status.phase = Phase::Draining;
                        }
                    });
                }
            }
            tracing::info!("noel worker: the server's link ended");
        });
    }
}

struct Files {
    socket: std::path::PathBuf,
    record: std::path::PathBuf,
}

impl Files {
    /// Removes the socket, and the record while it is still this
    /// process's.
    fn remove(&self) {
        let ours = fs::read_to_string(&self.record)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|record| record.get("pid").and_then(Value::as_u64))
            == Some(u64::from(std::process::id()));
        if ours {
            let _ = fs::remove_file(&self.record);
            let _ = fs::remove_file(&self.socket);
        }
    }
}

struct Supervision {
    status: Arc<StatusBoard>,
    parent: Arc<Mutex<Option<ControlSender>>>,
    idle_limit: Duration,
    host: Host,
    files: Files,
    view: Arc<Mutex<View>>,
}

/// Reports status to the parent, and ends the worker: idle for the limit,
/// asked to shut down with no run active, or by a signal.
/// Watches the worker until it should end; the exit code.
async fn supervise(supervision: Supervision) -> i32 {
    use tokio::signal::unix::{signal, SignalKind};
    let Supervision {
        status,
        parent,
        idle_limit,
        host,
        files,
        view,
    } = supervision;
    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut interrupt = signal(SignalKind::interrupt()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();
    let mut sent: Option<Value> = None;
    loop {
        let signalled = tokio::select! {
            () = status.changed.notified() => None,
            () = tokio::time::sleep(Duration::from_millis(500)) => None,
            Some(()) = recv(&mut terminate) => Some(libc::SIGTERM),
            Some(()) = recv(&mut interrupt) => Some(libc::SIGINT),
            Some(()) = recv(&mut hangup) => Some(libc::SIGHUP),
        };
        if let Some(signal) = signalled {
            // The run stays interrupted; the server resumes it.
            tracing::info!("noel worker: signal {signal}; exiting");
            noel_core::kill_running_tool_groups();
            files.remove();
            host.close().await;
            return 128 + signal;
        }
        let now = status.get();
        lock(&view).set_clients(now.holds);
        let report = now.control();
        if sent.as_ref() != Some(&report) {
            if let Some(parent) = &*lock(&parent) {
                parent.send(&report);
            }
            sent = Some(report);
        }
        let idle_for = status.idle_since().map(|since| since.elapsed());
        let done = now.exit_reason(idle_for, idle_limit);
        if let Some(why) = done {
            tracing::info!("noel worker: exiting ({why})");
            if let Some(parent) = &*lock(&parent) {
                parent.send(&json!({"type": "shutdown"}));
            }
            files.remove();
            host.close().await;
            return 0;
        }
    }
}

async fn recv(signal: &mut Option<tokio::signal::unix::Signal>) -> Option<()> {
    match signal {
        Some(signal) => signal.recv().await,
        None => std::future::pending().await,
    }
}

// ─── the session ────────────────────────────────────────────────────────────

/// The worker's side of the session controller.
struct WorkerHost {
    config: LoadedConfig,
    offline: bool,
    view: Arc<Mutex<View>>,
    status: Arc<StatusBoard>,
    /// Aborts to answer once the run settles, with how many queued
    /// submissions they withdrew.
    aborts: Vec<(Reply, usize)>,
    /// Runs plugin tools through the server's `noel.plugins`.
    plugin_host: Arc<dyn PluginToolHost>,
    /// Plugin tools that changed during a run, set once it settles.
    pending_plugin_tools: Option<Vec<PluginTool>>,
    /// Ends the manual compaction running now, if any: an abort that
    /// arrives meanwhile cancels it.
    compaction: Option<noel_core::CancellationToken>,
}

impl ControllerHost for WorkerHost {
    async fn provider(&mut self, runtime: &Runtime) -> Result<Box<dyn noel_ai::Provider>, String> {
        crate::provider_for_runtime(runtime, &self.config, self.offline)
            .await
            .map_err(|error| error.to_string())
    }

    fn media(&self, images: &[Image]) -> Result<Vec<MediaInput>, String> {
        if !images.is_empty() && crate::images_blocked(&self.config) {
            return Err("images are blocked by settings".into());
        }
        images
            .iter()
            .map(|image| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .map_err(|error| format!("invalid image data: {error}"))?;
                crate::media_input_from_bytes(bytes, image.mime_type.clone(), None, &self.config)
                    .map_err(|error| error.to_string())
            })
            .collect()
    }

    fn expand(&self, text: &str) -> String {
        expand_input(&self.config, text)
    }

    fn event(&mut self, event: Event<'_>) {
        for notice in crate::account_failover::drain_notices() {
            tracing::info!("noel worker: {notice}");
            lock(&self.view).notice(notice);
        }
        match &event {
            Event::RunStarted { run_id, .. } => {
                let run_id = (*run_id).to_owned();
                self.status
                    .update(|status| status.slot = Slot::Running(run_id));
            }
            Event::QueueChanged { queue } | Event::CallReturned { queue, .. } => {
                let pending = queue.pending();
                self.status.update(|status| status.queued = pending);
            }
            Event::SaveFailed { error } => tracing::warn!("noel worker: {error}"),
            _ => {}
        }
        lock(&self.view).event(&event);
        if let Event::RunSettled { run_id, outcome } = &event {
            tracing::info!("noel worker: run {run_id} settled: {:?}", outcome.status());
            self.status.update(|status| status.slot = Slot::Idle);
            for (reply, withdrawn) in self.aborts.drain(..) {
                let _ = reply.send(Ok(json!({"runId": run_id, "withdrawn": withdrawn})));
            }
        }
    }
}

impl WorkerHost {
    fn refresh(&mut self, runtime: &Runtime, queue: &[noel_core::QueueItem]) {
        let agent = view::agent(runtime, &self.config);
        lock(&self.view).refresh(runtime, agent, queue);
        let pending = queue.len();
        self.status.update(|status| status.queued = pending);
    }
}

fn refresh(controller: &mut SessionController<WorkerHost>) {
    let items = controller.queue().items();
    let (runtime, host) = controller.parts();
    host.refresh(runtime, &items);
}

/// Handles commands; a run they start is driven here, and what arrives
/// meanwhile is handled through [`Busy`].
/// How long a new worker waits for the server's plugin tools before it
/// handles its first command.
const PLUGIN_TOOLS_WAIT: Duration = Duration::from_secs(3);

async fn drive(
    mut controller: SessionController<WorkerHost>,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    resume: Option<String>,
) {
    if let Some(run_id) = resume {
        match controller.resume(&run_id).await {
            Ok(start) => {
                tracing::info!("noel worker: resuming run {run_id}");
                if !run_session(&mut controller, start, &mut inbox).await {
                    return;
                }
            }
            Err(error) => tracing::warn!("noel worker: run {run_id} not resumed: {error}"),
        }
    }
    // The server's plugin tools come first, so a run started at once
    // offers them; what arrives meanwhile waits its turn.
    let mut early = std::collections::VecDeque::new();
    let deadline = tokio::time::Instant::now() + PLUGIN_TOOLS_WAIT;
    while let Ok(Some(command)) = tokio::time::timeout_at(deadline, inbox.recv()).await {
        if let Command::PluginTools(tools) = command {
            set_plugin_tools(&mut controller, tools);
            break;
        }
        early.push_back(command);
    }
    // Whether the goal's evaluator is owed a look at the turn that just
    // settled (spec §9.3). What the user sent meanwhile goes first.
    let mut goal_due = false;
    loop {
        let command = match early.pop_front() {
            Some(command) => command,
            None if goal_due => {
                goal_due = false;
                match goal_step(&mut controller, &mut inbox, &mut early).await {
                    GoalStep::Continue(start) => {
                        if !run_session(&mut controller, *start, &mut inbox).await {
                            return;
                        }
                        goal_due = goal_follows(&mut controller);
                    }
                    GoalStep::Preempted => goal_due = true,
                    GoalStep::Done => {}
                }
                continue;
            }
            None => match inbox.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        let goal = controller.runtime().session().goal.clone();
        let status = controller.host().status.clone();
        status.update(|status| status.slot = Slot::Working);
        let start = idle_command_aborting(&mut controller, command, &mut inbox, &mut early).await;
        refresh(&mut controller);
        // A start in hand blocks an exit until its run reports starting
        // (#134: the status used to read idle in between).
        status.update(|status| {
            status.slot = if start.is_some() {
                Slot::Starting
            } else {
                Slot::Idle
            }
        });
        if let Some(start) = start {
            let finished = run_session(&mut controller, start, &mut inbox).await;
            status.update(|status| status.slot = Slot::Idle);
            if !finished {
                return;
            }
            goal_due = goal_follows(&mut controller);
        } else {
            // A goal just set starts its work; one just cleared owes
            // nothing.
            let now = controller.runtime().session().goal.clone();
            goal_due = match (&goal, &now) {
                (_, None) => false,
                (Some(before), Some(now)) if before.condition == now.condition => goal_due,
                (_, Some(_)) => true,
            };
        }
    }
}

/// What the goal's evaluator decided for the turn that settled.
enum GoalStep {
    /// Not met: the continuation's run.
    Continue(Box<Start>),
    /// A command arrived first; the evaluator is asked again after it.
    Preempted,
    /// Met, cleared, paused, or the evaluator could not be asked.
    Done,
}

/// Whether the run that just settled is one the goal's evaluator judges:
/// a goal is set and the run completed. A run the user interrupted, or
/// that failed, pauses the goal with a note.
fn goal_follows(controller: &mut SessionController<WorkerHost>) -> bool {
    let session = controller.runtime().session();
    if session.goal.is_none() {
        return false;
    }
    let paused = match session.runs.last().map(|run| run.status) {
        Some(RunStatus::Completed) => return true,
        Some(RunStatus::Aborted) => "goal paused: send a prompt to go on, or /goal clear to stop",
        _ => "goal paused: the run did not finish; send a prompt to go on, or /goal clear to stop",
    };
    goal_note(controller, paused.to_owned());
    false
}

fn goal_note(controller: &mut SessionController<WorkerHost>, note: String) {
    lock(&controller.host().view).notice(note);
    refresh(controller);
}

/// The goal's turn after a settled run (spec §9.3): a new goal starts its
/// work; otherwise the evaluator reads the conversation, and the goal is
/// cleared when met or past its turn cap, or the work goes on. A command
/// that arrives while the evaluator is asked goes first, and an abort
/// pauses the goal. An evaluator that fails or answers unparseably leaves
/// the goal for the next turn to settle.
async fn goal_step(
    controller: &mut SessionController<WorkerHost>,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
    early: &mut std::collections::VecDeque<Command>,
) -> GoalStep {
    let Some(goal) = controller.runtime().session().goal.clone() else {
        return GoalStep::Done;
    };
    let unfinished = controller.runtime().session().has_unfinished_turn();
    let asked = controller
        .runtime()
        .session()
        .messages
        .iter()
        .any(|message| message.role == noel_core::MessageRole::User);
    let prompt = if !asked {
        // Nothing to judge yet: the goal itself is the first prompt.
        noel_core::kickoff_prompt(&goal)
    } else if unfinished {
        return GoalStep::Done;
    } else {
        if goal.turns >= noel_core::GOAL_MAX_TURNS {
            let cleared = controller
                .configure(|runtime, _| runtime.clear_goal(Some("turn cap reached".to_owned())));
            let note = match cleared {
                Ok(()) => format!(
                    "goal cleared: {} turns ran without the condition being met; /goal <condition> to set another",
                    noel_core::GOAL_MAX_TURNS
                ),
                Err(error) => format!("goal: {error}"),
            };
            goal_note(controller, note);
            return GoalStep::Done;
        }
        let status = controller.host().status.clone();
        status.update(|status| status.slot = Slot::Working);
        let provider = {
            let (runtime, host) = controller.parts_mut();
            host.provider(runtime).await
        };
        let provider = match provider {
            Ok(provider) => provider,
            Err(error) => {
                status.update(|status| status.slot = Slot::Idle);
                goal_note(controller, format!("goal evaluator: {error}"));
                return GoalStep::Done;
            }
        };
        let verdict = {
            let (runtime, _) = controller.parts_mut();
            tokio::select! {
                verdict = noel_core::evaluate_goal(runtime, provider.as_ref(), &goal) => Ok(verdict),
                Some(next) = inbox.recv() => Err(next),
            }
        };
        status.update(|status| status.slot = Slot::Idle);
        match verdict {
            Err(Command::Abort(_, reply)) => {
                let _ = reply.send(Ok(json!({"withdrawn": 0})));
                goal_note(
                    controller,
                    "goal paused: send a prompt to go on, or /goal clear to stop".to_owned(),
                );
                return GoalStep::Done;
            }
            Err(next) => {
                early.push_back(next);
                return GoalStep::Preempted;
            }
            Ok(Some(noel_core::GoalVerdict::Met)) => {
                let cleared = controller
                    .configure(|runtime, _| runtime.clear_goal(Some("condition met".to_owned())));
                let note = match cleared {
                    Ok(()) => format!("goal met: {}", goal.condition),
                    Err(error) => format!("goal: {error}"),
                };
                goal_note(controller, note);
                return GoalStep::Done;
            }
            Ok(Some(noel_core::GoalVerdict::NotMet { why })) => {
                if let Err(error) = controller.configure(|runtime, _| runtime.advance_goal(&why)) {
                    goal_note(controller, format!("goal: {error}"));
                    return GoalStep::Done;
                }
                goal_note(controller, format!("goal not met: {why}"));
                noel_core::continuation_prompt(&goal, &why)
            }
            Ok(None) => {
                goal_note(
                    controller,
                    "goal evaluator did not answer; it is asked again after the next turn"
                        .to_owned(),
                );
                return GoalStep::Done;
            }
        }
    };
    let submission = Submission {
        kind: SubmissionKind::Prompt,
        request_id: None,
        content: prompt,
        images: Vec::new(),
        expected_run_id: None,
    };
    match controller.submit(submission).await {
        Ok(Submitted::Started(start)) => GoalStep::Continue(Box::new(start)),
        Ok(Submitted::Repeated(_)) => GoalStep::Done,
        Err(error) => {
            goal_note(controller, format!("goal: {error:?}"));
            GoalStep::Done
        }
    }
}

/// [`idle_command`], with the inbox read meanwhile: an abort cancels a
/// manual compaction in progress (#175) and is answered at once; anything
/// else waits for the command to finish.
async fn idle_command_aborting(
    controller: &mut SessionController<WorkerHost>,
    command: Command,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
    early: &mut std::collections::VecDeque<Command>,
) -> Option<Start> {
    let cancel = noel_core::CancellationToken::default();
    controller.host().compaction = Some(cancel.clone());
    let compacting = controller.host().status.clone();
    let start = {
        let command = idle_command(controller, command);
        tokio::pin!(command);
        loop {
            tokio::select! {
                start = &mut command => break start,
                Some(next) = inbox.recv() => match next {
                    Command::Abort(_, reply) if compacting.get().compacting => {
                        cancel.cancel();
                        let _ = reply.send(Ok(json!({"withdrawn": 0})));
                    }
                    next => early.push_back(next),
                },
            }
        }
    };
    controller.host().compaction = None;
    start
}

async fn run_session(
    controller: &mut SessionController<WorkerHost>,
    start: Start,
    inbox: &mut mpsc::UnboundedReceiver<Command>,
) -> bool {
    let finished = controller.run(start, inbox, busy_command).await;
    if let Some(tools) = controller.host().pending_plugin_tools.take() {
        set_plugin_tools(controller, tools);
    }
    refresh(controller);
    finished
}

fn set_plugin_tools(controller: &mut SessionController<WorkerHost>, tools: Vec<PluginTool>) {
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    tracing::info!("noel worker: plugin tools: {}", names.join(", "));
    controller.configure(|runtime, host| {
        runtime.set_plugin_tools(host.plugin_host.clone(), tools);
    });
}

/// Follows the server's plugin directory: its tools, as the worker's
/// runtime offers them.
fn follow_plugin_tools(plugins: Arc<RemoteRef>, commands: mpsc::UnboundedSender<Command>) {
    let mut updates = plugins.subscribe("directory");
    tokio::spawn(async move {
        let _keep = plugins;
        while let Some(update) = updates.next().await {
            let directory = match update {
                StateUpdate::Snapshot(value) | StateUpdate::Ops { value, .. } => value,
                StateUpdate::Failed(_) => continue,
            };
            let tools = directory["plugins"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|plugin| plugin["running"] == json!(true))
                .flat_map(|plugin| {
                    let name = plugin["name"].as_str().unwrap_or_default().to_owned();
                    plugin["contributions"]["tools"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |tool| PluginTool {
                            plugin: name.clone(),
                            name: tool["name"].as_str().unwrap_or_default().to_owned(),
                            description: tool["description"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                            parameters: tool["parameters"].clone(),
                            replay_safe: tool["replaySafe"] == json!(true),
                        })
                })
                .collect();
            if commands.send(Command::PluginTools(tools)).is_err() {
                return;
            }
        }
    });
}

/// Plugin tools run through the server, which forwards each call to the
/// plugin when it is granted `tools`.
struct ServerPluginTools(Arc<RemoteRef>);

impl PluginToolHost for ServerPluginTools {
    fn call(
        &self,
        plugin: &str,
        tool: &str,
        args: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PluginToolOutput> + Send>> {
        let plugins = self.0.clone();
        let args = json!({"plugin": plugin, "name": tool, "args": args.to_string()});
        Box::pin(async move {
            match plugins.call("callTool", args).await {
                Ok(result) => PluginToolOutput {
                    output: result["output"].as_str().unwrap_or_default().to_owned(),
                    is_error: result["isError"] == json!(true),
                },
                Err(error) => PluginToolOutput {
                    output: error.to_string(),
                    is_error: true,
                },
            }
        })
    }
}

/// A manual compaction, which an abort meanwhile cancels.
async fn compact(
    controller: &mut SessionController<WorkerHost>,
    focus: Option<&str>,
) -> Result<noel_core::CompactionRecord, ControllerError> {
    let cancel = controller.host().compaction.clone().unwrap_or_default();
    let status = controller.host().status.clone();
    status.update(|status| status.compacting = true);
    let result = controller.compact(focus, &cancel).await;
    status.update(|status| status.compacting = false);
    result
}

/// A command while no run is active; a run it starts is returned.
async fn idle_command(
    controller: &mut SessionController<WorkerHost>,
    command: Command,
) -> Option<Start> {
    // After `/quit` the session's extension hosts are shut down: nothing
    // new starts (#134); a client attaching again gets the next worker.
    if controller.host().status.get().phase == Phase::Leaving
        && let Command::Submit(_, reply) | Command::Run(_, reply) | Command::Resume(_, reply) =
            command
    {
        let _ = reply.send(Err(refused(
            "closing",
            "the session is closing; attach to it again to continue",
        )));
        return None;
    }
    match command {
        Command::Submit(args, reply) => {
            let (submission, _, reject_when_idle) = match submission(&args) {
                Ok(parsed) => parsed,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return None;
                }
            };
            // Durable's whenIdle: "reject": an idle session refuses the
            // input and writes nothing, so it never starts a run.
            if reject_when_idle && submission.kind != SubmissionKind::Continue {
                let _ = reply.send(Err(refused(
                    "idle",
                    "no run is active; the input was not admitted (whenIdle reject)",
                )));
                return None;
            }
            let submitted = if submission.kind == SubmissionKind::Continue {
                controller.continue_run(submission.request_id).await
            } else {
                controller.submit(submission).await
            };
            match submitted {
                Ok(Submitted::Started(start)) => {
                    let run_id = start.run_id().to_owned();
                    let _ = reply.send(Ok(json!({
                        "submissionId": run_id,
                        "runId": run_id,
                        "disposition": "started",
                    })));
                    return Some(start);
                }
                Ok(Submitted::Repeated(repeat)) => {
                    let _ = reply.send(Ok(repeated(&repeat, None)));
                }
                Err(error) => {
                    let _ = reply.send(Err(refusal(error)));
                }
            }
        }
        Command::Withdraw(args, reply) => {
            let id = WithdrawArgs::deserialize(&args)
                .map(|args| args.submission_id)
                .unwrap_or_default();
            let _ = reply.send(withdrawn(controller.withdraw(&id)));
        }
        Command::Abort(args, reply) => {
            let abort = AbortArgs::deserialize(&args).unwrap_or_default();
            let expected = abort.expected_run_id.as_deref();
            let policy = abort_policy(&args);
            let queued = match policy {
                AbortPolicy::WithdrawQueue => controller.queue().pending(),
                AbortPolicy::KeepQueue => 0,
            };
            let result = controller
                .abort_with(expected, policy)
                .map(|()| json!({"withdrawn": queued}));
            let _ = reply.send(result.map_err(|error| refused("abort", error)));
        }
        Command::Compact(args, reply) => {
            let focus = args.get("instructions").and_then(Value::as_str);
            let result = compact(controller, focus).await.map(|record| {
                json!({
                    "omittedMessages": record.omitted_messages,
                    "retainedMessages": record.retained_messages,
                })
            });
            let _ = reply.send(result.map_err(|error| refused("compact", error)));
        }
        Command::Configure(args, reply) => {
            let result = controller.configure(|runtime, host| configure(runtime, host, &args));
            let _ = reply.send(result);
        }
        Command::Resume(args, reply) => {
            let expected = text(&args, "expectedRunId");
            match controller.resume(&expected).await {
                Ok(start) => {
                    let _ = reply.send(Ok(json!({"runId": start.run_id()})));
                    return Some(start);
                }
                Err(error) => {
                    let _ = reply.send(Err(refused("resume", error)));
                }
            }
        }
        Command::Run(args, reply) => {
            // The loop holds the slot (Working) while it runs; the status
            // change it ends with also has the server read its directory
            // again (a fork, a new session).
            let (answer, start) = run_command(controller, &args).await;
            let _ = reply.send(Ok(answer));
            if start.is_some() {
                return start;
            }
        }
        Command::PluginTools(tools) => set_plugin_tools(controller, tools),
    }
    None
}

/// A command's output, as the worker answers `command`.
#[derive(Default)]
struct CommandOutput {
    entries: Vec<Value>,
    resume_pending: bool,
}

impl CommandUi for CommandOutput {
    fn push(&mut self, kind: crate::TuiEntryKind, text: impl Into<String>) {
        let kind = match kind {
            crate::TuiEntryKind::Error => "error",
            crate::TuiEntryKind::Tool => "tool",
            _ => "system",
        };
        self.entries
            .push(json!({"kind": kind, "text": text.into()}));
    }

    fn placeholder(&mut self, kind: crate::TuiEntryKind, text: impl Into<String>) -> usize {
        self.push(kind, text);
        self.entries.len() - 1
    }

    fn replace(&mut self, index: usize, text: impl Into<String>) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry["text"] = json!(text.into());
        }
    }

    fn redraw(&mut self, _config: &noel_config::LoadedConfig) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_resume_pending(&mut self, pending: bool) {
        self.resume_pending = pending;
    }

    fn switch_session(
        &mut self,
        runtime: &mut Runtime,
        config: &noel_config::LoadedConfig,
        switch: SessionSwitch,
    ) -> anyhow::Result<CommandOutcome> {
        // This worker serves one session: the client attaches to the other.
        open_elsewhere(runtime, config, switch).map(CommandOutcome::Session)
    }
}

/// `command({line, requestId?})`: runs `line` as the TUI would, on this
/// session (docs/client-tui-plan.md). The answer is `{entries, next?}`:
/// the rows to show, then what the client does next (`run` it started,
/// `selector` to open, `session` to attach to, `login` to run). A prompt
/// starts a run, returned to be driven.
/// A slash command or `!` line from a client: the entries it shows, what
/// comes next, and the run it started, if any. Each kind of command has its
/// handler; the rest go to the session commands.
async fn run_command(
    controller: &mut SessionController<WorkerHost>,
    args: &Value,
) -> (Value, Option<Start>) {
    let CommandArgs {
        line,
        request_id,
        errors,
    } = CommandArgs::deserialize(args).unwrap_or_default();
    if let Some(answer) = queue_follow_up(controller, &line, &request_id) {
        return answer;
    }
    if let Some(answer) = queue_listing(controller, &line) {
        return answer;
    }
    if let Some(answer) = shell_command(controller, &line).await {
        return answer;
    }
    if let Some(answer) = diff_command(controller, &line) {
        return answer;
    }
    if let Some(answer) = quit_command(controller, &line).await {
        return answer;
    }
    if let Some(answer) = bug_command(controller, &line, errors) {
        return answer;
    }
    session_command(controller, args, line).await
}

/// `/queue <text>`: a follow-up queued while the session runs or not.
fn queue_follow_up(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
    request_id: &Option<String>,
) -> Option<(Value, Option<Start>)> {
    if let Some(text) = line
        .strip_prefix("/queue ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let content = controller.parts().1.expand(text);
        let submission = Submission {
            kind: SubmissionKind::FollowUp,
            request_id: request_id.clone(),
            content,
            images: Vec::new(),
            expected_run_id: None,
        };
        let answer = match controller.enqueue(noel_core::QueueMode::FollowUp, submission) {
            Ok(_) => format!("queued: {}", controller.queue().pending()),
            Err(error) => error.to_string(),
        };
        refresh(controller);
        return Some((
            json!({"entries": [{"kind": "system", "text": answer}]}),
            None,
        ));
    }
    None
}

/// `/queue`, `/queue-clear` and `/interrupt`.
fn queue_listing(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
) -> Option<(Value, Option<Start>)> {
    let mut output = CommandOutput::default();
    let queued = match line {
        "/queue" => Some(Ok(queue_text(&controller.queue().items()))),
        "/queue-clear" | "/interrupt" => Some(controller.clear_queue().map(|items| {
            let cleared = format!("cleared {} queued message(s)", items.len());
            if line == "/interrupt" {
                format!("interrupted; {cleared}")
            } else {
                cleared
            }
        })),
        _ => None,
    };
    if let Some(result) = queued {
        match result {
            Ok(text) => output.push(crate::TuiEntryKind::System, text),
            Err(error) => output.push(crate::TuiEntryKind::Error, error),
        }
        refresh(controller);
        return Some((json!({"entries": output.entries}), None));
    }
    None
}

/// `!command`.
async fn shell_command(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
) -> Option<(Value, Option<Start>)> {
    // `!command`: a shell command in the session's environment, its output
    // shown and not sent to the model (the client resolves `!!`).
    if let Some(command) = line
        .strip_prefix('!')
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let command = command.to_owned();
        let (runtime, _) = controller.parts_mut();
        let entry = match crate::run_excluded_bash_result(runtime, command.clone()).await {
            Ok(result) => {
                let text = crate::format_bash_completed(&command, &result.output);
                let text = if result.is_error {
                    text.replacen("completed", "failed", 1)
                } else {
                    text
                };
                json!({"kind": "tool", "text": text})
            }
            Err(error) => {
                json!({"kind": "error", "text": format!("failed bash\n$ {command}\n{error}")})
            }
        };
        return Some((json!({"entries": [entry]}), None));
    }
    None
}

/// `/diff`.
fn diff_command(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
) -> Option<(Value, Option<Start>)> {
    // The diff panel: the edited files and each one's diff, read here.
    if line == "/diff" {
        let session = controller.runtime().session();
        let files = crate::collect_diff_file_entries(&session.cwd, &session.edited_files);
        let files = files.iter().map(|entry| {
            let detail = crate::build_diff_detail(&session.cwd, entry);
            json!({
                "path": entry.path,
                "added": entry.added,
                "removed": entry.removed,
                "untracked": entry.untracked,
                "lines": detail.lines,
            })
        });
        let files = files.collect::<Vec<_>>();
        return Some((
            json!({"entries": [], "next": {"type": "diff", "files": files}}),
            None,
        ));
    }
    None
}

/// `/quit`, `/exit` and `/q`.
async fn quit_command(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
) -> Option<(Value, Option<Start>)> {
    // A local `noel` quitting (docs/client-tui-plan.md): with no other
    // client and no run, the session ends as the in-process TUI's does,
    // its extensions told so; otherwise the client only detaches.
    if matches!(line, "/quit" | "/exit" | "/q") {
        let (runtime, host) = controller.parts_mut();
        let alone = host.status.get().holds <= 1;
        let mut entries = Vec::new();
        if alone
            && runtime
                .session()
                .runs
                .last()
                .is_none_or(|run| run.status.is_finished())
        {
            let cwd = runtime.session().cwd.clone();
            let mut diagnostics =
                crate::notify_extension_lifecycle(&host.config.extensions, "shutdown", &cwd);
            {
                let hosts = runtime.extension_hosts();
                let mut hosts = hosts.lock().await;
                diagnostics.extend(hosts.notify("shutdown", &cwd).await);
                hosts.shutdown(&cwd).await;
            }
            entries.extend(
                diagnostics
                    .into_iter()
                    .map(|text| json!({"kind": "error", "text": text})),
            );
            host.status.update(|status| status.phase = Phase::Leaving);
            return Some((json!({"entries": entries, "leaving": true}), None));
        }
        return Some((json!({"entries": entries}), None));
    }
    None
}

/// `/bug`.
fn bug_command(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
    errors: Option<Vec<String>>,
) -> Option<(Value, Option<Start>)> {
    // `/bug`: the report goes into the session's directory, with the error
    // rows the client shows (`errors`).
    if line.split_whitespace().next() == Some("/bug") {
        let errors = errors.unwrap_or_default();
        let errors = errors.iter().map(String::as_str).collect::<Vec<_>>();
        let (runtime, host) = controller.parts();
        let dir = std::env::current_dir().unwrap_or_else(|_| runtime.session().cwd.clone());
        let entry = match crate::bug_report::report_into(runtime, &host.config, line, &errors, &dir)
        {
            Ok(text) => json!({"kind": "system", "text": text}),
            Err(text) => json!({"kind": "error", "text": text}),
        };
        return Some((json!({"entries": [entry]}), None));
    }
    None
}

/// A session switch, or a command `session_commands` runs, and what its
/// outcome asks of the controller.
async fn session_command(
    controller: &mut SessionController<WorkerHost>,
    args: &Value,
    line: String,
) -> (Value, Option<Start>) {
    let mut output = CommandOutput::default();
    let mut next = Value::Null;
    let mut start = None;
    if let Some(switch) = selected_session(controller, &line) {
        match switch {
            Ok(session) => next = json!({"type": "session", "session": session}),
            Err(error) => output.push(crate::TuiEntryKind::Error, format!("{error:#}")),
        }
        return (json!({"entries": output.entries, "next": next}), None);
    }
    let outcome = {
        let (runtime, host) = controller.parts_mut();
        let offline = host.offline;
        session_commands::run(
            &mut output,
            runtime,
            &mut host.config,
            offline,
            line.clone(),
        )
        .await
    };
    // A rewind dropped entries the view holds: it is built again.
    if line == "/undo" || line.starts_with("/select rewind ") {
        refresh(controller);
    }
    match outcome {
        Ok(CommandOutcome::Handled) => {}
        Ok(CommandOutcome::Submit { prompt, media }) => {
            if !media.is_empty() {
                output.push(
                    crate::TuiEntryKind::Error,
                    "images are sent with the prompt",
                );
            } else {
                let submission = Submission {
                    kind: SubmissionKind::Prompt,
                    request_id: args
                        .get("requestId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    content: prompt.prompt,
                    images: Vec::new(),
                    expected_run_id: None,
                };
                match controller.submit(submission).await {
                    Ok(Submitted::Started(started)) => {
                        next = json!({"type": "run", "runId": started.run_id()});
                        start = Some(started);
                    }
                    Ok(Submitted::Repeated(repeat)) => {
                        next = json!({"type": "repeated", "result": repeated(&repeat, None)});
                    }
                    Err(error) => output.push(crate::TuiEntryKind::Error, error.to_string()),
                }
            }
        }
        Ok(CommandOutcome::Compact { focus }) => {
            let focus = Some(focus.as_str()).filter(|focus| !focus.is_empty());
            match compact(controller, focus).await {
                Ok(record) => output.push(
                    crate::TuiEntryKind::System,
                    format!(
                        "compacted: {} messages summarized, {} kept",
                        record.omitted_messages, record.retained_messages
                    ),
                ),
                Err(error) => output.push(crate::TuiEntryKind::Error, error),
            }
        }
        Ok(CommandOutcome::Login { provider, account }) => {
            next = json!({"type": "login", "provider": provider, "account": account});
        }
        Ok(CommandOutcome::Selector(state)) => {
            let mut selector = state.to_json();
            // The model picker's provider layer carries each provider's
            // models, so the client opens them at once, in key order.
            if state.kind == "provider" {
                let (runtime, host) = controller.parts();
                selector["layers"] = crate::provider_model_layers(&host.config, runtime);
            }
            next = json!({"type": "selector", "selector": selector});
        }
        Ok(CommandOutcome::Draft { text, media }) => {
            next = json!({"type": "draft", "text": text, "media": media});
        }
        Ok(CommandOutcome::Session(session)) => {
            next = json!({"type": "session", "session": session});
        }
        Err(error) => output.push(crate::TuiEntryKind::Error, format!("{error:#}")),
    }
    refresh(controller);
    let mut answer = json!({"entries": output.entries});
    if output.resume_pending {
        answer["resumePending"] = json!(true);
    }
    if !next.is_null() {
        answer["next"] = next;
    }
    (answer, start)
}

/// The queue as `/queue` lists it, as the in-process TUI does.
fn queue_text(items: &[noel_core::QueueItem]) -> String {
    if items.is_empty() {
        return "queue is empty".to_owned();
    }
    let lines = items
        .iter()
        .enumerate()
        .map(|(index, item)| format!("{}.\t{}", index + 1, item.content));
    lines.collect::<Vec<_>>().join("\n")
}

/// The session `switch` names, for the client to attach to. A journal
/// another process holds is refused, as `noel` refuses it (its own worker
/// holding it is no refusal); one outside the session directory is
/// registered with the server, which looks there.
fn open_elsewhere(
    runtime: &Runtime,
    config: &noel_config::LoadedConfig,
    switch: SessionSwitch,
) -> anyhow::Result<String> {
    let SessionSwitch::Open { path } = &switch else {
        return session_commands::switch_elsewhere(runtime, config, switch);
    };
    let session = SessionStore::read(path)?.session_id;
    match SessionStore::open(path.clone()) {
        Ok(opened) => drop(opened),
        Err(SessionError::Locked { pid: Some(pid), .. })
            if crate::held_by_worker(path, pid).as_deref() == Some(session.as_str()) => {}
        Err(error) => return Err(error.into()),
    }
    let dir = RuntimeDir::check(RuntimeDir::locate())?;
    let registered = dir.worker_journal(&session);
    let canonical = fs::canonicalize(path)?;
    if fs::canonicalize(config.paths.session_dir.join(format!("{session}.jsonl"))).ok()
        == Some(canonical.clone())
    {
        let _ = fs::remove_file(registered);
    } else {
        fs::write(registered, format!("{}\n", canonical.display()))?;
    }
    Ok(session)
}

/// A pick of the session or branch selector names another session here:
/// the client attaches to it.
fn selected_session(
    controller: &mut SessionController<WorkerHost>,
    line: &str,
) -> Option<anyhow::Result<String>> {
    let rest = line.strip_prefix("/select ")?.trim();
    let (kind, value) = rest.split_once(char::is_whitespace)?;
    if !matches!(
        kind,
        "session" | "sessions" | "resume" | "tree" | "branches"
    ) {
        return None;
    }
    let (runtime, host) = controller.parts();
    Some(
        crate::resolve_session_reference(&host.config.paths.session_dir, value.trim())
            .and_then(|path| open_elsewhere(runtime, &host.config, SessionSwitch::Open { path })),
    )
}

/// A command while a run is active.
fn busy_command(command: Command, busy: &mut Busy<'_, WorkerHost>) {
    match command {
        Command::Submit(args, reply) => {
            let result = submission(&args).and_then(|(submission, when_busy, _)| {
                let active = busy.run_id().to_owned();
                match busy.submit(submission, when_busy) {
                    Ok(Enqueued::Queued(id)) => Ok(json!({
                        "submissionId": id.submission_id,
                        "disposition": "queued",
                    })),
                    Ok(Enqueued::Repeated(repeat)) => Ok(repeated(&repeat, Some(&active))),
                    Err(error) => Err(refusal(error)),
                }
            });
            let _ = reply.send(result);
        }
        Command::Withdraw(args, reply) => {
            let id = WithdrawArgs::deserialize(&args)
                .map(|args| args.submission_id)
                .unwrap_or_default();
            let _ = reply.send(withdrawn(busy.withdraw(&id)));
        }
        Command::Abort(args, reply) => {
            let abort = AbortArgs::deserialize(&args).unwrap_or_default();
            let expected = abort.expected_run_id.as_deref();
            let policy = abort_policy(&args);
            let queued = match policy {
                AbortPolicy::WithdrawQueue => busy.queue().pending(),
                AbortPolicy::KeepQueue => 0,
            };
            match busy.abort_with(expected, policy) {
                // Answered once the run settles.
                Ok(()) => busy.host().aborts.push((reply, queued)),
                Err(error) => {
                    let _ = reply.send(Err(refused("abort", error)));
                }
            }
        }
        Command::Compact(_, reply)
        | Command::Configure(_, reply)
        | Command::Resume(_, reply)
        | Command::Run(_, reply) => {
            let _ = reply.send(Err(refused(
                "busy",
                "a run is active; wait for it to settle",
            )));
        }
        Command::PluginTools(tools) => busy.host().pending_plugin_tools = Some(tools),
    }
}

fn text(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `submit`'s arguments as a submission: `whenBusy` names its kind, as
/// What an abort does to the queue: Durable's rule, everything queued is
/// withdrawn, unless `keepQueue` (`noel`'s interrupt, which keeps it).
fn abort_policy(args: &Value) -> AbortPolicy {
    if AbortArgs::deserialize(args).is_ok_and(|args| args.keep_queue) {
        AbortPolicy::KeepQueue
    } else {
        AbortPolicy::WithdrawQueue
    }
}

/// `prompt`, `steer` and `follow_up` do over RPC; `type: "continue"` is
/// Durable's continue. The flag is `whenIdle: "reject"`. The arguments are
/// [`SubmitArgs`], the type the client sends (#164).
fn submission(args: &Value) -> Result<(Submission, WhenBusy, bool), WireError> {
    let args = SubmitArgs::deserialize(args)
        .map_err(|error| WireError::from(ChordError::invalid("args", error.to_string())))?;
    let reject_when_idle = args.when_idle.is_some();
    if args.kind == Some(SubmitType::Continue) {
        return Ok((
            Submission {
                kind: SubmissionKind::Continue,
                request_id: Some(args.request_id),
                content: String::new(),
                images: Vec::new(),
                expected_run_id: None,
            },
            WhenBusy::Reject,
            reject_when_idle,
        ));
    }
    let Some(content) = args.text else {
        return Err(ChordError::invalid("args", "an input needs its text").into());
    };
    let when_busy = args.when_busy.unwrap_or_default();
    let kind = match when_busy {
        protocol::WhenBusy::Reject => SubmissionKind::Prompt,
        protocol::WhenBusy::Steer => SubmissionKind::Steer,
        protocol::WhenBusy::FollowUp => SubmissionKind::FollowUp,
    };
    let images = args
        .images
        .into_iter()
        .map(|image| Image {
            data: image.data,
            mime_type: image.mime_type,
        })
        .collect();
    Ok((
        Submission {
            kind,
            request_id: Some(args.request_id),
            content,
            images,
            expected_run_id: None,
        },
        when_busy.core(),
        reject_when_idle,
    ))
}

/// A repeated `requestId`'s answer: the first submission and where it is.
fn repeated(repeat: &Repeat, active: Option<&str>) -> Value {
    let (run_id, status) = match &repeat.state {
        SubmissionState::Queued(_) => (None, "queued"),
        SubmissionState::Placed { run_id } => (
            Some(run_id.as_str()),
            if active == Some(run_id.as_str()) {
                "running"
            } else {
                "interrupted"
            },
        ),
        SubmissionState::Settled { run_id, status } => (Some(run_id.as_str()), run_status(*status)),
        SubmissionState::Withdrawn => (None, "withdrawn"),
    };
    let mut answer = json!({
        "submissionId": repeat.submission_id,
        "disposition": "repeated",
        "status": status,
    });
    if let Some(run_id) = run_id {
        answer["runId"] = json!(run_id);
    }
    answer
}

fn run_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "running",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
    }
}

fn withdrawn(result: Result<Withdrawal, ControllerError>) -> Result<Value, WireError> {
    let outcome = match result.map_err(|error| refused("withdraw", error))? {
        Withdrawal::Withdrawn => "withdrawn",
        Withdrawal::AlreadyPlaced => "already_placed",
        Withdrawal::Settled => "settled",
    };
    Ok(json!({ "outcome": outcome }))
}

/// A refused submission. Nothing was written, so none of these is
/// `unavailable`: retrying the same submission cannot succeed.
fn refusal(error: SubmitError) -> WireError {
    let reason = match &error {
        SubmitError::Busy => "busy",
        SubmitError::QueuedPrompt => "queued_prompt",
        SubmitError::Reused { .. } => "request_id_reused",
        SubmitError::Refused(_) => "refused",
    };
    let message = match error {
        SubmitError::Busy => {
            "a run is active; submit with whenBusy steer or followUp to queue".to_owned()
        }
        error => error.to_string(),
    };
    refused(reason, message)
}

/// `configure`: the session's model, then its thinking level, as RPC's
/// `set_model` and `set_thinking_level` change them.
fn configure(
    runtime: &mut Runtime,
    host: &mut WorkerHost,
    args: &Value,
) -> Result<Value, WireError> {
    let invalid = |error: anyhow::Error| refused("configure", error.to_string());
    let level = args.get("thinkingLevel").and_then(Value::as_str);
    if let Some(level) = level
        && crate::normalized_thinking_level(level).is_none()
    {
        return Err(refused(
            "configure",
            format!("Unknown thinking level: {level}"),
        ));
    }
    // Durable's `configure({compaction})`: replaces the conversation's
    // settings whole; `null` clears them (upstream 922c3ff2).
    let compaction = match args.get("compaction") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(
            serde_json::from_value::<noel_config::CompactionSettings>(value.clone()).map_err(
                |error| refused("configure", format!("Invalid compaction settings: {error}")),
            )?,
        )),
    };
    if let Some(Some(settings)) = &compaction {
        let percents = std::iter::once(settings.trigger_percent).chain(
            settings
                .model_overrides
                .iter()
                .flatten()
                .map(|(_, fields)| fields.trigger_percent),
        );
        if percents
            .flatten()
            .any(|percent| !(1..=99).contains(&percent))
        {
            return Err(refused(
                "configure",
                "compaction triggerPercent must be from 1 to 99",
            ));
        }
    }
    if let Some(settings) = compaction {
        runtime
            .set_compaction_settings(settings)
            .map_err(|error| refused("configure", error.to_string()))?;
    }
    if let Some(reference) = args.get("model").and_then(Value::as_str) {
        let model = model_reference(&host.config, reference, host.offline)
            .ok_or_else(|| refused("configure", format!("Model not found: {reference}")))?;
        crate::switch_model(runtime, &host.config, &model, level).map_err(invalid)?;
    } else if let Some(level) = level {
        crate::set_thinking_level(runtime, &host.config, level).map_err(invalid)?;
    }
    let agent = view::agent(runtime, &host.config);
    lock(&host.view).set_agent(agent.clone());
    let mut answer = json!({"thinkingLevel": agent["thinkingLevel"]});
    if let Some(model) = agent.get("model") {
        answer["model"] = model.clone();
    }
    if let Some(compaction) = agent.get("compaction") {
        answer["compaction"] = compaction.clone();
    }
    Ok(answer)
}

/// A model a client names: `provider/id` of a configured model that can
/// run now, or a reference the model picker accepts.
pub(crate) fn model_reference(
    config: &LoadedConfig,
    reference: &str,
    offline: bool,
) -> Option<ModelRef> {
    if let Some((provider, id)) = reference.split_once('/') {
        let model = ModelRef {
            provider: provider.to_owned(),
            id: id.to_owned(),
        };
        if crate::provider_usable(config, provider, offline)
            && crate::configured_model(config, &model).is_some()
        {
            return Some(model);
        }
    }
    crate::find_selectable_model(config, reference, offline)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #134: one function decides the exit, and a start in hand blocks it.
    #[test]
    fn the_worker_exits_only_with_nothing_in_hand() {
        let limit = Duration::from_secs(60);
        let draining = Status {
            phase: Phase::Draining,
            ..Status::default()
        };
        assert_eq!(
            draining.exit_reason(None, limit),
            Some("shut down by the server")
        );
        // The race: a start accepted and not running yet is not idle.
        for slot in [Slot::Working, Slot::Starting, Slot::Running("r1".into())] {
            let busy = Status {
                slot: slot.clone(),
                ..draining.clone()
            };
            assert_eq!(busy.exit_reason(Some(limit), limit), None, "{slot:?}");
            assert!(!busy.idle(), "{slot:?}");
        }
        let leaving = Status {
            phase: Phase::Leaving,
            holds: 1,
            ..Status::default()
        };
        assert_eq!(leaving.exit_reason(None, limit), None);
        let left = Status {
            holds: 0,
            ..leaving
        };
        assert_eq!(left.exit_reason(None, limit), Some("its last client quit"));
        let serving = Status::default();
        assert_eq!(
            serving.exit_reason(Some(Duration::from_secs(1)), limit),
            None
        );
        assert_eq!(serving.exit_reason(Some(limit), limit), Some("idle"));
        // Starting reports as working to the server.
        let starting = Status {
            slot: Slot::Starting,
            ..Status::default()
        };
        assert_eq!(starting.control()["working"], true);
        assert_eq!(starting.control()["running"], false);
    }

    #[test]
    fn when_busy_names_the_kind_of_a_submission() {
        let parse = |when: Value| {
            let mut args = json!({"requestId": "r", "text": "hi"});
            if !when.is_null() {
                args["whenBusy"] = when;
            }
            submission(&args).map(|(submission, when_busy, _)| (submission.kind, when_busy))
        };
        assert_eq!(
            parse(Value::Null).unwrap(),
            (SubmissionKind::Prompt, WhenBusy::Reject)
        );
        assert_eq!(
            parse(json!("steer")).unwrap(),
            (SubmissionKind::Steer, WhenBusy::Steer)
        );
        assert_eq!(
            parse(json!("followUp")).unwrap(),
            (SubmissionKind::FollowUp, WhenBusy::FollowUp)
        );
        assert!(parse(json!("later")).is_err());
    }

    #[test]
    fn a_continue_and_when_idle_are_read_from_submit() {
        let (continuing, _, reject) =
            submission(&json!({"requestId": "r", "type": "continue"})).unwrap();
        assert_eq!((continuing.kind, reject), (SubmissionKind::Continue, false));
        let (_, _, reject) =
            submission(&json!({"requestId": "r", "text": "hi", "whenIdle": "reject"})).unwrap();
        assert!(reject);
        assert!(submission(&json!({"requestId": "r"})).is_err());
        assert!(submission(&json!({"requestId": "r", "type": "later"})).is_err());
        assert!(submission(&json!({"requestId": "r", "text": "x", "whenIdle": "start"})).is_err());
    }

    #[test]
    fn refusals_are_never_retryable() {
        for error in [
            SubmitError::Busy,
            SubmitError::QueuedPrompt,
            SubmitError::Refused("no credential".into()),
        ] {
            let error = refusal(error);
            assert!(matches!(error, WireError::Plugin(_)), "{error}");
        }
        assert!(refusal(SubmitError::Busy).is_plugin("busy"));
    }

    #[test]
    fn a_repeat_says_where_its_first_submission_is() {
        let repeat = |state| Repeat {
            submission_id: "s".into(),
            kind: SubmissionKind::Prompt,
            state,
        };
        let placed = repeat(SubmissionState::Placed {
            run_id: "r1".into(),
        });
        assert_eq!(repeated(&placed, Some("r1"))["status"], "running");
        assert_eq!(repeated(&placed, None)["status"], "interrupted");
        let settled = repeat(SubmissionState::Settled {
            run_id: "r1".into(),
            status: RunStatus::Completed,
        });
        assert_eq!(
            repeated(&settled, None),
            json!({"submissionId": "s", "disposition": "repeated", "status": "completed", "runId": "r1"})
        );
        assert_eq!(
            repeated(&repeat(SubmissionState::Withdrawn), None)["status"],
            "withdrawn"
        );
    }
}
