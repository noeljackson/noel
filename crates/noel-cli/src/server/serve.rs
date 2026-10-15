//! `noel server [--foreground]`: the user's host, `server` (spec §5).
//!
//! - It takes `server.lock` in the runtime directory (`File::try_lock`); a
//!   second server exits with 75. The directory must be this user's, mode
//!   0700.
//! - It binds `server.sock` (0600), after removing a stale one this user
//!   owns, and writes `server.json` (`pid`, `build`, `startedAt`).
//! - Each connection's peer uid is checked before a byte is read; another
//!   uid is closed. A connection is a WebSocket on `/chord`, accepted as a
//!   client acting as `{kind: "owner", via: "device"}`, or `via: "tunnel"`
//!   when its upgrade request says `Noel-Via: tunnel` (`noel bridge` sets
//!   it, and drops one the other machine sent).
//! - It serves `noel.sessions` (the directory, `create`, `fork`) and
//!   `noel.models` (the catalog), to the owner only.
//! - It routes `session:<id>`: `authorize` lets the owner reach any session,
//!   and `resolve` gives the session's worker, started if it has none. A
//!   worker is started with the journal's interrupted last run to resume,
//!   at most three times per run; one that is lost mid-run is started again
//!   at once (spec §8.3).
//! - When it starts, it adopts the workers a previous server left: each
//!   `workers/*.json` whose socket answers as this user, with the recorded
//!   pid and this build. A worker of another build is told to shut down once
//!   its run settles.
//! - With no client link and no worker for `server.idleExitMs` (default ten
//!   minutes) it exits.
//!
//! Without `--foreground`, `noel server` starts a detached server (its
//! output in `server.log`) and returns once it listens.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use clap::Parser;
use noel_chord::host::{AcceptOptions, Host, HostEvent, HostOptions, Route};
use noel_chord::link::{ndjson, websocket, Link, WebSocketOptions};
use noel_chord::object::BoxFuture;
use noel_chord::{Cell, ChordError, Implementation, Kind};
use noel_config::{load_config_with_project_trust, LoadedConfig, ENV_SESSION_DIR, THINKING_LEVELS};
use noel_core::{ReloadableSystems, RunStatus, Runtime, SessionState, SessionStore};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

use super::{
    build_id, control_line, derived_session_id, models_contract, own_exe, own_uid, owner,
    parse_control, peer_check, principal_key, remove_stale_socket, sessions_contract, set_mode,
    valid_session_id, worker_principal, write_record, RuntimeDir, DEFAULT_SERVER_IDLE_MS,
    ENV_RUNTIME_DIR, EXIT_LOCKED, VIA_HEADER,
};

#[derive(Debug, Parser)]
#[command(
    name = "noel server",
    about = "Serve noel sessions to clients on this machine"
)]
pub(crate) struct ServerArgs {
    /// Run in this process instead of starting a detached server.
    #[arg(long)]
    foreground: bool,
}

/// How many times the server resumes one run by itself.
const RESUME_LIMIT: u32 = 3;
/// How long a starting worker has to report `ready`.
const WORKER_START: Duration = Duration::from_secs(30);

