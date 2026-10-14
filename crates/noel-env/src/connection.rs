//! One connection to a pi-env daemon (upstream `packages/env/src/connection.ts`): it starts a daemon command (a
//! local process, `ssh … noel-env serve`, or any other program that ends in a daemon's stdin and stdout), syncs on
//! the daemon's `PI-ENV <token>` line, and multiplexes requests over pi-env protocol 1's frames
//! (`packages/env/docs/protocol.md`).
//!
//! The daemon starts lazily, on the first request, and again on the request after it was lost: requests in flight
//! when it is lost fail with code `unknown` and `lost: true`, and handles belong to the daemon that opened them.
//!
//! Ported from upstream pi's `packages/env/src/connection.ts` (<https://github.com/earendil-works/pi>, commit
//! `1692bba`), Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::Instant;

use crate::frame::{
    encode_frame, CANCEL, ERROR, EVENT, MAX_FRAME, PING, PROTOCOL, REQUEST, RESULT,
};
use crate::Context;

const PING_INTERVAL: Duration = Duration::from_secs(5);
/// The daemon pings every five seconds; this much silence means the connection is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// How long starting the daemon and its `hello` may take.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Noise before the sync line kept while looking for it.
const MAX_NOISE: usize = 1024 * 1024;

/// A frame's JSON object.
pub type Json = Map<String, Value>;

/// A failure reported by the daemon, with its Node-style code, or a failure of the connection itself (`lost`).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteError {
    pub code: String,
    pub message: String,
    pub path: Option<String>,
    /// Every field of the error, such as `syscall`, `spillPath` or `lost`.
    pub fields: Json,
}

impl RemoteError {
    /// An error from its fields, as the daemon sends them.
    pub fn new(fields: Json) -> Self {
        Self {
            code: fields
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            message: fields
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Remote operation failed")
                .to_string(),
            path: fields
                .get("path")
                .and_then(Value::as_str)
                .map(str::to_string),
            fields,
        }
    }

    pub(crate) fn with(code: &str, message: &str, lost: bool) -> Self {
        let mut fields = Json::new();
        fields.insert("code".into(), Value::from(code));
        fields.insert("message".into(), Value::from(message));
        if lost {
            fields.insert("lost".into(), Value::Bool(true));
        }
        Self::new(fields)
    }

    /// Whether the connection was lost rather than the daemon refusing the request (upstream
    /// `isConnectionLost`).
    pub fn is_connection_lost(&self) -> bool {
        self.fields.get("lost") == Some(&Value::Bool(true))
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteError {}

/// The connection failed or ended; a request may succeed once it is started again.
pub(crate) fn lost(message: &str) -> RemoteError {
    RemoteError::with("unknown", message, true)
}

fn closed() -> RemoteError {
    RemoteError::with("unknown", "Connection closed", false)
}

/// What `hello` reports about the remote machine.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteInfo {
    pub protocol: u64,
    pub version: String,
    /// Rust's name of the system (`linux`, `macos`, …); [`node_platform`] gives Node's.
    pub os: String,
    pub arch: String,
    pub home: String,
    pub tmpdir: String,
    /// Path separator: `/`, or `\` on Windows.
    pub separator: String,
    /// The daemon's working directory.
    pub cwd: String,
    /// Windows: per-drive working directories.
    pub drive_cwds: BTreeMap<String, String>,
    pub pid: u64,
    /// Operations added to protocol 1 that this daemon supports, such as `listFiles`; absent from older daemons.
    pub features: Option<Vec<String>>,
}

/// Node's names for a system and architecture `hello` reports in Rust's spelling: `windows` is `win32`, `macos` is
/// `darwin`, `x86_64` is `x64`, `aarch64` is `arm64`, `x86` is `ia32`; other names are kept.
pub fn node_platform(os: &str, arch: &str) -> (String, String) {
    let platform = match os {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match arch {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    (platform.to_string(), arch.to_string())
}

/// A future computing the daemon command.
pub type CommandFuture = Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send>>;

/// What starts the daemon, before its `serve --token <hex>` arguments.
#[derive(Clone)]
pub enum DaemonCommand {
    /// A fixed argv, e.g. `["noel-env"]` or `["ssh", "-T", "--", "host", "noel-env"]`.
    Fixed(Vec<String>),
    /// Computed at each start (detecting and deploying first, for example). Its failure fails that start with
    /// `spawn_error` and its message, and the next request tries again.
    Computed(Arc<dyn Fn() -> CommandFuture + Send + Sync>),
}

impl fmt::Debug for DaemonCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed(argv) => formatter.debug_tuple("Fixed").field(argv).finish(),
            Self::Computed(_) => formatter.write_str("Computed"),
        }
    }
}

