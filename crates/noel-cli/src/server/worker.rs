//! `noel worker --session <id> --generation <g> [--resume <runId>]`: the host
//! `session:<id>` (spec §5), which the server starts and dials.
//!
//! - It opens the session's journal under its lock (`fatal`, reason
//!   `locked`, exit 75 if another process holds it), binds
//!   `workers/<id>.sock` (0600), writes `workers/<id>.json`, and prints one
//!   control line, `{"chord2control": {"type": "ready", ...}}`, on stdout.
//! - Each connection to its socket (from this user only) is its parent: it
//!   sends `ready` again (`pid`, `build`, `session`, `generation`), then
//!   speaks Chord as NDJSON lines `{"chord2": frame}`, with control lines for
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
use noel_chord::host::{Host, HostEvent, HostOptions, LinkOptions};
use noel_chord::link::{ndjson, ControlSender};
use noel_chord::object::BoxFuture;
use noel_chord::{CallContext, Cell, ChordError, Implementation};
use noel_config::{load_config_with_resources, project_is_trusted, LoadedConfig};
use noel_core::controller::{
    AbortPolicy, Busy, Enqueued, Event, Host as ControllerHost, Image, Repeat, SessionController,
    Start, Submission, SubmissionState, SubmitError, Submitted, WhenBusy, Withdrawal,
};
use noel_core::{
    expand_input, ReloadableSystems, RunStatus, Runtime, SessionError, SessionStore, SubmissionKind,
};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Notify};

use super::view::{self, View};
use super::{
    build_id, chat_contract, chat_variant, control_line, peer_check, principal_key,
    remove_stale_socket, set_mode, valid_session_id, write_record, RuntimeDir,
    DEFAULT_WORKER_IDLE_MS, EXIT_LOCKED,
};

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
    // builds it with no flags.
    let cwd = state.cwd.clone();
    if let Err(error) = std::env::set_current_dir(&cwd) {
        eprintln!(
            "noel worker: the session's directory {}: {error}",
            cwd.display()
        );
    }
    let paths = crate::config_paths(cwd.clone(), Some(session_dir))?;
    let trusted = project_is_trusted(&paths.agent_dir, &cwd).unwrap_or(false);
    let cli = crate::Cli::parse_from(["noel"]);
    let resources = crate::resource_commands::resource_options(&cli);
    let mut config = load_config_with_resources(paths, trusted, &resources)?;
    crate::apply_cli_overrides(&cli, &cwd, &mut config)?;
    let offline = crate::offline_enabled(false);
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
    let view = Arc::new(Mutex::new(View::new(&state)));
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
    serve_chat(&host, &session_id, lock(&view).cell().clone(), commands);

    let worker = WorkerHost {
        config,
        offline,
        view,
        status: status.clone(),
        aborts: Vec::new(),
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
    Host::new(HostOptions {
        principal_key: Some(Arc::new(principal_key)),
        on_error: Some(Arc::new(|error| eprintln!("noel worker: chord: {error}"))),
        observer: Some(Arc::new(move |event| match event {
            HostEvent::Held { type_name, .. } if type_name == "noel.chat" => {
                status.update(|status| status.holds += 1)
            }
            HostEvent::Released { type_name, .. } if type_name == "noel.chat" => {
                status.update(|status| status.holds = status.holds.saturating_sub(1))
            }
            HostEvent::LinkClosed { reason, .. } => {
                eprintln!("noel worker: link closed: {reason}")
            }
            _ => {}
        })),
        ..HostOptions::host(format!("session:{session_id}"))
    })
}

type Reply = oneshot::Sender<Result<Value, ChordError>>;

/// A `noel.chat` method call, for the task that drives the session.
enum Command {
    Submit(Value, Reply),
    Withdraw(Value, Reply),
    Abort(Value, Reply),
    Compact(Value, Reply),
    Configure(Value, Reply),
    Resume(Value, Reply),
}

fn serve_chat(host: &Host, session_id: &str, cell: Cell, commands: mpsc::UnboundedSender<Command>) {
    let session_id = session_id.to_owned();
    host.serve(chat_contract(), move |input, principal| {
        if input.get("session").and_then(Value::as_str) != Some(session_id.as_str()) {
            return Err(ChordError::invalid(
                "input",
                format!("this host serves session {session_id}"),
            ));
        }
        match chat_variant(principal, &session_id) {
            Some("participant") => Ok(participant(&cell, &commands)),
            Some(_) => Ok(Implementation::new("viewer").state("view", &cell)),
            None => Err(ChordError::forbidden(format!(
                "session {session_id} is not for this principal"
            ))),
        }
    });
}