pub(crate) async fn run(args: ServerArgs) -> i32 {
    if args.foreground {
        return match foreground().await {
            Ok(code) => code,
            Err(error) => {
                eprintln!("noel server: {error:#}");
                1
            }
        };
    }
    match start_detached().await {
        Ok(Started::Running(pid)) => {
            println!("noel server: running (pid {pid})");
            0
        }
        Ok(Started::AlreadyRunning(pid)) => {
            eprintln!("noel server: already running (pid {pid})");
            EXIT_LOCKED
        }
        Err(error) => {
            eprintln!("noel server: {error:#}");
            1
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ─── starting a detached server ─────────────────────────────────────────────

pub(crate) enum Started {
    Running(u32),
    AlreadyRunning(u32),
}

/// Starts `noel server --foreground` in its own session, its output in
/// `server.log`, and waits until it listens (or another server does).
pub(crate) async fn start_detached() -> Result<Started> {
    let dir = RuntimeDir::ensure(RuntimeDir::locate())?;
    let log_path = dir.server_log();
    let log = File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| anyhow!("open {}: {error}", log_path.display()))?;
    set_mode(&log_path, 0o600)?;
    let mut command = std::process::Command::new(own_exe());
    command
        .arg0("noel")
        .args(["server", "--foreground"])
        .env(ENV_RUNTIME_DIR, dir.root())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| anyhow!("start the server: {error}"))?;
    let pid = child.id();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.code() == Some(EXIT_LOCKED) {
                if let Some(other) = listening(&dir) {
                    return Ok(Started::AlreadyRunning(other));
                }
            } else {
                return Err(anyhow!(
                    "the server exited ({status}); see {}",
                    log_path.display()
                ));
            }
        } else if listening(&dir) == Some(pid) {
            // Reap it whenever it exits, should this process outlive it.
            std::thread::spawn(move || child.wait());
            return Ok(Started::Running(pid));
        }
        if tokio::time::Instant::now() > deadline {
            return Err(anyhow!(
                "the server did not start listening; see {}",
                log_path.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The pid of the server listening in `dir`, by its record and its socket.
fn listening(dir: &RuntimeDir) -> Option<u32> {
    let record: Value =
        serde_json::from_str(&fs::read_to_string(dir.server_record()).ok()?).ok()?;
    let pid = u32::try_from(record.get("pid")?.as_u64()?).ok()?;
    (dir.server_socket().exists() && alive(pid)).then_some(pid)
}

fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

// ─── the server ─────────────────────────────────────────────────────────────

async fn foreground() -> Result<i32> {
    let dir = RuntimeDir::ensure(RuntimeDir::locate())?;
    let lock_path = dir.server_lock();
    let lock_file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| anyhow!("open {}: {error}", lock_path.display()))?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            eprintln!(
                "noel server: another server holds {}; exiting",
                lock_path.display()
            );
            return Ok(EXIT_LOCKED);
        }
        Err(fs::TryLockError::Error(error)) => {
            return Err(anyhow!("lock {}: {error}", lock_path.display()))
        }
    }
    dir.ensure_workers()?;

    // Sessions and models as `noel` sees them from the home directory: the
    // global settings, never a project's.
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let paths = crate::config_paths(home.clone(), None)?;
    let mut config = load_config_with_project_trust(paths, false)?;
    if std::env::var_os(ENV_SESSION_DIR).is_none() {
        if let Some(session_dir) = config.settings.session_dir.clone() {
            config.paths = config.paths.with_session_dir(session_dir)?;
        }
    }
    let cli = <crate::Cli as Parser>::parse_from(["noel"]);
    crate::apply_cli_overrides(&cli, &home, &mut config)?;
    let offline = crate::offline_enabled(false);
    let idle_exit = Duration::from_millis(
        config
            .settings
            .server
            .as_ref()
            .and_then(|server| server.idle_exit_ms)
            .unwrap_or(DEFAULT_SERVER_IDLE_MS),
    );

    let socket = dir.server_socket();
    remove_stale_socket(&socket)?;
    let listener = UnixListener::bind(&socket)
        .map_err(|error| anyhow!("bind {}: {error}", socket.display()))?;
    set_mode(&socket, 0o600)?;
    let build = build_id();
    write_record(
        &dir.server_record(),
        &json!({
            "pid": std::process::id(),
            "build": build,
            "startedAt": super::now_ms(),
            "socket": socket.display().to_string(),
            "sessionDir": config.paths.session_dir.display().to_string(),
        }),
    )?;
    eprintln!(
        "noel server: listening on {} (pid {}, build {build}); sessions in {}",
        socket.display(),
        std::process::id(),
        config.paths.session_dir.display()
    );

    let clients: Arc<Mutex<HashSet<u64>>> = Arc::default();
    let tracked = clients.clone();
    // When a client or a worker last came or went.
    let activity = Arc::new(Mutex::new(tokio::time::Instant::now()));
    let touched = activity.clone();
    let host = Host::new(HostOptions {
        principal_key: Some(Arc::new(principal_key)),
        on_error: Some(Arc::new(|error| eprintln!("noel server: chord: {error}"))),
        observer: Some(Arc::new(move |event| {
            if matches!(
                event,
                HostEvent::LinkOpened { .. } | HostEvent::LinkClosed { .. }
            ) {
                *lock(&touched) = tokio::time::Instant::now();
            }
            log_event(event, &tracked)
        })),
        ..HostOptions::host("server")
    });
    let changed = Arc::new(Notify::new());
    let workers = Arc::new(Workers {
        dir: dir.clone(),
        session_dir: config.paths.session_dir.clone(),
        build,
        host: host.clone(),
        spawning: tokio::sync::Mutex::new(()),
        state: Mutex::default(),
        changed: changed.clone(),
        closing: AtomicBool::new(false),
    });
    let sessions = Arc::new(Sessions {
        config: config.clone(),
        offline,
        session_dir: config.paths.session_dir.clone(),
        directory: Cell::new(directory_schema(), json!({"sessions": []}))?,
        creating: Mutex::new(()),
        cache: Mutex::default(),
        changed: changed.clone(),
    });
    serve_sessions(&host, sessions.clone());
    serve_models(&host, &config, offline)?;
    host.route("session:", SessionRoute(workers.clone()));

    workers.adopt_all().await;
    tokio::spawn(refresh_directory(sessions, workers.clone()));
    tokio::spawn(accept_clients(listener, host.clone(), own_uid()));

    // Exits by a signal, or when nothing has used it for `idle_exit`.
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let code = loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = terminate.recv() => break 128 + libc::SIGTERM,
            _ = interrupt.recv() => break 128 + libc::SIGINT,
            _ = hangup.recv() => break 128 + libc::SIGHUP,
        }
        let idle = lock(&clients).is_empty() && workers.count() == 0;
        if idle && lock(&activity).elapsed() >= idle_exit {
            eprintln!("noel server: idle; exiting");
            break 0;
        }
    };
    // Workers are left running: a new server adopts them.
    workers.closing.store(true, Ordering::SeqCst);
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(dir.server_record());
    host.close().await;
    eprintln!("noel server: exited ({code})");
    drop(lock_file);
    Ok(code)
}

