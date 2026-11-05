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
use std::sync::atomic::{AtomicBool, Ordering};
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
    AbortPolicy, Busy, Enqueued, Event, Host as ControllerHost, Image, Repeat, SessionController,
    Start, Submission, SubmissionState, SubmitError, Submitted, WhenBusy, Withdrawal,
};
use noel_core::plugins::{PluginTool, PluginToolHost, PluginToolOutput};
use noel_core::{
    expand_input, ReloadableSystems, RunStatus, Runtime, SessionError, SessionStore, SubmissionKind,
};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Notify};

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
        Ok(never) => match never {},
        Err(fatal) => {
            let mut message = json!({"type": "fatal", "message": fatal.message});
            if let Some(reason) = fatal.reason {
                message["reason"] = json!(reason);
            }
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(control_line(&message).as_bytes());
            let _ = stdout.flush();
            eprintln!("noel worker: {}", fatal.message);
            fatal.code
        }
    }
}

enum Never {}

async fn start(args: WorkerArgs) -> Result<Never, Fatal> {
    let session_id = args.session.clone();
    if !valid_session_id(&session_id) {
        return Err(Fatal::new(format!("invalid session id {session_id:?}")));
    }
    let dir = RuntimeDir::check(RuntimeDir::locate())?;
    dir.ensure_workers()?;
    // The server names its session directory in NOEL_SESSION_DIR.
    let here = std::env::current_dir().unwrap_or_else(|_| "/".into());
    let session_dir = crate::config_paths(here, None)?.session_dir;
    let journal = session_dir.join(format!("{session_id}.jsonl"));
    if !journal.exists() {
        return Err(Fatal::new(format!("no session {session_id}")));
    }
    let (store, state) = match SessionStore::open(journal) {
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
            std::env::set_var(key, value);
        }
    }
    let launch: Vec<String> = serde_json::from_value(launched["args"].clone()).unwrap_or_default();
    let cli = crate::Cli::try_parse_from(std::iter::once("noel".to_owned()).chain(launch.clone()))
        .unwrap_or_else(|_| crate::Cli::parse_from(["noel"]));
    let _ = fs::write(
        dir.worker_applied_args(&session_id),
        serde_json::to_string(&launch).unwrap_or_default(),
    );
    let cwd = state.cwd.clone();
    if let Err(error) = std::env::set_current_dir(&cwd) {
        eprintln!(
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
    let selection = crate::resolve_cli_selection(&cli, &mut config, offline)?;
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
    };
    let mut controller = SessionController::new(runtime, worker, AbortPolicy::WithdrawQueue);
    refresh(&mut controller);

    let parent: Arc<Mutex<Option<ControlSender>>> = Arc::default();
    let draining = Arc::new(AtomicBool::new(false));
    tokio::spawn(accept_parents(
        listener,
        host.clone(),
        ready.clone(),
        parent.clone(),
        status.clone(),
        draining.clone(),
    ));
    announce_ready(&ready);
    eprintln!(
        "noel worker: session {session_id} generation {} ready (pid {})",
        args.generation,
        std::process::id()
    );
    tokio::spawn(supervise(Supervision {
        status,
        parent,
        draining,
        idle_limit,
        host,
        files: Files { socket, record },
    }));
    drive(controller, inbox, args.resume).await;
    // The controller only stops when every command sender is gone, which
    // the served object keeps alive: wait for the supervisor to exit.
    loop {
        std::future::pending::<()>().await;
    }
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
    /// The active run.
    running: Option<String>,
    queued: usize,
    /// A command that runs no run (a compaction, a configure) is in hand.
    working: bool,
    /// The last client quit, as a local `noel` quits: exit once idle.
    leaving: bool,
}

impl Status {
    fn idle(&self) -> bool {
        self.holds == 0 && self.running.is_none() && self.queued == 0 && !self.working
    }