fn participant(cell: &Cell, commands: &mpsc::UnboundedSender<Command>) -> Implementation {
    fn method(
        commands: &mpsc::UnboundedSender<Command>,
        make: fn(Value, Reply) -> Command,
    ) -> impl Fn(Option<Value>, CallContext) -> BoxFuture<Result<Value, ChordError>>
           + Send
           + Sync
           + 'static {
        let commands = commands.clone();
        move |args, _ctx| {
            let commands = commands.clone();
            Box::pin(async move {
                let (reply, answer) = oneshot::channel();
                let closing = || ChordError::unavailable("the session is closing");
                commands
                    .send(make(args.unwrap_or_else(|| json!({})), reply))
                    .map_err(|_| closing())?;
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
        let (link, sender) = ndjson(read, write, Some(control));
        sender.send(&status.get().control());
        *lock(&parent) = Some(sender);
        host.connect(link, LinkOptions::default());
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
    while let Some(command) = inbox.recv().await {
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
    refresh(controller);
    finished
}

/// A command while no run is active; a run it starts is returned.
async fn idle_command(
    controller: &mut SessionController<WorkerHost>,
    command: Command,
) -> Option<Start> {
    match command {
        Command::Submit(args, reply) => {
            let (submission, _) = match submission(&args) {
                Ok(parsed) => parsed,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return None;
                }
            };
            match controller.submit(submission).await {
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
            let _ = reply.send(result.map_err(|error| ChordError::invalid("abort", error)));
        }
        Command::Compact(args, reply) => {
            let focus = args.get("instructions").and_then(Value::as_str);
            let result = controller.compact(focus).await.map(|record| {
                json!({
                    "omittedMessages": record.omitted_messages,
                    "retainedMessages": record.retained_messages,
                })
            });
            let _ = reply.send(result.map_err(|error| ChordError::invalid("compact", error)));
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
                    let _ = reply.send(Err(ChordError::invalid("resume", error)));
                }
            }
        }
    }
    None
}

/// A command while a run is active.
fn busy_command(command: Command, busy: &mut Busy<'_, WorkerHost>) {
    match command {
        Command::Submit(args, reply) => {
            let result = submission(&args).and_then(|(submission, when_busy)| {
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
                    let _ = reply.send(Err(ChordError::invalid("abort", error)));
                }
            }
        }
        Command::Compact(_, reply) | Command::Configure(_, reply) | Command::Resume(_, reply) => {
            let _ = reply.send(Err(ChordError::invalid(
                "busy",
                "a run is active; wait for it to settle",
            )));
        }
    }
}

fn text(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `submit`'s arguments as a submission: `whenBusy` names its kind, as
/// `prompt`, `steer` and `follow_up` do over RPC.
fn submission(args: &Value) -> Result<(Submission, WhenBusy), ChordError> {
    let (kind, when_busy) = match args.get("whenBusy").and_then(Value::as_str) {
        None | Some("reject") => (SubmissionKind::Prompt, WhenBusy::Reject),
        Some("steer") => (SubmissionKind::Steer, WhenBusy::Steer),
        Some("followUp") => (SubmissionKind::FollowUp, WhenBusy::FollowUp),
        Some(other) => {
            return Err(ChordError::invalid(
                "args",
                format!("whenBusy {other:?} is not reject, steer or followUp"),
            ))
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

fn withdrawn(result: Result<Withdrawal, String>) -> Result<Value, ChordError> {
    let outcome = match result.map_err(|error| ChordError::invalid("withdraw", error))? {
        Withdrawal::Withdrawn => "withdrawn",
        Withdrawal::AlreadyPlaced => "already_placed",
        Withdrawal::Settled => "settled",
    };
    Ok(json!({ "outcome": outcome }))
}

/// A refused submission. Nothing was written, so none of these is
/// `unavailable`: retrying the same submission cannot succeed.
fn refusal(error: SubmitError) -> ChordError {
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
    ChordError::invalid(reason, message)
}

/// `configure`: the session's model, then its thinking level, as RPC's
/// `set_model` and `set_thinking_level` change them.
fn configure(
    runtime: &mut Runtime,
    host: &mut WorkerHost,
    args: &Value,
) -> Result<Value, ChordError> {
    let invalid = |error: anyhow::Error| ChordError::invalid("configure", error.to_string());
    let level = args.get("thinkingLevel").and_then(Value::as_str);
    if let Some(level) = level {
        if crate::normalized_thinking_level(level).is_none() {
            return Err(ChordError::invalid(
                "configure",
                format!("Unknown thinking level: {level}"),
            ));
        }
    }
    if let Some(reference) = args.get("model").and_then(Value::as_str) {
        let model = model_reference(&host.config, reference, host.offline).ok_or_else(|| {
            ChordError::invalid("configure", format!("Model not found: {reference}"))
        })?;
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
            submission(&args).map(|(submission, when_busy)| (submission.kind, when_busy))
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
    fn refusals_are_never_retryable() {
        for error in [
            SubmitError::Busy,
            SubmitError::QueuedPrompt,
            SubmitError::Refused("no credential".into()),
        ] {
            let error = refusal(error);
            assert_eq!(error.code, noel_chord::ErrorCode::Invalid);
        }
        assert_eq!(refusal(SubmitError::Busy).reason.as_deref(), Some("busy"));
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