fn log_event(event: &HostEvent, clients: &Mutex<HashSet<u64>>) {
    match event {
        HostEvent::LinkOpened {
            link,
            kind,
            principal,
        } => {
            if *kind == Kind::Client {
                lock(clients).insert(*link);
            }
            let principal = principal.as_ref().map(Value::to_string).unwrap_or_default();
            eprintln!(
                "noel server: link {link} opened: {} {principal}",
                kind.as_str()
            );
        }
        HostEvent::LinkClosed { link, reason } => {
            lock(clients).remove(link);
            eprintln!("noel server: link {link} closed: {reason}");
        }
        HostEvent::Held {
            link,
            id,
            type_name,
            input,
            ..
        } => eprintln!("noel server: hold link {link} id {id}: {type_name} {input}"),
        HostEvent::Released {
            link,
            id,
            type_name,
            input,
            ..
        } => eprintln!("noel server: release link {link} id {id}: {type_name} {input}"),
    }
}

/// How the owner came in, by the upgrade request: `tunnel` when it says
/// `Noel-Via: tunnel`, `device` when it has no `Noel-Via`. It must ask for
/// `/chord` (tungstenite's error type).
#[allow(clippy::result_large_err)]
fn check_upgrade(request: &Request) -> Result<&'static str, ErrorResponse> {
    let refuse = |status: StatusCode, why: &str| {
        let mut refusal = ErrorResponse::new(Some(why.to_owned()));
        *refusal.status_mut() = status;
        refusal
    };
    if request.uri().path() != "/chord" {
        return Err(refuse(StatusCode::NOT_FOUND, "not found"));
    }
    let mut via = request.headers().get_all(VIA_HEADER).iter();
    match (via.next(), via.next()) {
        (None, _) => Ok("device"),
        (Some(value), None) if value.as_bytes() == b"tunnel" => Ok("tunnel"),
        _ => Err(refuse(StatusCode::BAD_REQUEST, "bad Noel-Via")),
    }
}

/// Accepts clients: a connection whose peer is not the uid `owner_uid` is
/// closed before a byte is read.
pub(crate) async fn accept_clients(listener: UnixListener, host: Host, owner_uid: u32) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                eprintln!("noel server: accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(credentials) if credentials.uid() == owner_uid => {}
            Ok(credentials) => {
                eprintln!(
                    "noel server: refused a connection from uid {} (pid {:?}): not the owner",
                    credentials.uid(),
                    credentials.pid()
                );
                drop(stream);
                continue;
            }
            Err(error) => {
                eprintln!("noel server: refused a connection: no peer credentials: {error}");
                drop(stream);
                continue;
            }
        }
        let host = host.clone();
        tokio::spawn(async move {
            let via = Arc::new(Mutex::new("device"));
            let seen = via.clone();
            // tungstenite's error type.
            #[allow(clippy::result_large_err)]
            let upgrade =
                move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
                    *lock(&seen) = check_upgrade(request)?;
                    Ok(response)
                };
            match tokio_tungstenite::accept_hdr_async(stream, upgrade).await {
                Ok(socket) => {
                    let link = websocket(
                        socket,
                        WebSocketOptions {
                            accept_packed: false,
                        },
                    );
                    let via = *lock(&via);
                    host.accept(link, AcceptOptions::client(owner(via)));
                }
                Err(error) => eprintln!("noel server: refused: websocket upgrade: {error}"),
            }
        });
    }
}

// ─── workers ────────────────────────────────────────────────────────────────

struct Workers {
    dir: RuntimeDir,
    session_dir: PathBuf,
    build: String,
    host: Host,
    /// One worker starts at a time.
    spawning: tokio::sync::Mutex<()>,
    state: Mutex<WorkersState>,
    /// Tells the directory to look again.
    changed: Arc<Notify>,
    /// The server is exiting: no worker starts.
    closing: AtomicBool,
}

#[derive(Default)]
struct WorkersState {
    live: HashMap<String, LiveWorker>,
    /// The last generation started, by session.
    generations: HashMap<String, u64>,
    /// How often the server resumed each run.
    resumes: HashMap<String, u32>,
}

struct LiveWorker {
    link: Link,
    generation: u64,
    /// Its last `status`.
    status: Value,
    /// It said it is exiting.
    leaving: bool,
}

/// What a worker reports for the directory.
#[derive(Clone)]
struct LiveStatus {
    busy: bool,
    viewers: u64,
    run_id: Option<String>,
}

impl Workers {
    fn count(&self) -> usize {
        lock(&self.state).live.len()
    }

    fn statuses(&self) -> HashMap<String, LiveStatus> {
        let state = lock(&self.state);
        state
            .live
            .iter()
            .map(|(id, worker)| {
                let status = &worker.status;
                (
                    id.clone(),
                    LiveStatus {
                        busy: status.get("running").and_then(Value::as_bool) == Some(true),
                        viewers: status.get("viewers").and_then(Value::as_u64).unwrap_or(0),
                        run_id: status
                            .get("runId")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    },
                )
            })
            .collect()
    }

    fn live_link(&self, id: &str) -> Option<Link> {
        let state = lock(&self.state);
        let worker = state.live.get(id)?;
        (!worker.link.is_closed()).then(|| worker.link.clone())
    }