/// Receives the daemon's and the transport's diagnostic output.
pub type LogCallback = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct ConnectionOptions {
    pub command: DaemonCommand,
    pub on_log: Option<LogCallback>,
}

impl ConnectionOptions {
    pub fn new(command: DaemonCommand) -> Self {
        Self {
            command,
            on_log: None,
        }
    }
}

/// A request's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub json: Json,
    pub payload: Vec<u8>,
    /// The daemon session that answered; handles from it are only valid while it lives.
    pub session: u32,
}

/// Receives a request's progress events (`exec` output, `watch` changes): JSON and payload. Returning false cancels
/// the request, as aborting it does.
pub type EventCallback<'a> = &'a mut (dyn FnMut(&Json, &[u8]) -> bool + Send);

/// Receives a request's id and daemon session once it is sent.
pub type StartCallback<'a> = &'a mut (dyn FnMut(u32, u32) + Send);

#[derive(Default)]
pub struct RequestOptions<'a> {
    pub payload: &'a [u8],
    /// Aborting it sends `cancel`; the request still settles with the daemon's result or error.
    pub cx: Option<&'a Context>,
    pub on_event: Option<EventCallback<'a>>,
    /// The request's id and session, for [`Connection::kill`].
    pub on_start: Option<StartCallback<'a>>,
    /// Only run in this daemon session, for requests on handles it opened. After the connection was lost and
    /// started again, such requests fail instead of reaching a daemon that never opened the handle.
    pub session: Option<u32>,
    /// A noel addition: dropping the unsettled request sends `cancel`, so dropping an `exec` stops the command as
    /// aborting it does.
    pub cancel_on_drop: bool,
}

enum Incoming {
    Event(Json, Vec<u8>),
    Done(Result<Reply, RemoteError>),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a session is live, and whether its `hello` succeeded; one lock, so a session is never marked ready after
/// it was torn down.
#[derive(Default)]
struct Liveness {
    torn_down: bool,
    ready: bool,
}

/// One running daemon.
struct Session {
    id: u32,
    token: String,
    liveness: Mutex<Liveness>,
    /// Encoded frames for the daemon's stdin; `None` once torn down, which ends its stdin.
    writer: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    pending: Mutex<HashMap<u32, mpsc::UnboundedSender<Incoming>>>,
    last_seen: Mutex<Instant>,
    /// Asks the task that owns the process to signal it.
    kill: Notify,
}

impl Session {
    fn is_live(&self) -> bool {
        !lock(&self.liveness).torn_down
    }

    fn write(&self, kind: u8, id: u32, json: Json, payload: &[u8]) {
        if let Ok(bytes) = encode_frame(kind, id, &Value::Object(json), payload) {
            self.write_bytes(bytes);
        }
    }

    fn write_bytes(&self, bytes: Vec<u8>) {
        if !self.is_live() {
            return;
        }
        if let Some(writer) = lock(&self.writer).as_ref() {
            let _ = writer.send(bytes);
        }
    }

    fn cancel(&self, id: u32, kill: bool) {
        let mut json = Json::new();
        if kill {
            json.insert("mode".into(), Value::from("kill"));
        }
        self.write(CANCEL, id, json, &[]);
    }