    fn control(&self) -> Value {
        let mut message = json!({
            "type": "status",
            "viewers": self.holds,
            "running": self.running.is_some(),
            "queued": self.queued,
            "working": self.working,
        });
        if let Some(run_id) = &self.running {
            message["runId"] = json!(run_id);
        }
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
        HostEvent::Closed { .. } => eprintln!("noel worker: link closed"),
        HostEvent::ProtocolError { error, .. } => eprintln!("noel worker: chord: {error}"),
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
) -> impl Fn(Value, CallContext) -> BoxFuture<Result<Value, WireError>> + Send + Sync + 'static {
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
    ) -> impl Fn(Value, CallContext) -> BoxFuture<Result<Value, WireError>> + Send + Sync + 'static
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
    draining: Arc<AtomicBool>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                eprintln!("noel worker: accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Err(reason) = peer_check(&stream) {
            eprintln!("noel worker: refused a connection: {reason}");
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
        eprintln!("noel worker: a server connected");
        let (status, draining) = (status.clone(), draining.clone());
        tokio::spawn(async move {
            while let Some(message) = controls.recv().await {
                if message.get("type").and_then(Value::as_str) == Some("shutdown") {
                    eprintln!("noel worker: asked to shut down once no run is active");
                    draining.store(true, Ordering::SeqCst);
                    status.changed.notify_one();
                }
            }
            eprintln!("noel worker: the server's link ended");
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
    draining: Arc<AtomicBool>,
    idle_limit: Duration,
    host: Host,
    files: Files,
}

/// Reports status to the parent, and ends the worker: idle for the limit,
/// asked to shut down with no run active, or by a signal.
async fn supervise(supervision: Supervision) {
    use tokio::signal::unix::{signal, SignalKind};
    let Supervision {
        status,
        parent,
        draining,
        idle_limit,
        host,
        files,
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
            eprintln!("noel worker: signal {signal}; exiting");
            noel_core::kill_running_tool_groups();
            files.remove();
            host.close().await;
            std::process::exit(128 + signal);
        }
        let now = status.get();
        let report = now.control();
        if sent.as_ref() != Some(&report) {
            if let Some(parent) = &*lock(&parent) {
                parent.send(&report);
            }
            sent = Some(report);
        }
        let idle_since = status.idle_since();
        let done = if draining.load(Ordering::SeqCst) && now.running.is_none() && !now.working {
            Some("shut down by the server")
        } else if now.leaving && now.running.is_none() && !now.working {
            Some("its last client quit")
        } else if idle_since.is_some_and(|since| since.elapsed() >= idle_limit) {
            Some("idle")
        } else {
            None
        };
        if let Some(why) = done {
            eprintln!("noel worker: exiting ({why})");
            if let Some(parent) = &*lock(&parent) {
                parent.send(&json!({"type": "shutdown"}));
            }
            files.remove();
            host.close().await;
            std::process::exit(0);
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
            eprintln!("noel worker: {notice}");
        }
        match &event {
            Event::RunStarted { run_id, .. } => {
                let run_id = (*run_id).to_owned();
                self.status.update(|status| status.running = Some(run_id));
            }
            Event::QueueChanged { queue } | Event::CallReturned { queue, .. } => {
                let pending = queue.pending();
                self.status.update(|status| status.queued = pending);
            }
            Event::SaveFailed { error } => eprintln!("noel worker: {error}"),
            _ => {}
        }
        lock(&self.view).event(&event);
        if let Event::RunSettled { run_id, outcome } = &event {
            eprintln!("noel worker: run {run_id} settled: {:?}", outcome.status());
            self.status.update(|status| status.running = None);
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
                eprintln!("noel worker: resuming run {run_id}");
                if !run_session(&mut controller, start, &mut inbox).await {
                    return;
                }
            }
            Err(error) => eprintln!("noel worker: run {run_id} not resumed: {error}"),
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
    loop {
        let command = match early.pop_front() {
            Some(command) => command,
            None => match inbox.recv().await {
                Some(command) => command,
                None => break,
            },
        };
        controller
            .host()
            .status
            .update(|status| status.working = true);
        let start = idle_command(&mut controller, command).await;
        refresh(&mut controller);
        controller
            .host()
            .status
            .update(|status| status.working = false);
        if let Some(start) = start {
            if !run_session(&mut controller, start, &mut inbox).await {
                return;
            }
        }
    }
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
    eprintln!("noel worker: plugin tools: {}", names.join(", "));
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

/// A command while no run is active; a run it starts is returned.
async fn idle_command(
    controller: &mut SessionController<WorkerHost>,
    command: Command,
) -> Option<Start> {
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
            let id = text(&args, "submissionId");
            let _ = reply.send(withdrawn(controller.withdraw(&id)));
        }
        Command::Abort(args, reply) => {
            let expected = args.get("expectedRunId").and_then(Value::as_str);
            let queued = controller.queue().pending();
            let result = controller
                .abort(expected)
                .map(|()| json!({"withdrawn": queued}));
            let _ = reply.send(result.map_err(|error| refused("abort", error)));
        }
        Command::Compact(args, reply) => {
            let focus = args.get("instructions").and_then(Value::as_str);
            let result = controller.compact(focus).await.map(|record| {
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
            // Working while it runs; the status change it ends with also
            // has the server read its directory again (a fork, a new
            // session).
            let status = controller.parts().1.status.clone();
            status.update(|status| status.working = true);
            let (answer, start) = run_command(controller, &args).await;
            status.update(|status| status.working = false);
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
        session_commands::switch_elsewhere(runtime, config, switch).map(CommandOutcome::Session)
    }
}

/// `command({line, requestId?})`: runs `line` as the TUI would, on this
/// session (docs/client-tui-plan.md). The answer is `{entries, next?}`:
/// the rows to show, then what the client does next (`run` it started,
/// `selector` to open, `session` to attach to, `login` to run). A prompt
/// starts a run, returned to be driven.
async fn run_command(
    controller: &mut SessionController<WorkerHost>,
    args: &Value,
) -> (Value, Option<Start>) {
    let line = text(args, "line");
    let mut output = CommandOutput::default();
    let mut next = Value::Null;
    let mut start = None;
    // The queue is the controller's; the session's own fields are not.
    if let Some(text) = line
        .strip_prefix("/queue ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let content = controller.parts().1.expand(text);
        let submission = Submission {
            kind: SubmissionKind::FollowUp,
            request_id: args
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            content,
            images: Vec::new(),
            expected_run_id: None,
        };
        let answer = match controller.enqueue(noel_core::QueueMode::FollowUp, submission) {
            Ok(_) => format!("queued: {}", controller.queue().pending()),
            Err(error) => error.to_string(),
        };
        refresh(controller);
        return (
            json!({"entries": [{"kind": "system", "text": answer}]}),
            None,
        );
    }
    let queued = match line.as_str() {
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
        return (json!({"entries": output.entries}), None);
    }
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
        return (json!({"entries": [entry]}), None);
    }
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
        return (
            json!({"entries": [], "next": {"type": "diff", "files": files}}),
            None,
        );
    }
    // A local `noel` quitting (docs/client-tui-plan.md): with no other
    // client and no run, the session ends as the in-process TUI's does,
    // its extensions told so; otherwise the client only detaches.
    if matches!(line.as_str(), "/quit" | "/exit" | "/q") {
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
            host.status.update(|status| status.leaving = true);
        }
        return (json!({"entries": entries}), None);
    }
    if line == "/undo" {
        // The prompt goes back to the client's draft.
        let undone = controller.configure(|runtime, _| runtime.undo_last_turn());
        let answer = match undone {
            Ok(Some(prompt)) => json!({
                "entries": [{"kind": "system", "text": "undid the last turn"}],
                "next": {"type": "draft", "text": prompt.content, "media": prompt.media},
            }),
            Ok(None) => json!({"entries": [{"kind": "system", "text": "nothing to undo"}]}),
            Err(error) => json!({"entries": [{"kind": "error", "text": error.to_string()}]}),
        };
        refresh(controller);
        return (answer, None);
    }
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
        session_commands::run(&mut output, runtime, &mut host.config, offline, line).await
    };
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
            match controller.compact(focus).await {
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
        crate::resolve_session_reference(&host.config.paths.session_dir, value.trim()).and_then(
            |path| {
                session_commands::switch_elsewhere(
                    runtime,
                    &host.config,
                    SessionSwitch::Open { path, resume: true },
                )
            },
        ),
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
            let id = text(&args, "submissionId");
            let _ = reply.send(withdrawn(busy.withdraw(&id)));
        }
        Command::Abort(args, reply) => {
            let expected = args.get("expectedRunId").and_then(Value::as_str);
            let queued = busy.queue().pending();
            match busy.abort(expected) {
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
/// `prompt`, `steer` and `follow_up` do over RPC; `type: "continue"` is
/// Durable's continue. The flag is `whenIdle: "reject"`.
fn submission(args: &Value) -> Result<(Submission, WhenBusy, bool), WireError> {
    let reject_when_idle = match args.get("whenIdle").and_then(Value::as_str) {
        None => false,
        Some("reject") => true,
        Some(other) => {
            return Err(
                ChordError::invalid("args", format!("whenIdle {other:?} is not reject")).into(),
            )
        }
    };
    let continuing = match args.get("type").and_then(Value::as_str) {
        None | Some("input") => false,
        Some("continue") => true,
        Some(other) => {
            return Err(ChordError::invalid(
                "args",
                format!("type {other:?} is not input or continue"),
            )
            .into())
        }
    };
    if continuing {
        return Ok((
            Submission {
                kind: SubmissionKind::Continue,
                request_id: Some(text(args, "requestId")),
                content: String::new(),
                images: Vec::new(),
                expected_run_id: None,
            },
            WhenBusy::Reject,
            reject_when_idle,
        ));
    }
    if !args.get("text").is_some_and(Value::is_string) {
        return Err(ChordError::invalid("args", "an input needs its text").into());
    }
    let (kind, when_busy) = match args.get("whenBusy").and_then(Value::as_str) {
        None | Some("reject") => (SubmissionKind::Prompt, WhenBusy::Reject),
        Some("steer") => (SubmissionKind::Steer, WhenBusy::Steer),
        Some("followUp") => (SubmissionKind::FollowUp, WhenBusy::FollowUp),
        Some(other) => {
            return Err(ChordError::invalid(
                "args",
                format!("whenBusy {other:?} is not reject, steer or followUp"),
            )
            .into())
        }
    };
    let images = args
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .map(|image| Image {
                    data: text(image, "data"),
                    mime_type: text(image, "mimeType"),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((
        Submission {
            kind,
            request_id: Some(text(args, "requestId")),
            content: text(args, "text"),
            images,
            expected_run_id: None,
        },
        when_busy,
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

fn withdrawn(result: Result<Withdrawal, String>) -> Result<Value, WireError> {
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
    if let Some(level) = level {
        if crate::normalized_thinking_level(level).is_none() {
            return Err(refused(
                "configure",
                format!("Unknown thinking level: {level}"),
            ));
        }
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