    /// The session's worker, started if it has none. (Boxed: a worker's
    /// watch can start the next one.)
    fn ensure(self: &Arc<Self>, id: &str) -> BoxFuture<Result<Link, ChordError>> {
        let (workers, id) = (self.clone(), id.to_owned());
        Box::pin(async move {
            if let Some(link) = workers.live_link(&id) {
                return Ok(link);
            }
            let _spawning = workers.spawning.lock().await;
            if let Some(link) = workers.live_link(&id) {
                return Ok(link);
            }
            // A worker whose link dropped while it runs on is dialed again.
            let pid = recorded_pid(&workers.dir, &id);
            if let Some(pid) = pid.filter(|pid| alive(*pid)) {
                match workers.attach(&id, Some(pid), true).await {
                    Ok(link) => {
                        eprintln!("noel server: attached worker {id} again (pid {pid})");
                        return Ok(link);
                    }
                    Err(error) => eprintln!("noel server: {error}"),
                }
            }
            // A worker lost a moment ago may still hold the journal's lock.
            let mut attempt = 0;
            loop {
                match workers.spawn(&id).await {
                    Err(error) if error.reason.as_deref() == Some("locked") && attempt < 20 => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    result => return result,
                }
            }
        })
    }

    /// Starts a worker for the session, resuming its interrupted last run
    /// while that run's resumes last (spec §8.3), and attaches it.
    async fn spawn(self: &Arc<Self>, id: &str) -> Result<Link, ChordError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(ChordError::unavailable("the server is exiting"));
        }
        let journal = self.session_dir.join(format!("{id}.jsonl"));
        if !journal.is_file() {
            return Err(ChordError::invalid("session", format!("no session {id}")));
        }
        let resume = self.resume_for(id, journal).await;
        let generation = {
            let mut state = lock(&self.state);
            let recorded = recorded_generation(&self.dir, id);
            let generation = state.generations.entry(id.to_owned()).or_insert(recorded);
            *generation = (*generation).max(recorded) + 1;
            *generation
        };
        let log_path = self.dir.worker_log(id);
        let log = File::options()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| {
                ChordError::unavailable(format!("open {}: {error}", log_path.display()))
            })?;
        let _ = set_mode(&log_path, 0o600);
        let mut command = tokio::process::Command::new(own_exe());
        command
            .arg0("noel")
            .args(["worker", "--session", id, "--generation"])
            .arg(generation.to_string());
        if let Some(run_id) = &resume {
            command.args(["--resume", run_id]);
        }
        command
            .env(ENV_SESSION_DIR, &self.session_dir)
            .env(ENV_RUNTIME_DIR, self.dir.root())
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(log);
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(std::io::Error::from)
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| ChordError::unavailable(format!("start a worker: {error}")))?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped");
        let mut lines = BufReader::new(stdout).lines();
        let first = tokio::time::timeout(WORKER_START, lines.next_line()).await;
        drop(lines);
        let message = match first {
            Ok(Ok(Some(line))) => parse_control(&line),
            _ => None,
        };
        tokio::spawn(async move {
            if let Ok(status) = child.wait().await {
                eprintln!("noel server: worker pid {pid:?} exited: {status}");
            }
        });
        match message
            .as_ref()
            .and_then(|message| message.get("type")?.as_str())
        {
            Some("ready") => {}
            Some("fatal") => {
                let message = message.as_ref().expect("a message");
                let text = message.get("message").and_then(Value::as_str).unwrap_or("");
                let error = ChordError::unavailable(format!("session {id}: {text}"));
                return Err(match message.get("reason").and_then(Value::as_str) {
                    Some(reason) => error.with_reason(reason),
                    None => error,
                });
            }
            _ => {
                if let Some(pid) = pid {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                return Err(ChordError::unavailable(format!(
                    "the worker for {id} did not start; see {}",
                    log_path.display()
                )));
            }
        }
        if let Some(run_id) = &resume {
            *lock(&self.state).resumes.entry(run_id.clone()).or_insert(0) += 1;
        }
        eprintln!(
            "noel server: started worker {id} generation {generation} (pid {pid:?}){}",
            resume
                .as_ref()
                .map(|run| format!(", resuming run {run}"))
                .unwrap_or_default()
        );
        self.attach(id, pid, false).await
    }

    /// The journal's interrupted last run, if the server may resume it once
    /// more. The resume counts once a worker started with it is ready.
    async fn resume_for(&self, id: &str, journal: PathBuf) -> Option<String> {
        let state = tokio::task::spawn_blocking(move || SessionStore::read(&journal))
            .await
            .ok()?
            .ok()?;
        let run = state.runs.last()?;
        if run.status != RunStatus::Interrupted {
            return None;
        }
        let resumes = lock(&self.state).resumes.get(&run.id).copied().unwrap_or(0);
        if resumes >= RESUME_LIMIT {
            eprintln!(
                "noel server: run {} of {id} stays interrupted: resumed {RESUME_LIMIT} times already",
                run.id
            );
            return None;
        }
        Some(run.id.clone())
    }

    /// Dials the session's worker and makes it a child host. A worker
    /// being adopted must be of this build; one of another is told to shut
    /// down once its run settles.
    async fn attach(
        self: &Arc<Self>,
        id: &str,
        pid: Option<u32>,
        adopting: bool,
    ) -> Result<Link, ChordError> {
        let unavailable = |what: String| ChordError::unavailable(format!("worker {id}: {what}"));
        let socket = self.dir.worker_socket(id);
        let stream = UnixStream::connect(&socket)
            .await
            .map_err(|error| unavailable(format!("connect {}: {error}", socket.display())))?;
        let peer = peer_check(&stream).map_err(unavailable)?;
        if let (Some(expected), Some(peer)) = (pid, peer) {
            if expected != peer {
                return Err(unavailable(format!("pid {peer}, not {expected}")));
            }
        }
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await;
        if !matches!(read, Ok(Ok(1..))) {
            return Err(unavailable("no ready".into()));
        }
        let ready = parse_control(&line)
            .filter(|ready| ready.get("type").and_then(Value::as_str) == Some("ready"))
            .ok_or_else(|| unavailable("no ready".into()))?;
        let ready_pid = ready
            .get("pid")
            .and_then(Value::as_u64)
            .map(|pid| pid as u32);
        if ready.get("session").and_then(Value::as_str) != Some(id)
            || (peer.is_some() && ready_pid != peer)
        {
            return Err(unavailable(format!("unexpected ready {ready}")));
        }
        // The next worker this server starts for the session counts on.
        let generation = ready.get("generation").and_then(Value::as_u64).unwrap_or(0);
        {
            let mut state = lock(&self.state);
            let known = state.generations.entry(id.to_owned()).or_insert(0);
            *known = (*known).max(generation);
        }
        let build = ready.get("build").and_then(Value::as_str).unwrap_or("");
        if adopting && build != self.build {
            let _ = write
                .write_all(control_line(&json!({"type": "shutdown"})).as_bytes())
                .await;
            return Err(unavailable(format!(
                "build {build} is not this server's {}; told it to shut down",
                self.build
            )));
        }
        let (control, controls) = mpsc::unbounded_channel();
        let (link, _) = ndjson(reader, write, Some(control));
        {
            let mut state = lock(&self.state);
            state.live.insert(
                id.to_owned(),
                LiveWorker {
                    link: link.clone(),
                    generation,
                    status: json!({}),
                    leaving: false,
                },
            );
        }
        self.host.accept(
            link.clone(),
            AcceptOptions::host(format!("session:{id}"), worker_principal(id, generation)),
        );
        tokio::spawn(self.clone().watch(id.to_owned(), generation, controls));
        self.changed.notify_one();
        Ok(link)
    }

    /// Follows a worker's control lines until its link ends. A worker lost
    /// with its run interrupted is started again at once to resume it.
    async fn watch(
        self: Arc<Self>,
        id: String,
        generation: u64,
        mut controls: mpsc::UnboundedReceiver<Value>,
    ) {
        while let Some(message) = controls.recv().await {
            let mut state = lock(&self.state);
            let Some(worker) = state
                .live
                .get_mut(&id)
                .filter(|worker| worker.generation == generation)
            else {
                continue;
            };
            match message.get("type").and_then(Value::as_str) {
                Some("status") => worker.status = message,
                Some("shutdown") => worker.leaving = true,
                _ => {}
            }
            drop(state);
            self.changed.notify_one();
        }
        let leaving = {
            let mut state = lock(&self.state);
            match state.live.get(&id) {
                Some(worker) if worker.generation == generation => {
                    let leaving = worker.leaving;
                    state.live.remove(&id);
                    Some(leaving)
                }
                _ => None,
            }
        };
        self.changed.notify_one();
        let Some(leaving) = leaving else { return };
        if leaving {
            eprintln!("noel server: worker {id} generation {generation} exited");
            return;
        }
        eprintln!("noel server: lost worker {id} generation {generation}");
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        // Started again now only when it left a run to resume; otherwise
        // the next hold starts it.
        let journal = self.session_dir.join(format!("{id}.jsonl"));
        let interrupted = tokio::task::spawn_blocking(move || {
            SessionStore::read(&journal).ok().and_then(|state| {
                state
                    .runs
                    .last()
                    .map(|run| run.status == RunStatus::Interrupted)
            })
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
        if interrupted {
            if let Err(error) = self.ensure(&id).await {
                eprintln!("noel server: could not restart worker {id}: {error}");
            }
        }
    }

    /// Adopts the workers a previous server left (spec §5, §8.2).
    async fn adopt_all(self: &Arc<Self>) {
        let Ok(entries) = fs::read_dir(self.dir.workers()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let Some(record) = fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            else {
                continue;
            };
            let Some(id) = record
                .get("session")
                .and_then(Value::as_str)
                .filter(|id| valid_session_id(id))
                .map(str::to_owned)
            else {
                continue;
            };
            let pid = record
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok());
            if pid.is_some_and(|pid| !alive(pid)) {
                eprintln!("noel server: worker {id} is gone; removing its files");
                let _ = fs::remove_file(&path);
                let _ = remove_stale_socket(&self.dir.worker_socket(&id));
                continue;
            }
            match self.attach(&id, pid, true).await {
                Ok(_) => eprintln!("noel server: adopted worker {id} (pid {pid:?})"),
                Err(error) => eprintln!("noel server: did not adopt worker {id}: {error}"),
            }
        }
    }
}