    /// Ends the session and fails its requests with `error`; false if it had ended already.
    fn end(&self, error: &RemoteError) -> bool {
        {
            let mut liveness = lock(&self.liveness);
            if liveness.torn_down {
                return false;
            }
            liveness.torn_down = true;
        }
        // Ending stdin stops the daemon; the signal stops a transport that does not pass the end on.
        lock(&self.writer).take();
        self.kill.notify_one();
        let pending = std::mem::take(&mut *lock(&self.pending));
        for (_, request) in pending {
            let _ = request.send(Incoming::Done(Err(error.clone())));
        }
        true
    }
}

/// A request that was sent and has not settled. Dropping it stops waiting (and sends `cancel` with
/// `cancel_on_drop`); a reply that comes later is ignored.
pub struct PendingRequest {
    id: u32,
    session: Arc<Session>,
    receiver: mpsc::UnboundedReceiver<Incoming>,
    settled: bool,
    cancelled: bool,
    cancel_on_drop: bool,
}

impl PendingRequest {
    /// The request's id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Waits for the result, delivering events in order; aborting `cx`, or an event callback returning false,
    /// sends one `cancel`.
    pub async fn wait(
        mut self,
        cx: Option<&Context>,
        mut on_event: Option<EventCallback<'_>>,
    ) -> Result<Reply, RemoteError> {
        let background = Context::background();
        let cx = cx.unwrap_or(&background);
        loop {
            tokio::select! {
                biased;
                incoming = self.receiver.recv() => match incoming {
                    Some(Incoming::Event(json, payload)) => {
                        let keep = on_event.as_mut().is_none_or(|on_event| on_event(&json, &payload));
                        if !keep {
                            self.cancel();
                        }
                    }
                    Some(Incoming::Done(result)) => {
                        self.settled = true;
                        return result;
                    }
                    None => {
                        self.settled = true;
                        return Err(lost("pi-env connection lost"));
                    }
                },
                () = cx.aborted(), if !self.cancelled => self.cancel(),
            }
        }
    }

    fn cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            self.session.cancel(self.id, false);
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        lock(&self.session.pending).remove(&self.id);
        if self.cancel_on_drop && !self.cancelled {
            self.session.cancel(self.id, false);
        }
    }
}

#[derive(Clone)]
struct Started {
    info: Arc<RemoteInfo>,
    session: Arc<Session>,
}

type StartResult = Option<Result<Started, RemoteError>>;

#[derive(Default)]
struct State {
    session: Option<Arc<Session>>,
    /// The start in progress or done, shared by everyone waiting for it.
    ready: Option<watch::Receiver<StartResult>>,
    sessions: u32,
    next_id: u32,
    closed: bool,
}

struct Inner {
    options: ConnectionOptions,
    state: Mutex<State>,
}

impl Inner {
    /// Ends `session` and forgets it, so the next request starts a new daemon.
    fn teardown(&self, session: &Arc<Session>, error: RemoteError) {
        if !session.end(&error) {
            return;
        }
        let mut state = lock(&self.state);
        if state
            .session
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            state.session = None;
            // A start in progress publishes its own failure; only a finished one is forgotten here.
            if lock(&session.liveness).ready {
                state.ready = None;
            }
        }
    }

    fn log(&self, text: &str) {
        if let Some(on_log) = &self.options.on_log {
            on_log(text);
        }
    }
}

impl Drop for Inner {
    /// The last handle is gone: stop the daemon, as `close` does.
    fn drop(&mut self) {
        if let Some(session) = lock(&self.state).session.take() {
            session.end(&closed());
        }
    }
}

/// One connection to a pi-env daemon: started lazily on the first request and started again after it is lost.
/// Requests in flight when it is lost fail with code `unknown`; for mutations their outcome is then unknown.
/// Clones share the connection; dropping the last one stops the daemon.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("command", &self.inner.options.command)
            .finish_non_exhaustive()
    }
}