fn worker_record(dir: &RuntimeDir, id: &str) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(dir.worker_record(id)).ok()?).ok()
}

/// The generation in `workers/<id>.json`, which a server that did not start
/// the worker reads to number the next one.
fn recorded_generation(dir: &RuntimeDir, id: &str) -> u64 {
    worker_record(dir, id)
        .and_then(|record| record.get("generation").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// The pid in `workers/<id>.json`.
fn recorded_pid(dir: &RuntimeDir, id: &str) -> Option<u32> {
    worker_record(dir, id)
        .and_then(|record| record.get("pid").and_then(Value::as_u64))
        .and_then(|pid| u32::try_from(pid).ok())
}

struct SessionRoute(Arc<Workers>);

impl Route for SessionRoute {
    /// The owner reaches any session, a guest only its own.
    fn authorize(&self, principal: &Value, host_name: &str) -> BoxFuture<bool> {
        let allowed = host_name
            .strip_prefix("session:")
            .filter(|id| valid_session_id(id))
            .is_some_and(|id| match principal.get("kind").and_then(Value::as_str) {
                Some("owner") => true,
                Some("guest") => principal.get("session").and_then(Value::as_str) == Some(id),
                _ => false,
            });
        Box::pin(async move { allowed })
    }

    fn resolve(&self, host_name: &str) -> BoxFuture<Result<Link, ChordError>> {
        let workers = self.0.clone();
        let id = host_name.trim_start_matches("session:").to_owned();
        Box::pin(async move { workers.ensure(&id).await })
    }

    fn principal(&self, host_name: &str) -> Value {
        let id = host_name.trim_start_matches("session:");
        let generation = lock(&self.0.state)
            .live
            .get(id)
            .map(|worker| worker.generation)
            .unwrap_or(0);
        worker_principal(id, generation)
    }
}

// ─── noel.sessions and noel.models ──────────────────────────────────────────

struct Sessions {
    config: LoadedConfig,
    offline: bool,
    session_dir: PathBuf,
    directory: Cell,
    /// One `create` or `fork` at a time, so a repeat finds the first.
    creating: Mutex<()>,
    /// Directory entries by journal, with the size and time they were read
    /// at.
    cache: Mutex<HashMap<PathBuf, (u64, SystemTime, Value)>>,
    changed: Arc<Notify>,
}

fn directory_schema() -> Value {
    match sessions_contract()
        .variants
        .get("owner")
        .and_then(|members| members.get("directory"))
    {
        Some(noel_chord::MemberDescription::State { schema }) => schema.clone(),
        _ => unreachable!("noel.sessions has a directory"),
    }
}

fn serve_sessions(host: &Host, sessions: Arc<Sessions>) {
    host.serve(sessions_contract(), move |_input, principal| {
        if principal.get("kind").and_then(Value::as_str) != Some("owner") {
            return Err(ChordError::forbidden("noel.sessions is the owner's"));
        }
        let (create, fork) = (sessions.clone(), sessions.clone());
        Ok(Implementation::new("owner")
            .state("directory", &sessions.directory)
            .method("create", move |args, _ctx| {
                let sessions = create.clone();
                async move { blocking(move || sessions.create(&args.unwrap_or_default())).await }
            })
            .method("fork", move |args, _ctx| {
                let sessions = fork.clone();
                async move { blocking(move || sessions.fork(&args.unwrap_or_default())).await }
            }))
    });
}

async fn blocking(
    work: impl FnOnce() -> Result<Value, ChordError> + Send + 'static,
) -> Result<Value, ChordError> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(ChordError::unavailable(error.to_string())))
}

impl Sessions {
    /// `create({requestId, cwd, name?, model?, thinkingLevel?,
    /// workspace?})`: a session named by its `requestId`, so a repeat returns
    /// it and writes nothing. `workspace` (`ssh://…`, as `--workspace`) runs
    /// its tools on another machine, whose host noel must trust already.
    fn create(&self, args: &Value) -> Result<Value, ChordError> {
        let text = |key: &str| args.get(key).and_then(Value::as_str);
        let request_id = text("requestId").unwrap_or_default();
        let cwd = PathBuf::from(text("cwd").unwrap_or_default());
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(ChordError::invalid(
                "cwd",
                format!("{} is not an absolute path to a directory", cwd.display()),
            ));
        }
        let workspace = text("workspace")
            .map(|url| {
                crate::workspace::from_flag(url, &self.config.paths.agent_dir)
                    .map_err(|error| ChordError::invalid("workspace", error.to_string()))
            })
            .transpose()?;
        let model = text("model")
            .map(|reference| {
                super::worker::model_reference(&self.config, reference, self.offline).ok_or_else(
                    || ChordError::invalid("model", format!("Model not found: {reference}")),
                )
            })
            .transpose()?;
        let thinking = text("thinkingLevel")
            .map(|level| {
                crate::normalized_thinking_level(level).ok_or_else(|| {
                    ChordError::invalid("thinkingLevel", format!("Unknown thinking level: {level}"))
                })
            })
            .transpose()?;
        let _creating = lock(&self.creating);
        let id = derived_session_id("create", request_id);
        if self.session_dir.join(format!("{id}.jsonl")).exists() {
            return Ok(json!({"sessionId": id, "created": false}));
        }
        let failed = |error: &dyn std::fmt::Display| ChordError::unavailable(error.to_string());
        let (store, state) = match workspace {
            Some(workspace) => {
                SessionStore::create_in_workspace(&self.session_dir, cwd, &id, workspace)
            }
            None => SessionStore::create_with_id(&self.session_dir, cwd, &id),
        }
        .map_err(|error| failed(&error))?;
        let mut runtime = Runtime::with_store(
            state,
            ReloadableSystems::from_config(&self.config, 1),
            store,
        );
        let selection = crate::model_select::CliSelection { model, thinking };
        crate::select_initial_model(&mut runtime, &self.config, &selection, self.offline)
            .map_err(|error| failed(&error))?;
        if let Some(name) = text("name").map(str::trim).filter(|name| !name.is_empty()) {
            runtime
                .rename_session(Some(name.to_owned()))
                .map_err(|error| failed(&error))?;
        }
        // The worker takes the session's lock; this process lets it go.
        drop(runtime);
        self.listed(&id);
        Ok(json!({"sessionId": id, "created": true}))
    }

    /// `fork({requestId, sessionId})`: a copy of the session as its journal
    /// holds it, named by the `requestId`.
    fn fork(&self, args: &Value) -> Result<Value, ChordError> {
        let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
        let source_id = text("sessionId");
        if !valid_session_id(source_id) {
            return Err(ChordError::invalid(
                "session",
                format!("no session {source_id}"),
            ));
        }
        let source_path = self.session_dir.join(format!("{source_id}.jsonl"));
        let source = SessionStore::read(&source_path)
            .map_err(|_| ChordError::invalid("session", format!("no session {source_id}")))?;
        let _creating = lock(&self.creating);
        let id = derived_session_id("fork", text("requestId"));
        if self.session_dir.join(format!("{id}.jsonl")).exists() {
            return Ok(json!({"sessionId": id, "created": false}));
        }
        SessionStore::fork_with_id(&self.session_dir, &source, false, &id)
            .map_err(|error| ChordError::unavailable(error.to_string()))?;
        self.listed(&id);
        Ok(json!({"sessionId": id, "created": true}))
    }

    /// Puts a session just made first in the directory before its maker
    /// hears it is made, so the maker's next listing has it.
    fn listed(&self, id: &str) {
        let path = self.session_dir.join(format!("{id}.jsonl"));
        if let Ok(state) = SessionStore::read(&path) {
            let modified = fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or_else(|_| SystemTime::now());
            let mut entry = directory_entry(&state, modified);
            entry["live"] = json!({"worker": false, "busy": false, "viewers": 0});
            let _ = self.directory.change(|directory| {
                let sessions = directory["sessions"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut next: Vec<Value> = sessions
                    .into_iter()
                    .filter(|session| session["sessionId"] != json!(id))
                    .collect();
                next.insert(0, entry);
                vec![noel_chord::delta::Op::Set(
                    vec![noel_chord::delta::Seg::Key("sessions".into())],
                    Value::Array(next),
                )]
            });
        }
        self.changed.notify_one();
    }

    /// The directory: every readable session, newest first, with what the
    /// live workers report.
    fn scan(&self, live: &HashMap<String, LiveStatus>) -> Value {
        let mut found = Vec::new();
        let entries = fs::read_dir(&self.session_dir)
            .into_iter()
            .flatten()
            .flatten();
        let mut cache = lock(&self.cache);
        let mut seen = HashSet::new();
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            seen.insert(path.clone());
            let cached = cache
                .get(&path)
                .filter(|(size, time, _)| *size == metadata.len() && *time == modified)
                .map(|(_, _, entry)| entry.clone());
            let entry = match cached {
                Some(entry) => entry,
                None => {
                    let Ok(state) = SessionStore::read(&path) else {
                        continue;
                    };
                    let entry = directory_entry(&state, modified);
                    cache.insert(path.clone(), (metadata.len(), modified, entry.clone()));
                    entry
                }
            };
            found.push(entry);
        }
        cache.retain(|path, _| seen.contains(path));
        drop(cache);
        for entry in &mut found {
            let id = entry["sessionId"].as_str().unwrap_or_default().to_owned();
            let status = live.get(&id);
            entry["live"] = json!({
                "worker": status.is_some(),
                "busy": status.is_some_and(|status| status.busy),
                "viewers": status.map_or(0, |status| status.viewers),
            });
            if let Some(run_id) = status.and_then(|status| status.run_id.clone()) {
                entry["lastRun"] = json!({"runId": run_id, "status": "running"});
            }
        }
        found.sort_by(|a, b| {
            let time = |entry: &Value| entry["modified"].as_u64().unwrap_or(0);
            time(b).cmp(&time(a))
        });
        json!({ "sessions": found })
    }
}