impl Connection {
    pub fn new(options: ConnectionOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                state: Mutex::new(State {
                    next_id: 1,
                    ..State::default()
                }),
            }),
        }
    }

    /// A connection that starts `argv`, then `serve --token <hex>`.
    pub fn command(argv: Vec<String>) -> Self {
        Self::new(ConnectionOptions::new(DaemonCommand::Fixed(argv)))
    }

    /// Connects if needed and returns what the daemon reported about its machine.
    pub async fn info(&self) -> Result<Arc<RemoteInfo>, RemoteError> {
        Ok(self.connect().await?.info)
    }

    /// Connects if needed and returns the live session's id.
    pub async fn session(&self) -> Result<u32, RemoteError> {
        Ok(self.connect().await?.session.id)
    }

    /// Sends a request and waits for its result.
    pub async fn request(
        &self,
        op: &str,
        json: Json,
        options: RequestOptions<'_>,
    ) -> Result<Reply, RemoteError> {
        let RequestOptions {
            payload,
            cx,
            on_event,
            on_start,
            session,
            cancel_on_drop,
        } = options;
        let session = match session {
            Some(session) => self
                .current(session)
                .ok_or_else(|| lost("pi-env connection lost"))?,
            None => self.connect().await?.session,
        };
        let mut pending = self.send(&session, op, json, payload, cx, on_start)?;
        pending.cancel_on_drop = cancel_on_drop;
        pending.wait(cx, on_event).await
    }

    /// Sends a request in the daemon session `session` without waiting for it, so several can be in flight at once;
    /// fails if that session is no longer the live one.
    pub fn start_request(
        &self,
        session: u32,
        op: &str,
        json: Json,
        payload: &[u8],
    ) -> Result<PendingRequest, RemoteError> {
        let current = self
            .current(session)
            .ok_or_else(|| lost("pi-env connection lost"))?;
        self.send(&current, op, json, payload, None, None)
    }

    /// Stops the daemon; it kills everything it started. Later requests fail.
    pub fn close(&self) {
        let session = {
            let mut state = lock(&self.inner.state);
            state.closed = true;
            state.session.clone()
        };
        if let Some(session) = session {
            self.inner.teardown(&session, closed());
        }
    }

    /// Kills one `exec` without aborting it, so it settles with the killed process's status.
    pub fn kill(&self, id: u32, session: u32) {
        if let Some(current) = self.current(session) {
            current.cancel(id, true);
        }
    }

    fn current(&self, session: u32) -> Option<Arc<Session>> {
        lock(&self.inner.state)
            .session
            .clone()
            .filter(|current| current.id == session && current.is_live())
    }

    fn send(
        &self,
        session: &Arc<Session>,
        op: &str,
        mut json: Json,
        payload: &[u8],
        cx: Option<&Context>,
        on_start: Option<StartCallback<'_>>,
    ) -> Result<PendingRequest, RemoteError> {
        if !session.is_live() {
            return Err(lost("pi-env connection lost"));
        }
        let id = {
            let mut state = lock(&self.inner.state);
            let id = state.next_id;
            state.next_id = state.next_id.checked_add(1).unwrap_or(1);
            id
        };
        // Upstream sends `{ ...json, op }`. A frame above the limit would end the connection at the daemon;
        // callers split transfers, so only an oversized request (a huge argv, say) fails here, alone.
        json.insert("op".into(), Value::from(op));
        let frame = encode_frame(REQUEST, id, &Value::Object(json), payload).map_err(|error| {
            RemoteError::with(
                "unknown",
                &format!("Cannot send {op} to pi-env: {error}"),
                false,
            )
        })?;
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&session.pending).insert(id, sender);
        if let Some(on_start) = on_start {
            on_start(id, session.id);
        }
        session.write_bytes(frame);
        let mut pending = PendingRequest {
            id,
            session: session.clone(),
            receiver,
            settled: false,
            cancelled: false,
            cancel_on_drop: false,
        };
        if cx.is_some_and(Context::is_aborted) {
            pending.cancel();
        }
        Ok(pending)
    }

    async fn connect(&self) -> Result<Started, RemoteError> {
        let mut ready = {
            let mut state = lock(&self.inner.state);
            if state.closed {
                return Err(closed());
            }
            match &state.ready {
                Some(ready) => ready.clone(),
                None => {
                    let (sender, receiver) = watch::channel(None);
                    state.ready = Some(receiver.clone());
                    let connection = self.clone();
                    // The start runs on its own, as upstream's promise does, so a caller that stops waiting does
                    // not stop it for the others.
                    tokio::spawn(async move { connection.start(sender).await });
                    receiver
                }
            }
        };
        let result = match ready.wait_for(Option::is_some).await {
            Ok(result) => result.clone(),
            Err(_) => None,
        };
        result.unwrap_or_else(|| Err(lost("pi-env connection lost")))
    }

    /// Runs one start and publishes its outcome. A failed start is not remembered: the next request tries again.
    async fn start(&self, sender: watch::Sender<StartResult>) {
        let result = self.start_session().await;
        if result.is_err() {
            let mut state = lock(&self.inner.state);
            let ours = sender.subscribe();
            if state
                .ready
                .as_ref()
                .is_some_and(|ready| ready.same_channel(&ours))
            {
                state.ready = None;
            }
        }
        sender.send_replace(Some(result));
    }

    async fn start_session(&self) -> Result<Started, RemoteError> {
        let command = match &self.inner.options.command {
            DaemonCommand::Fixed(argv) => argv.clone(),
            DaemonCommand::Computed(compute) => compute()
                .await
                .map_err(|message| RemoteError::with("spawn_error", &message, true))?,
        };
        if lock(&self.inner.state).closed {
            return Err(closed());
        }
        let Some((program, args)) = command.split_first() else {
            return Err(RemoteError::with("spawn_error", "No daemon command", false));
        };
        let token = random_token();
        let mut child = Command::new(program)
            .args(args)
            .args(["serve", "--token", &token])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| RemoteError::with("spawn_error", &error.to_string(), true))?;
        let (writer, frames) = mpsc::unbounded_channel();
        let session = Arc::new(Session {
            id: {
                let mut state = lock(&self.inner.state);
                state.sessions += 1;
                state.sessions
            },
            token,
            liveness: Mutex::default(),
            writer: Mutex::new(Some(writer)),
            pending: Mutex::default(),
            last_seen: Mutex::new(Instant::now()),
            kill: Notify::new(),
        });
        lock(&self.inner.state).session = Some(session.clone());
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(write_frames(child.stdin.take(), frames));
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(read_frames(weak.clone(), session.clone(), stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(read_log(weak.clone(), stderr));
        }
        let (exited_sender, mut exited) = watch::channel(None::<RemoteError>);
        tokio::spawn(own_process(
            weak.clone(),
            session.clone(),
            child,
            exited_sender,
        ));
        tokio::spawn(ping(weak, session.clone()));

        let mut hello = Json::new();
        hello.insert("protocol".into(), Value::from(PROTOCOL));
        let outcome = match self.send(&session, "hello", hello, &[], None, None) {
            Err(error) => Err(error),
            Ok(pending) => {
                tokio::select! {
                    biased;
                    exited = exited.wait_for(Option::is_some) => Err(exited
                        .ok()
                        .and_then(|error| error.clone())
                        .unwrap_or_else(|| lost("pi-env connection lost"))),
                    reply = pending.wait(None, None) => reply,
                    () = tokio::time::sleep(START_TIMEOUT) => Err(lost(&format!(
                        "pi-env did not answer within {} s",
                        START_TIMEOUT.as_secs()
                    ))),
                }
            }
        };
        let outcome = outcome.and_then(|reply| {
            let protocol = reply.json.get("protocol").cloned().unwrap_or(Value::Null);
            if protocol != PROTOCOL {
                return Err(lost(&format!("Unsupported protocol {protocol}")));
            }
            serde_json::from_value::<RemoteInfo>(Value::Object(reply.json))
                .map_err(|error| lost(&format!("Malformed hello from pi-env: {error}")))
        });
        let outcome = outcome.and_then(|info| {
            let mut liveness = lock(&session.liveness);
            if liveness.torn_down {
                return Err(lost("pi-env connection lost"));
            }
            liveness.ready = true;
            Ok(Started {
                info: Arc::new(info),
                session: session.clone(),
            })
        });
        if let Err(error) = &outcome {
            self.inner.teardown(&session, error.clone());
        }
        outcome
    }
}

/// Owns the daemon's process: signals it when asked, and tears the session down when it exits. One task, so the
/// process is signalled only before it is reaped, while its pid is still its own.
async fn own_process(
    inner: Weak<Inner>,
    session: Arc<Session>,
    mut child: Child,
    exited: watch::Sender<Option<RemoteError>>,
) {
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            () = session.kill.notified() => {
                if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
                    // SAFETY: kill(2) on our own unreaped child; upstream's `child.kill()` sends SIGTERM too.
                    unsafe { libc::kill(pid, libc::SIGTERM) };
                }
            }
        }
    };
    let code = status
        .ok()
        .and_then(|status| status.code())
        .map_or("null".to_string(), |code| code.to_string());
    exited.send_replace(Some(lost(&format!(
        "pi-env exited with code {code} before it was ready"
    ))));
    let error = lost("pi-env connection lost");
    match inner.upgrade() {
        Some(inner) => inner.teardown(&session, error),
        None => {
            session.end(&error);
        }
    }
}

/// Pings every five seconds, and ends a session that has been silent for 30.
async fn ping(inner: Weak<Inner>, session: Arc<Session>) {
    let mut interval = tokio::time::interval(PING_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        if !session.is_live() {
            return;
        }
        session.write(PING, 0, Json::new(), &[]);
        if lock(&session.last_seen).elapsed() > SILENCE_LIMIT {
            let error = lost("pi-env connection timed out");
            match inner.upgrade() {
                Some(inner) => inner.teardown(&session, error),
                None => {
                    session.end(&error);
                }
            }
            return;
        }
    }
}