fn directory_entry(state: &SessionState, modified: SystemTime) -> Value {
    let mut entry = json!({
        "sessionId": state.session_id,
        "cwd": state.cwd.display().to_string(),
        "modified": modified.duration_since(UNIX_EPOCH).map(|time| time.as_millis() as u64).unwrap_or(0),
        "messageCount": state.messages.len(),
    });
    if let Some(name) = &state.name {
        entry["name"] = json!(name);
    }
    if let Some(workspace) = &state.workspace {
        entry["workspace"] = json!(crate::workspace::describe(workspace));
    }
    if let Some(prompt) = state
        .messages
        .iter()
        .find(|message| message.role == noel_core::MessageRole::User)
    {
        let prompt: String = prompt.content.chars().take(200).collect();
        entry["firstPrompt"] = json!(prompt);
    }
    if let Some(model) = &state.active_model {
        entry["model"] = json!(format!("{}/{}", model.provider, model.id));
    }
    if let Some(run) = state.runs.last() {
        let status = serde_json::to_value(run.status).unwrap_or_default();
        entry["lastRun"] = json!({"runId": run.id, "status": status});
    }
    entry
}

/// Keeps the directory current: when sessions or workers change, and every
/// few seconds for journals written meanwhile.
async fn refresh_directory(sessions: Arc<Sessions>, workers: Arc<Workers>) {
    loop {
        let live = workers.statuses();
        let scanner = sessions.clone();
        if let Ok(value) = tokio::task::spawn_blocking(move || scanner.scan(&live)).await {
            if let Err(error) = sessions.directory.update(value) {
                eprintln!("noel server: directory: {error}");
            }
        }
        tokio::select! {
            () = sessions.changed.notified() => {}
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        // Changes that come together are read together.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn serve_models(host: &Host, config: &LoadedConfig, offline: bool) -> Result<()> {
    let schema = match models_contract()
        .variants
        .get("owner")
        .and_then(|members| members.get("catalog"))
    {
        Some(noel_chord::MemberDescription::State { schema }) => schema.clone(),
        _ => unreachable!("noel.models has a catalog"),
    };
    let models = crate::model_select::selectable_models(config, offline)
        .into_iter()
        .map(|model| {
            let reasoning = model.reasoning();
            let levels = if reasoning {
                THINKING_LEVELS
                    .iter()
                    .copied()
                    .filter(|level| model.clamp_thinking_level(level) == *level)
                    .collect()
            } else {
                vec!["off"]
            };
            json!({
                "provider": model.provider,
                "modelId": model.id,
                "name": model.name.clone().unwrap_or_else(|| model.id.clone()),
                "reasoning": reasoning,
                "thinkingLevels": levels,
            })
        })
        .collect::<Vec<_>>();
    let catalog = Cell::new(schema, json!({ "models": models }))?;
    host.serve(models_contract(), move |_input, principal| {
        if principal.get("kind").and_then(Value::as_str) != Some("owner") {
            return Err(ChordError::forbidden("noel.models is the owner's"));
        }
        Ok(Implementation::new("owner").state("catalog", &catalog))
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn a_connection_from_another_uid_is_closed_before_a_byte_is_read() {
        let dir = std::env::temp_dir().join(format!("nsv-uid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir = RuntimeDir::ensure(dir).unwrap();
        let socket = dir.server_socket();
        let listener = UnixListener::bind(&socket).unwrap();
        let host = Host::new(HostOptions::host("server"));
        // This process is not the owner this server accepts.
        tokio::spawn(accept_clients(listener, host, own_uid().wrapping_add(1)));
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        // The upgrade request is never read: the server closes the
        // connection, so the client sees the end of the stream.
        let _ = stream
            .write_all(b"GET /chord HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await;
        let mut reply = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut reply))
            .await
            .expect("closed in time");
        assert!(read.is_err() || reply.is_empty(), "{reply:?}");
        fs::remove_dir_all(dir.root()).unwrap();
    }

    #[tokio::test]
    async fn a_link_is_the_device_s_unless_its_upgrade_says_tunnel() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let dir = std::env::temp_dir().join(format!("nsv-via-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir = RuntimeDir::ensure(dir).unwrap();
        let socket = dir.server_socket();
        let listener = UnixListener::bind(&socket).unwrap();
        let (opened, mut principals) = mpsc::unbounded_channel();
        let host = Host::new(HostOptions {
            observer: Some(Arc::new(move |event| {
                if let HostEvent::LinkOpened { principal, .. } = event {
                    let _ = opened.send(principal.clone());
                }
            })),
            ..HostOptions::host("server")
        });
        tokio::spawn(accept_clients(listener, host, own_uid()));
        let upgrade = |via: Option<&str>| {
            let socket = socket.clone();
            let via = via.map(str::to_owned);
            async move {
                let mut request = "ws://localhost/chord".into_client_request().unwrap();
                if let Some(via) = via {
                    request
                        .headers_mut()
                        .insert(VIA_HEADER, via.parse().unwrap());
                }
                let stream = UnixStream::connect(&socket).await.unwrap();
                tokio_tungstenite::client_async(request, stream).await
            }
        };
        async fn next(
            principals: &mut mpsc::UnboundedReceiver<Option<Value>>,
        ) -> Option<Option<Value>> {
            tokio::time::timeout(Duration::from_secs(5), principals.recv())
                .await
                .ok()
                .flatten()
        }
        let _device = upgrade(None).await.unwrap();
        assert_eq!(next(&mut principals).await, Some(Some(owner("device"))));
        let _tunnel = upgrade(Some("tunnel")).await.unwrap();
        assert_eq!(next(&mut principals).await, Some(Some(owner("tunnel"))));
        // Any other claim is refused, and opens no link.
        for claim in ["device", "relay", "Tunnel"] {
            let error = upgrade(Some(claim)).await.unwrap_err().to_string();
            assert!(error.contains("400"), "{claim}: {error}");
        }
        assert_eq!(next(&mut principals).await, None);
        fs::remove_dir_all(dir.root()).unwrap();
    }
}