/// Reads the daemon's stdout: noise up to the sync line, then frames.
async fn read_frames(inner: Weak<Inner>, session: Arc<Session>, mut stdout: ChildStdout) {
    let corrupt = |session: &Arc<Session>| {
        let error = lost("Corrupt frame from pi-env");
        match inner.upgrade() {
            Some(inner) => inner.teardown(session, error),
            None => {
                session.end(&error);
            }
        }
    };
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut synced = false;
    let marker = format!("PI-ENV {}\n", session.token).into_bytes();
    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if !session.is_live() {
            return;
        }
        *lock(&session.last_seen) = Instant::now();
        buffer.extend_from_slice(&chunk[..read]);
        if !synced {
            // Shell startup files may print before the daemon runs; skip everything before its sync line.
            let Some(index) = find(&buffer, &marker) else {
                if buffer.len() > MAX_NOISE {
                    buffer.drain(..buffer.len() - marker.len());
                }
                continue;
            };
            let noise = String::from_utf8_lossy(&buffer[..index]).trim().to_string();
            if !noise.is_empty() {
                if let Some(inner) = inner.upgrade() {
                    inner.log(&noise);
                }
            }
            buffer.drain(..index + marker.len());
            synced = true;
        }
        let mut consumed = 0;
        while session.is_live() && buffer.len() - consumed >= 4 {
            let rest = &buffer[consumed..];
            let length = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
            if !(9..=MAX_FRAME).contains(&length) {
                corrupt(&session);
                return;
            }
            if rest.len() < 4 + length {
                break;
            }
            let body = &rest[4..4 + length];
            let kind = body[0];
            let id = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
            let json_length = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as usize;
            let json = if 9 + json_length <= length {
                serde_json::from_slice::<Value>(&body[9..9 + json_length]).ok()
            } else {
                None
            };
            let Some(Value::Object(json)) = json else {
                corrupt(&session);
                return;
            };
            let payload = body[9 + json_length..].to_vec();
            consumed += 4 + length;
            dispatch(&session, kind, id, json, payload);
        }
        buffer.drain(..consumed);
    }
}

fn dispatch(session: &Session, kind: u8, id: u32, json: Json, payload: Vec<u8>) {
    if kind == EVENT {
        if let Some(request) = lock(&session.pending).get(&id) {
            let _ = request.send(Incoming::Event(json, payload));
        }
        return;
    }
    if kind != RESULT && kind != ERROR {
        return;
    }
    let Some(request) = lock(&session.pending).remove(&id) else {
        return;
    };
    let result = if kind == RESULT {
        Ok(Reply {
            json,
            payload,
            session: session.id,
        })
    } else {
        Err(RemoteError::new(json))
    };
    let _ = request.send(Incoming::Done(result));
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 32 hexadecimal digits, as upstream's `randomBytes(16)`; the token only marks where the daemon's output starts.
fn random_token() -> String {
    crate::local::random_uuid().replace('-', "")
}

async fn write_frames(stdin: Option<ChildStdin>, mut frames: mpsc::UnboundedReceiver<Vec<u8>>) {
    let Some(mut stdin) = stdin else {
        return;
    };
    while let Some(frame) = frames.recv().await {
        if stdin.write_all(&frame).await.is_err() {
            // As upstream, write errors are ignored: the process's exit ends the session.
            while frames.recv().await.is_some() {}
            return;
        }
    }
    let _ = stdin.shutdown().await;
}

async fn read_log(inner: Weak<Inner>, mut stderr: ChildStderr) {
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if let Some(inner) = inner.upgrade() {
                    inner.log(&String::from_utf8_lossy(&chunk[..read]));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platforms_have_node_names() {
        let names = |os: &str, arch: &str| {
            let (platform, arch) = node_platform(os, arch);
            format!("{platform}-{arch}")
        };
        assert_eq!(names("windows", "aarch64"), "win32-arm64");
        assert_eq!(names("macos", "x86_64"), "darwin-x64");
        assert_eq!(names("linux", "riscv64"), "linux-riscv64");
    }

    #[test]
    fn errors_keep_their_fields_and_tell_a_lost_connection() {
        let error = RemoteError::with("ENOENT", "missing", false);
        assert_eq!(
            (error.code.as_str(), error.message.as_str()),
            ("ENOENT", "missing")
        );
        assert!(!error.is_connection_lost());
        assert!(lost("gone").is_connection_lost());
        assert!(!closed().is_connection_lost());
        let bare = RemoteError::new(Json::new());
        assert_eq!(
            (bare.code.as_str(), bare.message.as_str()),
            ("unknown", "Remote operation failed")
        );
        let token = random_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
