//! `noel --attach <session-id>`: the TUI on a session that `noel server`
//! holds (spec §7.4).
//!
//! The terminal, the editor and the transcript are the local TUI's; only the
//! session's backend differs. Instead of a `Runtime` in this process, the
//! session is a `noel.chat` reference through the server
//! ([`crate::server::client`]), which is started when none listens:
//!
//! - The transcript is rebuilt from the view's `entries` (upstream `Message`
//!   JSON) as `/resume` rebuilds it from saved messages, followed by what
//!   the active run streams in `docs["pi.live"]`: its reply's committed
//!   partial and its tools' output. Each change of the view updates the rows
//!   it touches in place, so a reply that streams grows one entry, and every
//!   attached TUI shows the same run.
//! - Enter while idle submits with `whenBusy: "reject"`; while a run is
//!   active, Enter queues a follow-up and the steer key a steer, as the local
//!   TUI does. Every submission has its own `requestId`, sent again with the
//!   same id when it fails `unavailable`. Esc (or the interrupt key) aborts
//!   the run, and Up on an empty draft withdraws the last queued submission
//!   into the draft.
//! - `/model <provider/id>`, `/thinking <level>` and `/compact` are
//!   `configure` and `compact`. Commands that act on this process's own
//!   session (sessions, settings, accounts, local shell commands) say they
//!   are not available when attached.
//! - The server owns resuming: the TUI never continues a run itself. A lost
//!   worker is a note, then the view is held again and redrawn from its
//!   snapshot; a lost server is reconnected (and started) as `noel client
//!   watch` does.
//! - An unknown session is an error before the terminal is taken.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use noel_chord::v2::host::StateUpdate;
use noel_chord::v2::WireError;
use noel_config::LoadedConfig;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::server::client::{connect, Client, Endpoint, Followed};
use crate::server::protocol::{
    started_run, to_args, AbortArgs, CommandArgs, ForkArgs, ImageArg, ImportArgs, SessionArgs,
    SubmitArgs, WhenBusy, WithdrawArgs,
};
use crate::server::{new_request_id, valid_session_id};
use crate::{
    Activity, TuiApp, TuiEntry, TuiEntryKind, TuiTerminal, QUIT_CONFIRM_WINDOW,
    STREAM_RENDER_INTERVAL_MS,
};

/// How long the first view may take: starting a server and a worker.
const FIRST_VIEW: Duration = Duration::from_secs(30);

/// `quit_ends`: quitting ends the session's run and, with no other client,
/// the session, as a local `noel` quits (plain `noel` as a client);
/// otherwise quitting detaches (`--attach`). Returns the session shown last
/// when the TUI should restart (a rebuilt binary, dogfood).
pub(crate) async fn run(
    session: &str,
    endpoint: Endpoint,
    mut config: LoadedConfig,
    owned_screen: bool,
    quit_ends: bool,
    no_splash: bool,
) -> Result<Option<String>> {
    if !valid_session_id(session) {
        return Err(anyhow!("--attach: {session:?} is not a session id"));
    }
    let (sender, mut incoming) = mpsc::unbounded_channel();
    let reports = sender.clone();
    let local_machine = matches!(endpoint, Endpoint::Local);
    let place = match &endpoint {
        Endpoint::Local => "noel server".to_owned(),
        Endpoint::Ssh { destination, .. } => format!("noel server on {destination}"),
        Endpoint::Paired(paired) => format!("noel server on {}:{}", paired.host, paired.port),
    };
    let client = connect(
        endpoint,
        Arc::new(move |message| {
            let _ = reports.send(Incoming::Report(message));
        }),
    )
    .await?;
    let (mut follower, first, early) =
        match first_view(&client, session, &sender, &mut incoming).await {
            Ok(attached) => attached,
            Err(error) => {
                client.close().await;
                return Err(error);
            }
        };

    let (restore, mut terminal, mut viewport_height) = crate::open_tui_terminal(owned_screen)?;
    let mut app = TuiApp {
        owned_screen,
        status: "Ready".to_owned(),
        ..TuiApp::default()
    };
    // A local `noel` greets as it always has; an attach says where it is.
    let greeting = if quit_ends {
        let renderer = crate::terminal_renderer(&config);
        format!(
            "{}\ntype /help for commands, /reload to reload config, /quit to exit",
            if no_splash {
                renderer.banner()
            } else {
                renderer.splash()
            }
        )
    } else {
        format!(
            "{}\nattached to session {session} on {place} · /help for commands, /quit to detach",
            crate::terminal_renderer(&config).banner()
        )
    };
    app.push(TuiEntryKind::System, greeting);
    if let Err(error) = crate::terminal_theme(&config) {
        app.push(TuiEntryKind::Error, format!("{error}; using system theme"));
    }
    let mut session = session.to_owned();
    let mut first = first;
    let mut early = early;
    let mut switched = false;
    let mut auto_restart = crate::AutoRestart::from_env();
    // A local `noel` that a signal ends takes its session's worker with it
    // when no other client holds the session, as its tools would end with
    // it in process.
    let local = Arc::new(LocalHolds::default());
    if quit_ends {
        let local = local.clone();
        crate::signals::set_exit_hook(Some(Box::new(move || {
            if let Some(session) = local.alone() {
                stop_local_worker(&session);
            }
        })));
    }
    let result = loop {
        let mut remote = Remote::new(&session, client.clone(), sender.clone());
        remote.local_machine = local_machine;
        if let Ok(mut held) = local.session.lock() {
            held.clone_from(&session);
        }
        remote.local = local.clone();
        if std::mem::take(&mut switched) {
            remote.run_command("/session".to_owned());
        }
        // What came before the view: starting a server, sending again.
        for message in std::mem::take(&mut early) {
            remote.receive(&mut app, message);
        }
        // The whole history before the view's window, as reopening a
        // session shows all of it.
        if let Err(error) = remote
            .history
            .load_earlier(&remote.client, &session, &first)
            .await
        {
            app.push(
                TuiEntryKind::Error,
                format!("could not load the earlier history: {error:#}"),
            );
        }
        remote.first(&mut app, first);
        let ended = remote
            .drive(
                &mut app,
                &mut terminal,
                &mut viewport_height,
                &mut config,
                &mut incoming,
                &mut auto_restart,
            )
            .await;
        follower.abort();
        let next = match ended {
            Ok(Some(next)) => next,
            Ok(None) if quit_ends => {
                // As a local quit: the run is aborted, then the session ends
                // unless another client holds it.
                if remote.run.running().is_some() {
                    let abort = AbortArgs {
                        expected_run_id: None,
                        keep_queue: true,
                    };
                    let _ = client
                        .chat_call(&session, "abort", to_args(&abort), false)
                        .await;
                }
                let args = to_args(&CommandArgs {
                    line: "/quit".to_owned(),
                    request_id: Some(new_request_id()),
                    errors: None,
                });
                if let Ok(answer) = client.chat_call(&session, "command", args, false).await {
                    remote.command_answer(&mut app, "/quit", &answer, None);
                    // The session ends with its worker, which lets go of the
                    // journal: the next `noel` opens it as this one quit it.
                    // Released first: a hold left would start it again.
                    if answer["leaving"] == true {
                        client.close().await;
                        wait_for_worker_exit(&session).await;
                    }
                }
                break Ok(remote.restart.then(|| session.clone()));
            }
            Ok(None) => break Ok(remote.restart.then(|| session.clone())),
            Err(error) => break Err(error),
        };
        // Another session a command named (a fork, a pick): its view, or
        // back to this one if it cannot be held. A local `noel`'s flags go
        // with it, as with the session it opened.
        if quit_ends {
            match crate::hand_over_flags(&next, true) {
                Ok(Some(notice)) => app.push(TuiEntryKind::System, notice),
                Ok(None) => {}
                Err(error) => app.push(TuiEntryKind::Error, format!("{error:#}")),
            }
        }
        let attached = match first_view(&client, &next, &sender, &mut incoming).await {
            Ok(attached) => {
                session = next;
                Ok(attached)
            }
            Err(error) => {
                app.push(TuiEntryKind::Error, format!("{error:#}"));
                first_view(&client, &session, &sender, &mut incoming).await
            }
        };
        match attached {
            Ok((next_follower, next_first, next_early)) => {
                follower = next_follower;
                first = next_first;
                early = next_early;
                // What was shown stays, as a local switch keeps it; the
                // session's details follow, as a local switch prints them.
                switched = true;
            }
            Err(error) => break Err(error),
        }
    };
    crate::signals::set_exit_hook(None);
    crate::close_tui_terminal(terminal, restore, &app)?;
    client.close().await;
    result
}

/// What the signal hook knows of the session's holds.
#[derive(Default)]
struct LocalHolds {
    session: std::sync::Mutex<String>,
    /// The holds the view last reported (`noel.clients`).
    holds: AtomicU64,
    /// This client's calls in flight, each a hold of its own.
    calls: Arc<AtomicUsize>,
}

impl LocalHolds {
    /// The session, when no other client holds it.
    fn alone(&self) -> Option<String> {
        let holds = self.holds.load(Ordering::SeqCst);
        let ours = 1 + self.calls.load(Ordering::SeqCst) as u64;
        let session = self.session.lock().ok()?.clone();
        (holds > 0 && holds <= ours && !session.is_empty()).then_some(session)
    }

    /// Counts a call until the guard drops.
    fn calling(&self) -> Calling {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Calling(self.calls.clone())
    }
}

struct Calling(Arc<AtomicUsize>);

impl Drop for Calling {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Ends the local worker of `session` (SIGTERM: it kills its tools' groups
/// and exits).
fn stop_local_worker(session: &str) {
    let Ok(dir) = crate::server::RuntimeDir::check(crate::server::RuntimeDir::locate()) else {
        return;
    };
    let record = std::fs::read_to_string(dir.worker_record(session))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    if let Some(pid) = record.and_then(|record| record["pid"].as_i64()) {
        if let Ok(pid) = i32::try_from(pid) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

/// Waits, briefly, for the local worker of `session` to exit: its socket
/// goes when it does.
async fn wait_for_worker_exit(session: &str) {
    let Ok(dir) = crate::server::RuntimeDir::check(crate::server::RuntimeDir::locate()) else {
        return;
    };
    let socket = dir.worker_socket(session);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while socket.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Follows `session` and waits for its first view, keeping what came
/// before it. An unknown session is an error.
async fn first_view(
    client: &Client,
    session: &str,
    sender: &mpsc::UnboundedSender<Incoming>,
    incoming: &mut mpsc::UnboundedReceiver<Incoming>,
) -> Result<(tokio::task::JoinHandle<anyhow::Error>, Value, Vec<Incoming>)> {
    let mut follower = {
        let (client, session, sender) = (client.clone(), session.to_owned(), sender.clone());
        tokio::spawn(async move {
            client
                .follow(&session, |followed| {
                    let _ = sender.send(Incoming::Followed(followed));
                })
                .await
        })
    };
    let mut early = Vec::new();
    let first = tokio::time::timeout(FIRST_VIEW, async {
        loop {
            tokio::select! {
                message = incoming.recv() => match message {
                    Some(Incoming::Followed(Followed::Update(StateUpdate::Snapshot(view)))) => {
                        return Ok(view)
                    }
                    Some(Incoming::Followed(Followed::Update(StateUpdate::Failed(error)))) => {
                        return Err(attach_error(session, error.into()))
                    }
                    Some(other) => early.push(other),
                    None => return Err(anyhow!("the connection to noel server ended")),
                },
                ended = &mut follower => {
                    let error = ended.unwrap_or_else(|error| anyhow!("{error}"));
                    return Err(attach_error(session, error));
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(anyhow!(
            "no view of session {session} within {} s",
            FIRST_VIEW.as_secs()
        ))
    });
    match first {
        Ok(view) => Ok((follower, view, early)),
        Err(error) => {
            follower.abort();
            Err(error)
        }
    }
}

/// An attach's error, naming an unknown session plainly.
fn attach_error(session: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<WireError>() {
        Some(chord) if crate::server::errors::is_no_session(chord) => {
            anyhow!("no session {session} on noel server (`noel client ls` lists its sessions)")
        }
        _ => anyhow!("could not attach to session {session}: {error}"),
    }
}

/// Runs a session command on `noel.sessions` for the TUI showing `session`
/// (in `cwd`): the session made or deleted, or null when the worker runs it
/// (deleting `session` itself, importing into a session a worker holds).
async fn sessions_request(client: &Client, session: &str, cwd: &str, line: &str) -> Result<Value> {
    let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let named = |reference: &str| {
        let reference = reference.to_owned();
        async move { resolve_listed(client, &reference).await }
    };
    match command {
        "/fork" | "/clone" => {
            let source = if rest.is_empty() {
                session.to_owned()
            } else {
                named(rest).await?
            };
            let args = ForkArgs {
                request_id: new_request_id(),
                session_id: source,
                clone: command == "/clone",
            };
            client.sessions_call("fork", to_args(&args)).await
        }
        "/import" => {
            let path = std::path::Path::new(cwd).join(rest);
            let args = ImportArgs {
                request_id: new_request_id(),
                path,
            };
            match client.sessions_call("import", to_args(&args)).await {
                // The export of a session a worker holds (this one) goes
                // in through that worker.
                Err(error) if format!("{error:#}").contains("in use by another noel process") => {
                    Ok(Value::Null)
                }
                imported => imported,
            }
        }
        _ => {
            let target = named(rest).await?;
            if target == session {
                return Ok(Value::Null);
            }
            client
                .sessions_call("delete", to_args(&SessionArgs { session_id: target }))
                .await
        }
    }
}

/// The session the server lists under `reference`: its id, the one id it
/// begins, or its name.
async fn resolve_listed(client: &Client, reference: &str) -> Result<String> {
    let directory = client.directory().await?;
    let sessions = directory["sessions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let id = |session: &Value| session["sessionId"].as_str().unwrap_or_default().to_owned();
    if let Some(exact) = sessions
        .iter()
        .find(|session| session["sessionId"] == reference)
    {
        return Ok(id(exact));
    }
    let matching = sessions
        .iter()
        .filter(|session| {
            id(session).starts_with(reference) || session["name"].as_str() == Some(reference)
        })
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [one] => Ok(id(one)),
        [] => Err(anyhow!("no session {reference}")),
        _ => Err(anyhow!("{reference} names {} sessions", matching.len())),
    }
}

/// What reaches the TUI from the server.
enum Incoming {
    /// The view, and what happened to the link or the hold.
    Followed(Followed),
    /// What the client did on its own: started a server, sent a call again.
    Report(String),
    /// A call's answer, with the generation of the `Remote` that sent it:
    /// an answer for a session switched away from is dropped (#146).
    Answer(Call, Result<Value>, u64),
}

/// Each `Remote` (each session attached to in turn) has its own generation.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// The row showing a `!` command run on the worker, and the call whose
/// answer fills it (#146: any command's answer used to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellRow {
    index: usize,
    pending: Pending,
}

/// The calls the TUI makes.
enum Call {
    /// A submission, with its text.
    Submit(String, Pending),
    /// A slash command for the worker to run, with its line.
    Command(String, Pending),
    /// A withdrawal of a queued submission, with its text; `recall` puts the
    /// text back into an empty draft.
    Withdraw {
        id: String,
        text: String,
        recall: bool,
    },
    Abort,
    /// A session command `noel.sessions` answers, with its line: the session
    /// it made, the one it deleted, or `None` to leave it to the worker.
    Sessions(String, Pending),
}

/// A call this TUI waits on that may start a run: "Working..." shows while
/// any is pending, and each answer ends only its own (#133).
type Pending = u64;

/// This TUI's view of the session's run (#133): one state, where it was a
/// counter, three Options and two flags that could disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunView {
    Idle,
    /// A call this TUI made started run `run_id`; the view has not shown it.
    /// It gives up after [`STARTING_TIMEOUT`], should the view never show it.
    Starting {
        run_id: String,
        since: Instant,
    },
    /// The view shows run `run_id` active, first seen at `since`.
    Running {
        run_id: String,
        since: Instant,
        saw_tools: bool,
        /// An abort was sent for it.
        aborting: bool,
    },
}

/// How long a started run may go unseen in the view before the TUI stops
/// showing "Working..." for it.
const STARTING_TIMEOUT: Duration = Duration::from_secs(10);

impl RunView {
    fn running(&self) -> Option<&str> {
        match self {
            Self::Running { run_id, .. } => Some(run_id),
            _ => None,
        }
    }

    /// The next state for a view whose active run is `live` (whether it has
    /// run a tool) and whose last ended run is `last_run`.
    fn advance(self, live: Option<&str>, tools: bool, last_run: Option<&str>) -> Self {
        match (self, live) {
            // The same run, still active.
            (
                Self::Running {
                    run_id,
                    since,
                    saw_tools,
                    aborting,
                },
                Some(live_id),
            ) if run_id == live_id => Self::Running {
                run_id,
                since,
                saw_tools: saw_tools || tools,
                aborting,
            },
            // A run this TUI has not seen active before.
            (_, Some(live_id)) => Self::Running {
                run_id: live_id.to_owned(),
                since: Instant::now(),
                saw_tools: tools,
                aborting: false,
            },
            // A started run not shown yet waits, unless it already ended
            // (it is the last run) or the view never shows it.
            (Self::Starting { run_id, since }, None)
                if since.elapsed() < STARTING_TIMEOUT && last_run != Some(run_id.as_str()) =>
            {
                Self::Starting { run_id, since }
            }
            (_, None) => Self::Idle,
        }
    }
}

/// What a key or a command asks of the loop.
enum Action {
    None,
    Quit,
    /// `/clear`: the screen as well as the transcript.
    Clear,
}

/// The session as the server shows it, and the calls into it.
struct Remote {
    /// The server runs on this machine: a login runs here, in the terminal,
    /// writing the auth file the worker reads; otherwise it runs on the
    /// server's machine, relayed (`noel.accounts`).
    local_machine: bool,
    /// A login the worker asked for, run once the loop has the terminal.
    login: Option<(String, String)>,
    /// How many holds the session has and how many are this client's
    /// calls, for the signal hook.
    local: Arc<LocalHolds>,
    /// The last submission sent: the next waits for its answer, so the
    /// worker queues them in the order typed.
    last_submit: Option<tokio::task::JoinHandle<()>>,
    session: String,
    client: Client,
    sender: mpsc::UnboundedSender<Incoming>,
    /// The view as of the last update, its entries every entry the
    /// session's history holds here (not only the view's window).
    view: Value,
    /// The session's history: the windows seen and the pages loaded.
    history: noel_client::Transcript,
    transcript: Transcript,
    /// The session's run as this TUI sees it.
    run: RunView,
    /// The run this TUI interrupted, noted as locally once it ends.
    interrupted: Option<String>,
    /// The binary was rebuilt: the TUI restarts (dogfood).
    restart: bool,
    /// The retry last noted (its `at`).
    retry_noted: Option<u64>,
    /// The worker's notes shown, by id; `None` until the first view, whose
    /// notes are old.
    notices_seen: Option<u64>,
    /// Queued submissions a withdrawal was sent for.
    withdrawing: Vec<String>,
    /// The last run as last shown: `(runId, status)`.
    last_run: Option<(String, String)>,
    /// Why the view is not current: its worker or the server was lost.
    waiting: Option<&'static str>,
    /// A session a command named, to attach to next.
    switch_to: Option<String>,
    /// The model picker's provider layer: each provider's models.
    provider_layers: std::collections::HashMap<String, Value>,
    /// Calls sent that may start a run, not answered yet.
    pending: std::collections::BTreeSet<Pending>,
    next_pending: Pending,
    /// The row of the `!` command running on the worker.
    shell_row: Option<ShellRow>,
    /// This Remote's generation, which its calls' answers carry.
    generation: u64,
    /// The diff panel's files' diffs, by path, as the worker read them.
    diff_lines: std::collections::HashMap<String, Vec<String>>,
}

impl Remote {
    fn new(session: &str, client: Client, sender: mpsc::UnboundedSender<Incoming>) -> Self {
        Self {
            session: session.to_owned(),
            client,
            sender,
            view: Value::Null,
            history: noel_client::Transcript::default(),
            transcript: Transcript::default(),
            run: RunView::Idle,
            interrupted: None,
            restart: false,
            retry_noted: None,
            notices_seen: None,
            withdrawing: Vec::new(),
            last_run: None,
            waiting: None,
            switch_to: None,
            diff_lines: std::collections::HashMap::new(),
            shell_row: None,
            generation: GENERATIONS.fetch_add(1, Ordering::Relaxed),
            pending: std::collections::BTreeSet::new(),
            next_pending: 0,
            provider_layers: std::collections::HashMap::new(),
            last_submit: None,
            local: Arc::default(),
            local_machine: false,
            login: None,
        }
    }

    /// The first view: the transcript, and its prompts as the editor's
    /// history, as reopening a session restores them.
    fn first(&mut self, app: &mut TuiApp, view: Value) {
        for entry in self.history.merge(&view).iter().map(message_of) {
            if entry["role"] == "user" {
                app.editor_state.record_history(blocks(entry, "text"));
            }
        }
        self.last_run = last_run(&view);
        // As `noel` opening a session: a turn its saved context left
        // unfinished continues once, then what is queued after it.
        let docs = &view["docs"];
        let unfinished =
            docs["noel.turn"]["unfinished"] == true && docs["pi.live"]["run"].is_null();
        let queued = docs["pi.inbox"]["items"].as_array().map_or(0, Vec::len);
        let hint = crate::resume_hint(unfinished, queued);
        self.update(app, StateUpdate::Snapshot(view));
        if let Some(hint) = hint {
            app.push(TuiEntryKind::System, hint);
        }
        if unfinished {
            self.continue_turn();
        }
    }

    fn receive(&mut self, app: &mut TuiApp, message: Incoming) {
        match message {
            Incoming::Followed(Followed::Update(update)) => self.update(app, update),
            Incoming::Followed(Followed::ServerLost) => {
                self.waiting = Some("reconnecting to noel server");
                app.push(
                    TuiEntryKind::System,
                    "lost noel server; reconnecting (starting one if none runs)",
                );
            }
            Incoming::Followed(Followed::Reconnected) => {
                self.waiting = Some("holding the session again");
                app.push(
                    TuiEntryKind::System,
                    "reconnected to noel server; holding the session again",
                );
            }
            Incoming::Followed(Followed::ViewEnded(why)) => {
                self.waiting = Some("holding the session again");
                app.push(
                    TuiEntryKind::System,
                    format!("lost the session's worker ({why}); holding the session again"),
                );
            }
            Incoming::Report(message) => app.push(TuiEntryKind::System, message),
            Incoming::Answer(call, result, generation) => {
                if generation == self.generation {
                    self.answer(app, call, result);
                }
            }
        }
    }

    fn update(&mut self, app: &mut TuiApp, update: StateUpdate) {
        let mut view = match update {
            StateUpdate::Snapshot(view) => {
                if self.waiting.take().is_some() {
                    app.push(
                        TuiEntryKind::System,
                        "holding the session again: redrawn from its snapshot",
                    );
                }
                view
            }
            StateUpdate::Ops { value, .. } => value,
            StateUpdate::Failed(error) => {
                app.push(TuiEntryKind::Error, format!("the session's view: {error}"));
                return;
            }
        };
        // The view holds a window of the history; the TUI shows all of it.
        view["entries"] = Value::Array(self.history.merge(&view));
        let dirty = first_difference(&self.view, &view);
        self.view = view;
        self.sync_transcript(app, dirty);
        if let Some(count) = self.view["docs"]["noel.clients"]["count"].as_u64() {
            self.local.holds.store(count, Ordering::SeqCst);
        }
        self.note_retry(app);
        self.note_notices(app);
        self.note_last_run(app);
    }

    /// Shows the worker's new notes (an account failover's switch).
    fn note_notices(&mut self, app: &mut TuiApp) {
        let items = self.view["docs"]["noel.notices"]["items"].as_array();
        let newest = items
            .and_then(|items| items.last())
            .and_then(|item| item["id"].as_u64())
            .unwrap_or(0);
        let Some(seen) = self
            .notices_seen
            .replace(newest.max(self.notices_seen.unwrap_or(0)))
        else {
            return;
        };
        for item in items.into_iter().flatten() {
            if item["id"].as_u64().is_some_and(|id| id > seen) {
                if let Some(text) = item["text"].as_str() {
                    app.push(TuiEntryKind::System, text.to_owned());
                }
            }
        }
    }

    /// Notes each retry the run waits out once, as the in-process TUI
    /// does: the attempt that failed, the backoff and why.
    fn note_retry(&mut self, app: &mut TuiApp) {
        let generation = &self.view["docs"]["pi.live"]["generation"];
        let retry = &generation["retry"];
        let Some(error) = retry["error"].as_str() else {
            return;
        };
        let at = retry["at"].as_u64();
        if at.is_none() || at == self.retry_noted {
            return;
        }
        self.retry_noted = at;
        let attempt = generation["attempt"]
            .as_u64()
            .unwrap_or(2)
            .saturating_sub(1);
        let text = match (retry["delayMs"].as_u64(), retry["maxAttempts"].as_u64()) {
            (Some(delay), Some(most)) => {
                let delay = if delay >= 1000 {
                    format!("{}s", delay / 1000)
                } else {
                    format!("{delay}ms")
                };
                format!("retrying in {delay} (attempt {attempt} of {most}): {error}")
            }
            _ => format!("retrying (attempt {attempt}): {error}"),
        };
        app.push(TuiEntryKind::System, text);
    }

    /// Notes a run that another client's abort or a failure ended; a
    /// completed run, or one the server resumes, says nothing.
    fn note_last_run(&mut self, app: &mut TuiApp) {
        let now = last_run(&self.view);
        if now == self.last_run {
            return;
        }
        self.last_run.clone_from(&now);
        let Some((run_id, status)) = now else { return };
        // The run this TUI interrupted has ended, aborted or not: its note
        // is due once, then forgotten.
        if self.interrupted.as_deref() == Some(run_id.as_str()) {
            self.interrupted = None;
            if status == "aborted" {
                app.push(TuiEntryKind::System, "interrupted");
                return;
            }
        }
        match status.as_str() {
            "failed" => app.push(
                TuiEntryKind::Error,
                format!(
                    "run failed: {}",
                    self.view["lastRun"]["error"]
                        .as_str()
                        .unwrap_or("unknown error")
                ),
            ),
            // An aborted reply already shows `aborted` after it.
            "aborted"
                if entries(&self.view)
                    .last()
                    .is_none_or(|entry| message_of(entry)["stopReason"] != "aborted") =>
            {
                app.push(TuiEntryKind::System, "aborted")
            }
            _ => {}
        }
    }

    /// The footer and the panels: the queue from `pi.inbox`, the run and
    /// what it does from `pi.live`, the model from `pi.agent`.
    fn refresh_chrome(&mut self, app: &mut TuiApp, config: &LoadedConfig) {
        app.show_hardware_cursor = config.settings.show_hardware_cursor.unwrap_or(true);
        let docs = &self.view["docs"];
        app.todos = serde_json::from_value(docs["noel.todos"]["items"].clone()).unwrap_or_default();
        let live = &docs["pi.live"];
        app.queued_messages = docs["pi.inbox"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| item["content"].as_str().unwrap_or_default().to_owned())
            .collect();
        let tools = live["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        let last_run = self.last_run.as_ref().map(|(run_id, _)| run_id.as_str());
        let run = std::mem::replace(&mut self.run, RunView::Idle);
        self.run = run.advance(live["run"]["runId"].as_str(), tools, last_run);
        app.turn_running = matches!(self.run, RunView::Running { .. });
        app.turn_saw_tools = matches!(
            self.run,
            RunView::Running {
                saw_tools: true,
                ..
            }
        );
        let queued = app.queued_messages.len();
        app.status = if let Some(why) = self.waiting {
            format!("{why}… · /quit to detach")
        } else if let RunView::Running {
            since, aborting, ..
        } = &self.run
        {
            let status = crate::activity_status(*since, &activity(live), queued);
            if *aborting {
                status.replace("esc to interrupt", "aborting")
            } else {
                status
            }
        } else if !self.pending.is_empty() || matches!(self.run, RunView::Starting { .. }) {
            // Sent, and not shown running yet: as the in-process TUI shows a
            // prompt it is about to run.
            "Working...".to_owned()
        } else {
            let agent = &docs["pi.agent"];
            let mut status = format!(
                "{} {} {} ≡ {queued} ↺ {} · attached",
                agent["model"].as_str().unwrap_or("-"),
                agent["thinkingLevel"].as_str().unwrap_or("-"),
                config.settings.theme.as_deref().unwrap_or("system"),
                app.editor_state.history().len()
            );
            if let Some((_, last)) = &self.last_run {
                if matches!(last.as_str(), "aborted" | "failed" | "interrupted") {
                    status.push_str(&format!(" · last run {last}"));
                }
            }
            status
        };
        if let Some(since) = app.quit_armed_since {
            if since.elapsed() < QUIT_CONFIRM_WINDOW {
                app.status = "press ctrl+c/ctrl+d again to quit".to_owned();
            } else {
                app.quit_armed_since = None;
            }
        }
        let conversation = &self.view["conversation"];
        let title = conversation["name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| self.session.chars().take(8).collect());
        app.header_line = format!(
            "{}  {title}  queue:{queued}",
            conversation["cwd"].as_str().unwrap_or_default()
        );
    }

    /// Runs the TUI until it quits: the server's updates and the calls'
    /// answers as they arrive, the terminal's keys at the render interval.
    async fn drive(
        &mut self,
        app: &mut TuiApp,
        terminal: &mut TuiTerminal,
        viewport_height: &mut u16,
        config: &mut LoadedConfig,
        incoming: &mut mpsc::UnboundedReceiver<Incoming>,
        auto_restart: &mut crate::AutoRestart,
    ) -> Result<Option<String>> {
        let mut tick = tokio::time::interval(Duration::from_millis(STREAM_RENDER_INTERVAL_MS * 2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.refresh_chrome(app, config);
        crate::redraw_tui(terminal, app, config)?;
        loop {
            let mut changed = false;
            tokio::select! {
                message = incoming.recv() => {
                    // This holds a sender, so the channel stays open.
                    let Some(message) = message else { return Ok(None) };
                    self.receive(app, message);
                    while let Ok(message) = incoming.try_recv() {
                        self.receive(app, message);
                    }
                    changed = true;
                }
                _ = tick.tick() => {
                    while event::poll(Duration::ZERO)? {
                        changed = true;
                        match event::read()? {
                            Event::Key(key) if key.kind == KeyEventKind::Press => {
                                match self.key(key, app, config).await {
                                    Action::None => {}
                                    Action::Quit => return Ok(None),
                                    Action::Clear => {
                                        self.clear(app);
                                        crate::clear_terminal_screen(terminal, app.owned_screen)?;
                                    }
                                }
                            }
                            Event::Mouse(mouse) => {
                                crate::handle_transcript_mouse(mouse, app);
                            }
                            Event::Paste(text) => app.paste_text(&text),
                            Event::Resize(_, _) => {
                                crate::resize_tui_viewport(
                                    terminal,
                                    viewport_height,
                                    app.owned_screen,
                                )?;
                                // As the in-process loop: drawn once at the
                                // new size, then again below, which places
                                // the cursor.
                                crate::redraw_tui(terminal, app, config)?;
                            }
                            _ => {}
                        }
                    }
                    crate::copy_pending_selection(app).await;
                    // The spinner moves while a run is active.
                    changed |= app.turn_running || app.quit_armed_since.is_some();
                }
            }
            if let Some(next) = self.switch_to.take() {
                return Ok(Some(next));
            }
            // A login runs on the plain terminal, as locally; the worker then
            // reloads, so the new account is selectable at once.
            if let Some((provider, account)) = self.login.take() {
                let mut surface = crate::TuiSurface {
                    terminal,
                    viewport_height,
                };
                let login = if self.local_machine {
                    crate::run_tui_login(&mut surface, app, config, &provider, &account).await
                } else {
                    let client = self.client.clone();
                    let flow = async {
                        crate::server::client::relay_login(&client, &provider, &account, |kind| {
                            use std::io::{BufRead, Write};
                            match kind {
                                "key" => println!("paste the {provider} API key:"),
                                _ => println!(
                                    "after signing in, paste the full code shown (code#state):"
                                ),
                            }
                            let _ = std::io::stdout().flush();
                            let mut input = String::new();
                            std::io::stdin().lock().read_line(&mut input)?;
                            Ok(input.trim().to_owned())
                        })
                        .await
                    };
                    crate::with_suspended_tui(&mut surface, app, config, flow)
                        .await
                        .and_then(|outcome| outcome)
                };
                match login {
                    Ok(line) => {
                        app.push(TuiEntryKind::System, line);
                        self.run_command("/reload".to_owned());
                    }
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
                changed = true;
            }
            // A rebuilt `noel` (dogfood) restarts between turns, as the
            // in-process TUI does: never mid-run or mid-turn.
            if self.run.running().is_none()
                && !app.turn_running
                && self.view["docs"]["noel.turn"]["unfinished"] != true
                && auto_restart.should_restart()?
            {
                app.push(TuiEntryKind::System, "rebuilt; restarting");
                crate::redraw_tui(terminal, app, config)?;
                self.restart = true;
                return Ok(None);
            }
            if changed {
                self.refresh_chrome(app, config);
                crate::redraw_tui(terminal, app, config)?;
            }
        }
    }

    /// `/clear`: what is shown now is cleared; later entries and the run
    /// still show.
    fn clear(&mut self, app: &mut TuiApp) {
        app.clear_visible();
        // Its row is gone; the command's answer still shows as rows.
        self.shell_row = None;
        self.transcript = Transcript {
            hidden: entries(&self.view).len(),
            ..Transcript::default()
        };
        self.sync_transcript(app, 0);
    }

    // ─── keys ───────────────────────────────────────────────────────────────

    async fn key(&mut self, key: KeyEvent, app: &mut TuiApp, config: &mut LoadedConfig) -> Action {
        let bindings = crate::keybinding_map(config);
        let name = crate::key_event_name(&key);
        let bound = |action: &str| {
            name.as_deref()
                .is_some_and(|name| bindings.matches(action, name))
        };
        if app.selector().is_some() {
            self.selector_key(key, app, config);
            return Action::None;
        }
        if app.diff_panel().is_some() {
            let lines = &self.diff_lines;
            let handled = crate::diff_panel_key(&key, app, |entry| crate::DiffDetail {
                path: entry.path.clone(),
                lines: lines.get(&entry.path).cloned().unwrap_or_default(),
                scroll: 0,
            });
            if handled {
                return Action::None;
            }
        }
        if bound("newline") {
            app.insert_input_newline();
            app.quit_armed_since = None;
            return Action::None;
        }
        if crate::handle_transcript_key(&key, app) {
            crate::copy_pending_selection(app).await;
            return Action::None;
        }
        if bound("interrupt") {
            if app.turn_running {
                self.abort();
                return Action::None;
            }
            if !app.input.is_empty() || !app.input_media.is_empty() {
                // As idle locally: ctrl+c clears a draft, ctrl+d keeps it.
                if name.as_deref() != Some("ctrl+d") {
                    app.clear_input();
                }
                app.quit_armed_since = None;
                return Action::None;
            }
            return if app.quit_requested(Instant::now()) {
                Action::Quit
            } else {
                Action::None
            };
        }
        app.quit_armed_since = None;
        if bound("paste-image") {
            crate::paste_clipboard_image(app, config).await;
            return Action::None;
        }
        if bound("expand") {
            app.previews_expanded = !app.previews_expanded;
            return Action::None;
        }
        if bound("todos") {
            app.todos_expanded = !app.todos_expanded;
            return Action::None;
        }
        if bound("editor") {
            crate::edit_input_in_external_editor(app);
            return Action::None;
        }
        if bound("line-start") {
            app.cursor_to_line_start();
            return Action::None;
        }
        if bound("line-end") {
            app.cursor_to_line_end();
            return Action::None;
        }
        if bound("cursor-left") {
            app.move_cursor_left();
            return Action::None;
        }
        if bound("cursor-right") {
            app.move_cursor_right();
            return Action::None;
        }
        if bound("steer") {
            return self.enter(app, config, true).await;
        }
        match key.code {
            KeyCode::Esc if app.turn_running => self.abort(),
            KeyCode::Esc => {
                app.clear_input();
                app.close_overlay(|o| matches!(o, crate::Overlay::Multiline(_)));
            }
            KeyCode::Backspace => app.pop_input_char(),
            KeyCode::Delete => app.delete_input_char(),
            KeyCode::Up if app.input.is_empty() && !app.queued_messages.is_empty() => {
                self.withdraw_last()
            }
            KeyCode::Up => app.history_previous(),
            KeyCode::Down => app.history_next(),
            KeyCode::Enter => return self.enter(app, config, false).await,
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.push_input_char(ch)
            }
            _ => {}
        }
        Action::None
    }

    /// Sends the draft: a command, or a submission. While a run is active a
    /// submission is a follow-up, or with the steer key a steer.
    async fn enter(&mut self, app: &mut TuiApp, config: &mut LoadedConfig, steer: bool) -> Action {
        // As locally: a draft with images waits while a run is active (they
        // are not queued), and is sent once idle.
        if app.turn_running && !app.input_media.is_empty() {
            return Action::None;
        }
        let line = app.input.trim().to_owned();
        let media = std::mem::take(&mut app.input_media);
        app.clear_input();
        app.input_media = media;
        // As locally: an image alone has a default prompt.
        let line = if line.is_empty() && !app.input_media.is_empty() {
            "Describe this image.".to_owned()
        } else {
            line
        };
        if line.is_empty() {
            return Action::None;
        }
        if let Some(lines) = app.multiline_mut() {
            if line == "." {
                let prompt = lines.join("\n");
                app.close_overlay(|o| matches!(o, crate::Overlay::Multiline(_)));
                self.prompt(app, prompt, steer);
            } else {
                lines.push(line);
            }
            return Action::None;
        }
        if line.starts_with('/') || line.starts_with('!') {
            return self.command(&line, app, config, steer).await;
        }
        self.prompt(app, line, steer);
        Action::None
    }

    fn prompt(&mut self, app: &mut TuiApp, text: String, steer: bool) {
        app.editor_state.record_history(text.clone());
        let when_busy = match (app.turn_running, steer) {
            (false, _) => WhenBusy::Reject,
            (true, true) => WhenBusy::Steer,
            (true, false) => WhenBusy::FollowUp,
        };
        let images = std::mem::take(&mut app.input_media)
            .into_iter()
            .map(|media| ImageArg {
                data: media.data_base64,
                mime_type: media.mime_type,
            })
            .collect();
        self.submit(text, images, when_busy);
    }

    /// A slash command. What has a meaning for the session is a call; what
    /// acts on this process's own session is not available; anything else
    /// is a prompt, which the worker expands as `noel` would (a skill, a
    /// template, a tool command such as `/bash`).
    async fn command(
        &mut self,
        line: &str,
        app: &mut TuiApp,
        config: &mut LoadedConfig,
        steer: bool,
    ) -> Action {
        let name = line.split_whitespace().next().unwrap_or(line);
        match name {
            "/quit" | "/exit" | "/q" => return Action::Quit,
            "/clear" => return Action::Clear,
            "/help" => app.push(
                TuiEntryKind::System,
                format!(
                    "{}\n\nattached to session {}: commands run on the session's worker",
                    crate::terminal_renderer(config).help(),
                    self.session
                ),
            ),
            "/hotkeys" => app.push(TuiEntryKind::System, crate::format_hotkeys(config)),
            "/history" => app.push(
                TuiEntryKind::System,
                crate::format_history(&app.editor_state),
            ),
            "/status" => app.push(TuiEntryKind::System, format!("status\t{}", app.status)),
            "/app" => app.push(TuiEntryKind::System, crate::open_desktop_app(&self.session)),
            // The theme is this terminal's.
            "/theme" if line != "/theme" => {
                let name = line.trim_start_matches("/theme").trim();
                match crate::persist_theme(config, name) {
                    Ok(name) => app.push(TuiEntryKind::System, format!("theme: {name}")),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
            "/accent" if line != "/accent" => {
                let color = line.trim_start_matches("/accent").trim();
                match crate::persist_accent(config, color) {
                    Ok(()) => app.push(TuiEntryKind::System, format!("accent: {color}")),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
            "/diff" if app.diff_panel().is_some() => {
                app.close_overlay(|o| matches!(o, crate::Overlay::DiffPanel(_)))
            }
            "/changelog" => app.push(TuiEntryKind::System, crate::format_changelog()),
            "/themes" => app.push(TuiEntryKind::System, crate::format_themes(config)),
            "/theme" => self.theme_selector(app, config),
            "/copy" if app.transcript_view.borrow().has_selection() => {
                app.transcript_view.get_mut().request_copy();
            }
            "/copy" => self.copy_last_reply(app).await,
            // The external editor is this terminal's.
            "/editor" => {
                let initial = line.trim_start_matches("/editor").trim();
                match crate::read_external_editor_prompt(initial) {
                    Ok(prompt) if !prompt.trim().is_empty() => self.prompt(app, prompt, steer),
                    Ok(_) => app.push(TuiEntryKind::System, "editor returned an empty prompt"),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
            "/multiline" => {
                app.overlay = crate::Overlay::Multiline(Vec::new());
                app.push(
                    TuiEntryKind::System,
                    "enter multiline prompt; submit . on its own line",
                );
            }
            // While a run is active, as the in-process TUI does mid-turn.
            "/queue" if app.turn_running && line == "/queue" => self.list_pending(app),
            "/queue" if app.turn_running => {
                let text = line.trim_start_matches("/queue").trim();
                if !text.is_empty() {
                    self.submit(text.to_owned(), Vec::new(), WhenBusy::FollowUp);
                }
            }
            "/todos" if app.turn_running => app.todos_expanded = !app.todos_expanded,
            "/queue-clear" if app.turn_running => self.clear_pending(app),
            "/interrupt" if app.turn_running => self.abort(),
            "!!" if app.last_shell_command.is_none() => {
                app.push(TuiEntryKind::System, "no previous shell command")
            }
            "!" if line == "!" => app.push(TuiEntryKind::System, "usage: ! <command>"),
            _ if name.starts_with('!') => self.shell(app, line, steer),
            "/image" if line == "/image" => {
                app.push(TuiEntryKind::System, "usage: /image <path> [prompt]")
            }
            "/image" => self.attach_image(app, config, line, steer),
            // While a run is active a prompt-like command queues as a
            // prompt; otherwise the worker runs it as `noel` would (an
            // extension's command, a skill, a template, a tool command).
            _ if app.turn_running && is_prompt_command(line, name) => {
                self.prompt(app, line.to_owned(), steer)
            }
            _ if app.turn_running => app.push(
                TuiEntryKind::System,
                format!("{line} is not available while a turn is running"),
            ),
            // Managing sessions is the server's (`noel.sessions`): a fork, a
            // clone, an import, deleting another session. Deleting the
            // session shown stays the worker's, which opens a new one.
            _ if matches!(name, "/fork" | "/clone")
                || (matches!(name, "/import" | "/delete") && line.trim() != name) =>
            {
                self.sessions_command(line.to_owned());
            }
            // `/bug` takes the error rows shown here into its report.
            _ if line.split_whitespace().next() == Some("/bug") => self.bug(app, line),
            _ => {
                self.run_command(line.to_owned());
            }
        }
        Action::None
    }

    /// `/theme` alone: the theme picker, this terminal's.
    fn theme_selector(&self, app: &mut TuiApp, config: &LoadedConfig) {
        let items = crate::theme_names(config).into_iter().map(|name| {
            let active = config.settings.theme.as_deref().unwrap_or("system") == name
                || (name == "system" && config.settings.theme.as_deref() == Some("default"));
            json!({"label": name, "value": name, "active": active})
        });
        let selector = json!({
            "kind": "theme",
            "title": "theme",
            "items": items.collect::<Vec<_>>(),
        });
        if let Some(selector) = crate::TuiSelectorState::from_json(&selector) {
            app.push(TuiEntryKind::System, format!("{} selector", selector.title));
            app.overlay = crate::Overlay::Selector(selector);
        }
    }

    /// `/copy` without a selection: the last assistant reply.
    async fn copy_last_reply(&self, app: &mut TuiApp) {
        let last = entries(&self.view)
            .iter()
            .rev()
            .map(message_of)
            .find(|message| message["role"] == "assistant")
            .map(|message| blocks(message, "text"));
        match last {
            None => app.push(TuiEntryKind::System, "no assistant message"),
            Some(text) => {
                let copied = crate::clipboard::copy_text(&text).await;
                match copied {
                    Ok(status) => app.push(TuiEntryKind::System, format!("{text}\n{status}")),
                    Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                }
            }
        }
    }

    /// `/queue` during a run: the pending follow-ups.
    fn list_pending(&self, app: &mut TuiApp) {
        let pending = inbox(&self.view);
        if pending.is_empty() {
            app.push(TuiEntryKind::System, "no pending follow-ups");
        } else {
            let list = pending
                .iter()
                .enumerate()
                .map(|(index, (_, _, text))| format!("{}. {text}", index + 1))
                .collect::<Vec<_>>()
                .join("\n");
            app.push(TuiEntryKind::System, format!("pending follow-ups:\n{list}"));
        }
    }

    /// `/queue-clear` during a run: every pending follow-up withdrawn.
    fn clear_pending(&mut self, app: &mut TuiApp) {
        let pending = inbox(&self.view);
        let cleared = pending.len();
        for (id, _, text) in pending {
            self.withdraw(id, text, false);
        }
        app.push(
            TuiEntryKind::System,
            format!("cleared {cleared} pending follow-up(s)"),
        );
    }

    /// `!command` and `!!`: run on the worker, its row filled when it answers.
    fn shell(&mut self, app: &mut TuiApp, line: &str, steer: bool) {
        match crate::resolve_bang_command(line, &app.last_shell_command) {
            Some(command) if !app.turn_running && self.shell_row.is_none() => {
                app.last_shell_command = Some(command.clone());
                app.previews_expanded = false;
                let index =
                    app.push_placeholder(TuiEntryKind::Tool, crate::format_bash_running(&command));
                let pending = self.run_command(format!("!{command}"));
                self.shell_row = Some(ShellRow { index, pending });
            }
            Some(_) => app.push(
                TuiEntryKind::System,
                "! commands wait for the run to settle",
            ),
            None => self.prompt(app, line.to_owned(), steer),
        }
    }

    /// `/image <path> [prompt]`: the image goes with the next prompt.
    fn attach_image(&mut self, app: &mut TuiApp, config: &LoadedConfig, line: &str, steer: bool) {
        let rest = line.trim_start_matches("/image").trim();
        let (path, prompt) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let cwd = std::env::current_dir().unwrap_or_default();
        match crate::load_media_input(&cwd, std::path::Path::new(path), config) {
            Ok(media) => {
                app.push(TuiEntryKind::System, crate::format_media_fallback(&media));
                app.input_media.push(media);
                if !prompt.trim().is_empty() {
                    self.prompt(app, prompt.trim().to_owned(), steer);
                }
            }
            Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
        }
    }

    /// `/bug`: the error rows shown here go into the report.
    fn bug(&mut self, app: &mut TuiApp, line: &str) {
        let errors = app
            .entries
            .iter()
            .filter(|entry| entry.kind == TuiEntryKind::Error)
            .map(|entry| entry.text.clone())
            .collect::<Vec<_>>();
        let pending = self.pend();
        let args = to_args(&CommandArgs {
            line: line.to_owned(),
            request_id: Some(new_request_id()),
            errors: Some(errors),
        });
        self.call(Call::Command(line.to_owned(), pending), args);
    }

    /// Runs `line` on the session's worker; the TUI shows it working until
    /// the answer, as the in-process TUI blocks on a command.
    fn run_command(&mut self, line: String) -> Pending {
        let pending = self.pend();
        let args = to_args(&CommandArgs {
            line: line.clone(),
            request_id: Some(new_request_id()),
            errors: None,
        });
        self.call(Call::Command(line, pending), args);
        pending
    }

    /// The shell row, if `pending` is the command it shows.
    fn take_shell_row(&mut self, pending: Pending) -> Option<usize> {
        let row = self.shell_row.filter(|row| row.pending == pending)?;
        self.shell_row = None;
        Some(row.index)
    }

    /// Syncs the transcript, keeping the shell row's index right and its
    /// placeholder live: the sync removes and re-indexes entries (#146).
    fn sync_transcript(&mut self, app: &mut TuiApp, dirty: usize) {
        let removed = self.transcript.sync(app, &self.view, dirty);
        if let Some(row) = &mut self.shell_row {
            if removed.contains(&row.index) {
                self.shell_row = None;
            } else {
                row.index -= removed.iter().filter(|index| **index < row.index).count();
                app.live_entry_index = Some(row.index);
            }
        }
    }

    /// A new pending call: "Working..." until its answer.
    fn pend(&mut self) -> Pending {
        self.next_pending += 1;
        self.pending.insert(self.next_pending);
        self.next_pending
    }

    /// A key while a selector is open: moving and filtering here, a pick on
    /// the worker (`/select <kind> <value>`). The model picker's provider
    /// layer opens that provider's models.
    fn selector_key(&mut self, key: KeyEvent, app: &mut TuiApp, config: &mut LoadedConfig) {
        match key.code {
            KeyCode::Esc => {
                let mut selector = app.take_selector().expect("selector");
                if let Some(parent) = selector.parent.take() {
                    app.overlay = crate::Overlay::Selector(*parent);
                }
            }
            KeyCode::Up => app.selector_mut().expect("selector").move_selection(-1),
            KeyCode::Down => app.selector_mut().expect("selector").move_selection(1),
            KeyCode::PageUp => app
                .selector_mut()
                .expect("selector")
                .move_selection(-(crate::SELECTOR_PAGE_STEP as isize)),
            KeyCode::PageDown => app
                .selector_mut()
                .expect("selector")
                .move_selection(crate::SELECTOR_PAGE_STEP as isize),
            KeyCode::Home => app.selector_mut().expect("selector").jump_to_start(),
            KeyCode::End => app.selector_mut().expect("selector").jump_to_end(),
            KeyCode::Backspace => app.selector_mut().expect("selector").pop_query_char(),
            KeyCode::Enter => {
                let selector = app.take_selector().expect("selector");
                let Some(item) = selector.selected_item().cloned() else {
                    app.push(TuiEntryKind::System, "no selector match");
                    // As locally: only the settings list stays open.
                    if selector.kind == "settings" {
                        app.overlay = crate::Overlay::Selector(selector);
                    }
                    return;
                };
                // A settings pick keeps the list open; the worker's answer
                // redraws it, unless it was closed meanwhile.
                if selector.kind == "settings" {
                    self.run_command(format!("/select settings {}", item.value));
                    app.overlay = crate::Overlay::Selector(selector);
                    return;
                }
                if selector.kind == "provider" {
                    let models = self.provider_layers.get(&item.value).and_then(|items| {
                        crate::TuiSelectorState::from_json(&json!({
                            "kind": "model",
                            "title": format!("model · {}", item.value),
                            "items": items,
                        }))
                    });
                    match models {
                        Some(mut models) if !models.items.is_empty() => {
                            models.parent = Some(Box::new(selector));
                            app.overlay = crate::Overlay::Selector(models);
                        }
                        _ => app.push(
                            TuiEntryKind::System,
                            format!("no models for {}", item.value),
                        ),
                    }
                } else if matches!(selector.kind.as_str(), "theme" | "themes") {
                    match crate::persist_theme(config, &item.value) {
                        Ok(name) => app.push(TuiEntryKind::System, format!("theme: {name}")),
                        Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
                    }
                } else {
                    self.run_command(format!("/select {} {}", selector.kind, item.value));
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.selector_mut().expect("selector").push_query_char(ch)
            }
            _ => {}
        }
    }

    // ─── calls ──────────────────────────────────────────────────────────────

    /// Submits `text` with a new `requestId`, sent again with that id while
    /// it fails `unavailable`. A prompt that meets a run another client
    /// started meanwhile is queued after it, as Enter queues during a run.
    fn submit(&mut self, text: String, images: Vec<ImageArg>, when_busy: WhenBusy) {
        // Every submission is pending until its own answer: a steer or a
        // follow-up no longer takes another call's count (#133).
        let pending = self.pend();
        let (client, session, sender) = (
            self.client.clone(),
            self.session.clone(),
            self.sender.clone(),
        );
        let generation = self.generation;
        let previous = self.last_submit.take();
        let calling = self.local.calling();
        self.last_submit = Some(tokio::spawn(async move {
            let _calling = calling;
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let send = |when_busy: WhenBusy| {
                let args =
                    SubmitArgs::input(new_request_id(), text.clone(), when_busy, images.clone());
                client.chat_call(&session, "submit", to_args(&args), true)
            };
            let mut result = send(when_busy).await;
            if when_busy == WhenBusy::Reject && refused_busy(&result) {
                result = send(WhenBusy::FollowUp).await;
            }
            let _ = sender.send(Incoming::Answer(
                Call::Submit(text, pending),
                result,
                generation,
            ));
        }));
    }

    /// `/fork [session]`, `/clone [session]`, `/import <path>` or
    /// `/delete <session>` through `noel.sessions`. A session is named as the
    /// picker names it: its id, a prefix of it, or its name.
    fn sessions_command(&mut self, line: String) {
        let pending = self.pend();
        let (client, session, sender) = (
            self.client.clone(),
            self.session.clone(),
            self.sender.clone(),
        );
        let generation = self.generation;
        let cwd = self.view["conversation"]["cwd"]
            .as_str()
            .unwrap_or("/")
            .to_owned();
        tokio::spawn(async move {
            let result = sessions_request(&client, &session, &cwd, &line).await;
            let _ = sender.send(Incoming::Answer(
                Call::Sessions(line, pending),
                result,
                generation,
            ));
        });
    }

    /// Continues the turn the saved context left unfinished: Durable's
    /// `continue` submission, which places what is queued first.
    fn continue_turn(&mut self) {
        let pending = self.pend();
        let args = to_args(&SubmitArgs::continuing(new_request_id()));
        self.call(Call::Submit(String::new(), pending), args);
    }

    fn call(&mut self, call: Call, args: Value) {
        let method = match &call {
            Call::Submit(..) => "submit",
            Call::Command(..) => "command",
            Call::Withdraw { .. } => "withdraw",
            Call::Abort => "abort",
            Call::Sessions(..) => unreachable!("sent by sessions_command"),
        };
        let (client, session, sender) = (
            self.client.clone(),
            self.session.clone(),
            self.sender.clone(),
        );
        let generation = self.generation;
        // Commands keep their order with submissions (`/queue x`, then a
        // prompt); the rest go at once.
        let previous = match &call {
            Call::Command(..) => self.last_submit.take(),
            _ => None,
        };
        let ordered = matches!(&call, Call::Command(..));
        let calling = self.local.calling();
        let task = tokio::spawn(async move {
            let _calling = calling;
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let result = client.chat_call(&session, method, args, false).await;
            let _ = sender.send(Incoming::Answer(call, result, generation));
        });
        if ordered {
            self.last_submit = Some(task);
        }
    }

    /// Aborts the active run, once.
    fn abort(&mut self) {
        let RunView::Running {
            run_id, aborting, ..
        } = &mut self.run
        else {
            return;
        };
        if *aborting {
            return;
        }
        *aborting = true;
        let run_id = run_id.clone();
        self.interrupted = Some(run_id.clone());
        let args = to_args(&AbortArgs {
            expected_run_id: Some(run_id),
            keep_queue: true,
        });
        self.call(Call::Abort, args);
    }

    /// Withdraws the last queued submission, to edit it.
    fn withdraw_last(&mut self) {
        let last = inbox(&self.view)
            .into_iter()
            .rev()
            .find(|(id, _, _)| !self.withdrawing.contains(id));
        if let Some((id, _, text)) = last {
            self.withdraw(id, text, true);
        }
    }

    fn withdraw(&mut self, id: String, text: String, recall: bool) {
        if self.withdrawing.contains(&id) {
            return;
        }
        self.withdrawing.push(id.clone());
        let args = to_args(&WithdrawArgs {
            submission_id: id.clone(),
        });
        self.call(Call::Withdraw { id, text, recall }, args);
    }

    fn answer(&mut self, app: &mut TuiApp, call: Call, result: Result<Value>) {
        if let Call::Submit(_, pending) | Call::Command(_, pending) | Call::Sessions(_, pending) =
            &call
        {
            self.pending.remove(pending);
            let started = result.as_ref().ok().and_then(started_run);
            if let Some(run_id) = started {
                if self.run.running() != Some(run_id.as_str()) {
                    self.run = RunView::Starting {
                        run_id,
                        since: Instant::now(),
                    };
                }
            }
        }
        if let Call::Withdraw { id, .. } = &call {
            self.withdrawing.retain(|withdrawing| withdrawing != id);
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                let message = match &call {
                    Call::Submit(text, _) => format!("not sent: {} ({error})", one_line(text)),
                    Call::Abort => {
                        if let RunView::Running { aborting, .. } = &mut self.run {
                            *aborting = false;
                        }
                        self.interrupted = None;
                        format!("abort: {error}")
                    }
                    Call::Sessions(line, _) => format!("{}: {error:#}", one_line(line)),
                    Call::Withdraw { text, .. } => {
                        format!("withdraw {}: {error}", one_line(text))
                    }
                    Call::Command(line, pending) => {
                        if let Some(row) = self.take_shell_row(*pending) {
                            app.replace_entry(row, format!("failed bash\n{error}"));
                            app.finish_live_entry();
                            return;
                        }
                        format!("{}: {error}", one_line(line))
                    }
                };
                app.push(TuiEntryKind::Error, message);
                return;
            }
        };
        match call {
            Call::Submit(..) => {}
            // The worker's: deleting the session shown.
            Call::Sessions(line, _) if value.is_null() => {
                self.run_command(line);
            }
            Call::Sessions(line, _) if line.starts_with("/delete") => app.push(
                TuiEntryKind::System,
                format!(
                    "deleted {}",
                    value["sessionId"].as_str().unwrap_or_default()
                ),
            ),
            Call::Sessions(..) => {
                self.switch_to = value["sessionId"].as_str().map(str::to_owned);
            }
            Call::Command(line, pending) => {
                let shell_row = self.take_shell_row(pending);
                self.command_answer(app, &line, &value, shell_row)
            }
            Call::Withdraw { text, recall, .. } => match value["outcome"].as_str() {
                Some("withdrawn") if recall && app.input.is_empty() => {
                    crate::recall_queued_message(app, text)
                }
                Some("withdrawn") => app.push(TuiEntryKind::System, format!("withdrew: {text}")),
                Some("already_placed") => app.push(
                    TuiEntryKind::System,
                    format!("already placed into the run: {text}"),
                ),
                _ => app.push(TuiEntryKind::System, format!("already answered: {text}")),
            },
            Call::Abort => {
                let withdrawn = value["withdrawn"].as_u64().unwrap_or(0);
                if withdrawn > 0 {
                    app.push(
                        TuiEntryKind::System,
                        format!("withdrew {withdrawn} queued submission(s)"),
                    );
                }
            }
        }
    }
}

impl Remote {
    /// What the worker made of a command: its rows, then what to do next.
    fn command_answer(
        &mut self,
        app: &mut TuiApp,
        line: &str,
        value: &Value,
        shell_row: Option<usize>,
    ) {
        let mut entries = value["entries"].as_array().cloned().unwrap_or_default();
        // A `!` command's result fills the row that showed it running.
        if let Some(row) = shell_row {
            let text = if entries.is_empty() {
                String::new()
            } else {
                let first = entries.remove(0);
                first["text"].as_str().unwrap_or_default().to_owned()
            };
            app.replace_entry(row, text);
            app.finish_live_entry();
        }
        for entry in &entries {
            let kind = match entry["kind"].as_str() {
                Some("error") => TuiEntryKind::Error,
                Some("tool") => TuiEntryKind::Tool,
                _ => TuiEntryKind::System,
            };
            app.push(kind, entry["text"].as_str().unwrap_or_default());
        }
        let next = &value["next"];
        match next["type"].as_str() {
            Some("session") => {
                if let Some(session) = next["session"].as_str() {
                    if session == self.session {
                        // The session open here: its details, as locally.
                        self.run_command("/session".to_owned());
                    } else {
                        self.switch_to = Some(session.to_owned());
                    }
                }
            }
            Some("selector") => match crate::TuiSelectorState::from_json(&next["selector"]) {
                // A settings pick's list again, after the list was closed.
                Some(_)
                    if line.starts_with("/select settings ")
                        && app.selector().is_none_or(|open| open.kind != "settings") => {}
                Some(selector) => {
                    if let Some(layers) = next["selector"]["layers"].as_object() {
                        self.provider_layers = layers
                            .iter()
                            .map(|(provider, items)| (provider.clone(), items.clone()))
                            .collect();
                    }
                    app.overlay = crate::Overlay::Selector(selector);
                }
                None => app.push(
                    TuiEntryKind::Error,
                    "the worker sent an unreadable selector",
                ),
            },
            Some("diff") => {
                let mut files = Vec::new();
                self.diff_lines.clear();
                for file in next["files"].as_array().into_iter().flatten() {
                    let path = file["path"].as_str().unwrap_or_default().to_owned();
                    let lines = file["lines"].as_array().into_iter().flatten();
                    let lines = lines.filter_map(Value::as_str).map(str::to_owned).collect();
                    self.diff_lines.insert(path.clone(), lines);
                    files.push(crate::DiffFileEntry {
                        path,
                        added: file["added"].as_u64(),
                        removed: file["removed"].as_u64(),
                        untracked: file["untracked"] == true,
                    });
                }
                app.overlay = crate::Overlay::DiffPanel(crate::DiffPanelState {
                    files,
                    selected: 0,
                    detail: None,
                });
            }
            Some("draft") if app.input.is_empty() => {
                crate::recall_queued_message(
                    app,
                    next["text"].as_str().unwrap_or_default().to_owned(),
                );
                // Its images come back to the draft too, as locally.
                app.input_media = serde_json::from_value(next["media"].clone()).unwrap_or_default();
            }
            // Run once the loop has the terminal: here on this machine, or
            // relayed from the server's (`noel.accounts`).
            Some("login") => {
                self.login = Some((
                    next["provider"].as_str().unwrap_or_default().to_owned(),
                    next["account"].as_str().unwrap_or("default").to_owned(),
                ));
            }
            _ => {}
        }
    }
}

/// A refusal because a run is active: the plugin error `busy`.
fn refused_busy(result: &Result<Value>) -> bool {
    result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<WireError>())
        .is_some_and(|error| error.is_plugin("busy"))
}

/// Whether a slash command goes to the session as a prompt: a tool command
/// (`/bash`, `/read`, …), a skill, or anything that is not one of the TUI's
/// own commands (a template, or text for the model), as the local TUI
/// treats them.
fn is_prompt_command(line: &str, name: &str) -> bool {
    if name.starts_with('!') {
        return false;
    }
    if crate::response_kind_for_prompt(line) == TuiEntryKind::Tool || name.starts_with("/skill:") {
        return true;
    }
    let builtin = name.starts_with("/extension:")
        || noel_tui::COMMAND_HELP
            .iter()
            .any(|help| help.command.split_whitespace().next() == Some(name));
    !builtin
}

fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 60 || text.contains('\n') {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line.to_owned()
    }
}

// ─── the view ───────────────────────────────────────────────────────────────

fn entries(view: &Value) -> &[Value] {
    view["entries"].as_array().map_or(&[], Vec::as_slice)
}

/// The message of a history entry (`model[0]`), or the entry itself.
fn message_of(entry: &Value) -> &Value {
    match entry.get("model").and_then(|model| model.get(0)) {
        Some(message) => message,
        None => entry,
    }
}

/// `pi.inbox`'s items: `(id, mode, content)`.
fn inbox(view: &Value) -> Vec<(String, String, String)> {
    view["docs"]["pi.inbox"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| {
            let text = |key: &str| item[key].as_str().unwrap_or_default().to_owned();
            (text("id"), text("mode"), text("content"))
        })
        .collect()
}

fn last_run(view: &Value) -> Option<(String, String)> {
    let last = &view["lastRun"];
    Some((
        last["runId"].as_str()?.to_owned(),
        last["status"].as_str()?.to_owned(),
    ))
}

/// What the active run is doing, for the footer's spinner.
fn activity(live: &Value) -> Activity {
    if live["compactions"]
        .as_array()
        .is_some_and(|running| !running.is_empty())
    {
        return Activity::Compacting;
    }
    if let Some(tool) = live["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["status"] == "running")
    {
        return Activity::Tool(tool["name"].as_str().unwrap_or("tool").to_owned());
    }
    let message = &live["generation"]["message"];
    if !blocks(message, "text").is_empty() {
        Activity::Writing
    } else if !blocks(message, "thinking").is_empty() {
        Activity::Thinking
    } else {
        Activity::Waiting
    }
}

/// The first entry two views differ at.
fn first_difference(old: &Value, new: &Value) -> usize {
    entries(old)
        .iter()
        .zip(entries(new))
        .take_while(|(old, new)| old == new)
        .count()
}

/// The text of a message's content blocks of type `kind` (`text` or
/// `thinking`); a string content is its text.
fn blocks(message: &Value, kind: &str) -> String {
    match &message["content"] {
        Value::String(text) if kind == "text" => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == kind)
            .filter_map(|block| block[kind].as_str())
            .collect(),
        _ => String::new(),
    }
}

// ─── the transcript ─────────────────────────────────────────────────────────

/// Where a row of the transcript comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// A message of the view's entries.
    Entry,
    /// The active run's reply, still streaming: its thinking, its text.
    Thinking,
    Text,
    /// A tool the active run runs, or a retry it waits out.
    Tool,
}

/// One entry of the TUI's transcript as the view gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    kind: TuiEntryKind,
    text: String,
    source: Source,
}

/// Adds a row, unless its text is empty: the TUI shows no empty entry.
fn add(rows: &mut Vec<Row>, kind: TuiEntryKind, text: String, source: Source) {
    if !text.trim().is_empty() {
        rows.push(Row { kind, text, source });
    }
}

/// The rows of entry `index`, as the local TUI restores a saved message:
/// a reply's thinking, then its text, then `aborted` if it was; a prompt
/// and its images; a tool's result with its call.
fn entry_rows(entries: &[Value], index: usize) -> Vec<Row> {
    let record = &entries[index];
    let mut rows = Vec::new();
    // Markers of the history: where the model's context was cut or reset.
    match record["kind"].as_str() {
        Some("pi.compaction") => {
            let data = &record["data"];
            add(
                &mut rows,
                TuiEntryKind::System,
                format!(
                    "compacted: {} messages summarized, {} kept",
                    data["omittedMessages"], data["retainedMessages"]
                ),
                Source::Entry,
            );
            return rows;
        }
        Some("pi.reset") => {
            add(
                &mut rows,
                TuiEntryKind::System,
                format!("context reset to {} messages", record["data"]["messages"]),
                Source::Entry,
            );
            return rows;
        }
        _ => {}
    }
    let entry = message_of(record);
    match entry["role"].as_str().unwrap_or_default() {
        "assistant" => {
            add(
                &mut rows,
                TuiEntryKind::Thinking,
                blocks(entry, "thinking"),
                Source::Entry,
            );
            add(
                &mut rows,
                TuiEntryKind::Assistant,
                blocks(entry, "text"),
                Source::Entry,
            );
            if entry["stopReason"] == "aborted" {
                add(
                    &mut rows,
                    TuiEntryKind::System,
                    "aborted".into(),
                    Source::Entry,
                );
            }
        }
        // Placed from the queue: each queued text as the in-process TUI
        // shows it.
        "user" if record["data"]["queued"].is_string() => {
            // As locally: steering is a system row, a follow-up a prompt.
            let (prefix, kind) = if record["data"]["queued"] == "steer" {
                ("steering>", TuiEntryKind::System)
            } else {
                ("queued>", TuiEntryKind::User)
            };
            for text in blocks(entry, "text").split(noel_core::STEERING_JOIN) {
                add(
                    &mut rows,
                    kind.clone(),
                    format!("{prefix} {text}"),
                    Source::Entry,
                );
            }
        }
        "user" => {
            add(
                &mut rows,
                TuiEntryKind::User,
                blocks(entry, "text"),
                Source::Entry,
            );
            for image in entry["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "image")
            {
                add(
                    &mut rows,
                    TuiEntryKind::System,
                    format!(
                        "image: -\t{}\tunknown-size\tterminal display fallback: attached to provider message",
                        image["mimeType"].as_str().unwrap_or_default()
                    ),
                    Source::Entry,
                );
            }
        }
        "toolResult" => {
            let name = entry["toolName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .unwrap_or("tool");
            add(
                &mut rows,
                TuiEntryKind::Tool,
                format!(
                    "{} {name}\n{}\n{}",
                    if entry["isError"] == true {
                        "failed"
                    } else {
                        "completed"
                    },
                    call_detail(&entries[..index], entry["toolCallId"].as_str()),
                    blocks(entry, "text")
                ),
                Source::Entry,
            );
        }
        _ => add(
            &mut rows,
            TuiEntryKind::System,
            blocks(entry, "text"),
            Source::Entry,
        ),
    }
    rows
}

/// What the active run streams (`pi.live`): its reply's thinking and text
/// so far, a retry it waits out, and each tool that runs with its output,
/// as the local TUI shows them during a turn.
fn live_rows(live: &Value, entries: &[Value]) -> Vec<Row> {
    let mut rows = Vec::new();
    let generation = &live["generation"];
    let message = &generation["message"];
    add(
        &mut rows,
        TuiEntryKind::Thinking,
        blocks(message, "thinking"),
        Source::Thinking,
    );
    add(
        &mut rows,
        TuiEntryKind::Assistant,
        blocks(message, "text"),
        Source::Text,
    );
    for tool in live["tools"].as_array().into_iter().flatten() {
        let verb = match tool["status"].as_str() {
            Some("running") => "running",
            Some("interrupted") => "interrupted",
            // Pending calls show once they run; a done call's result is an
            // entry.
            _ => continue,
        };
        add(
            &mut rows,
            TuiEntryKind::Tool,
            format!(
                "{verb} {}\n{}\n{}",
                tool["name"].as_str().unwrap_or("tool"),
                call_detail(entries, tool["callId"].as_str()),
                tool["output"].as_str().unwrap_or_default()
            ),
            Source::Tool,
        );
    }
    rows
}

/// What the local TUI shows of call `id` (a command, a path), from the
/// replies in `entries`.
fn call_detail(entries: &[Value], id: Option<&str>) -> String {
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        return String::new();
    };
    entries
        .iter()
        .rev()
        .map(message_of)
        .filter(|entry| entry["role"] == "assistant")
        .flat_map(|entry| entry["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "toolCall" && block["id"] == id)
        .map(|block| {
            crate::tool_call_detail(&noel_ai::ChatToolCall {
                id: id.to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                arguments: block["arguments"].to_string(),
            })
        })
        .unwrap_or_default()
}

/// The rows shown for the view, and where in the TUI's entries. Notes the
/// TUI adds itself sit between them and are left alone.
#[derive(Default)]
struct Transcript {
    /// The committed entries' rows, then the live ones, each with its index
    /// in the TUI's entries.
    rows: Vec<(Row, usize)>,
    /// Where each committed entry's rows start in `rows`.
    starts: Vec<usize>,
    /// How many of `rows` are committed entries'.
    committed: usize,
    /// Entries before this one are not shown (`/clear`).
    hidden: usize,
}

impl Transcript {
    /// Shows `view`, whose entries from `dirty` on may differ from what is
    /// shown. Rows that change are changed in place: a text that grows is
    /// appended to, so a streaming reply stays one entry as it becomes the
    /// saved reply.
    /// Returns the indexes of the entries it removed, as they were.
    fn sync(&mut self, app: &mut TuiApp, view: &Value, dirty: usize) -> Vec<usize> {
        let entries = entries(view);
        let dirty = dirty.min(self.starts.len());
        let from = self.starts.get(dirty).copied().unwrap_or(self.committed);
        self.starts.truncate(dirty);
        let mut fresh = Vec::new();
        for index in dirty..entries.len() {
            self.starts.push(from + fresh.len());
            if index >= self.hidden {
                fresh.extend(entry_rows(entries, index));
            }
        }
        self.committed = from + fresh.len();
        fresh.extend(live_rows(&view["docs"]["pi.live"], entries));

        let old = self.rows.split_off(from);
        let kept = fresh.len();
        for (offset, row) in fresh.into_iter().enumerate() {
            let index = match old.get(offset) {
                Some((shown, index)) => {
                    show(app, *index, shown, &row);
                    *index
                }
                None => {
                    app.push(row.kind.clone(), row.text.clone());
                    app.entries.len() - 1
                }
            };
            self.rows.push((row, index));
        }
        let mut gone = old
            .iter()
            .skip(kept)
            .map(|(_, index)| *index)
            .collect::<Vec<_>>();
        gone.sort_unstable();
        for index in gone.iter().rev() {
            app.remove_entry(*index);
            for (_, shown) in &mut self.rows {
                if *shown > *index {
                    *shown -= 1;
                }
            }
        }

        // The live rows are still being written: the inline view must not
        // archive them, and a thinking preview follows its tail.
        app.live_entry_index = None;
        app.live_thinking_index = None;
        app.live_tool_indices.clear();
        for (row, index) in &self.rows[self.committed..] {
            match row.source {
                Source::Thinking => app.live_thinking_index = Some(*index),
                Source::Text => app.live_entry_index = Some(*index),
                Source::Tool => app.live_tool_indices.push(*index),
                Source::Entry => {}
            }
        }
        gone
    }
}

/// Changes the entry at `index`, shown as `shown`, to `row`.
fn show(app: &mut TuiApp, index: usize, shown: &Row, row: &Row) {
    if shown.kind == row.kind {
        if shown.text == row.text {
            return;
        }
        if let Some(rest) = row.text.strip_prefix(shown.text.as_str()) {
            app.append_entry(index, rest);
            return;
        }
        app.replace_entry(index, row.text.clone());
        return;
    }
    app.entries[index] = TuiEntry {
        kind: row.kind.clone(),
        text: row.text.clone(),
    };
    app.transcript_view.get_mut().replaced(index);
    if let Some(progress) = app.transcript_scrollback.entries.get_mut(index) {
        *progress = crate::EntryScrollback::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #133: the run's states and how the view moves them.
    #[test]
    fn the_run_view_follows_the_live_run_and_gives_up_on_an_unseen_start() {
        let starting = RunView::Starting {
            run_id: "r1".into(),
            since: Instant::now(),
        };
        // Not shown yet: still starting.
        let still = starting.clone().advance(None, false, None);
        assert!(matches!(still, RunView::Starting { .. }));
        // It ended before the view showed it running: idle.
        assert_eq!(
            starting.clone().advance(None, false, Some("r1")),
            RunView::Idle
        );
        // Never shown: idle after the timeout, not "Working..." forever.
        let stale = RunView::Starting {
            run_id: "r1".into(),
            since: Instant::now() - STARTING_TIMEOUT,
        };
        assert_eq!(stale.advance(None, false, None), RunView::Idle);
        // Shown: running; an abort sent stays marked until the run ends.
        let running = starting.advance(Some("r1"), false, None);
        let RunView::Running { since, .. } = running else {
            panic!("running")
        };
        let aborting = RunView::Running {
            run_id: "r1".into(),
            since,
            saw_tools: false,
            aborting: true,
        };
        assert_eq!(
            aborting.clone().advance(Some("r1"), true, None),
            RunView::Running {
                run_id: "r1".into(),
                since,
                saw_tools: true,
                aborting: true,
            }
        );
        // Another run: its own state, not the aborted one's.
        assert!(matches!(
            aborting.clone().advance(Some("r2"), false, None),
            RunView::Running {
                aborting: false,
                ..
            }
        ));
        assert_eq!(aborting.advance(None, false, Some("r1")), RunView::Idle);
    }

    fn view(entries: Value, live: Value) -> Value {
        json!({
            "conversation": {"id": "s1", "cwd": "/w"},
            "entries": entries,
            "docs": {"pi.live": live, "pi.inbox": {"items": []}},
        })
    }

    fn shown(app: &TuiApp) -> Vec<(TuiEntryKind, &str)> {
        app.entries
            .iter()
            .map(|entry| (entry.kind.clone(), entry.text.as_str()))
            .collect()
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn entries_map_to_rows_as_saved_messages_restore() {
        let entries = [
            user("run ls"),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "list it"},
                {"type": "text", "text": "Listing."},
                {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}},
            ], "stopReason": "toolUse"}),
            json!({"role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": [{"type": "text", "text": "a.txt"}], "isError": false}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Half"}], "stopReason": "aborted", "errorMessage": "Request was aborted"}),
            json!({"role": "toolResult", "toolCallId": "", "toolName": "read", "content": [{"type": "text", "text": "missing"}], "isError": true}),
        ];
        let rows = (0..entries.len())
            .flat_map(|index| entry_rows(&entries, index))
            .map(|row| (row.kind, row.text))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (TuiEntryKind::User, "run ls".to_owned()),
                (TuiEntryKind::Thinking, "list it".to_owned()),
                (TuiEntryKind::Assistant, "Listing.".to_owned()),
                (TuiEntryKind::Tool, "completed bash\n$ ls\na.txt".to_owned()),
                (TuiEntryKind::Assistant, "Half".to_owned()),
                (TuiEntryKind::System, "aborted".to_owned()),
                (TuiEntryKind::Tool, "failed read\n\nmissing".to_owned()),
            ]
        );
    }

    #[test]
    fn live_rows_show_the_partial_and_running_tools() {
        // A retry is a note (`note_retry`), not a live row.
        let entries = [json!({"role": "assistant", "content": [
            {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "make"}},
            {"type": "toolCall", "id": "c2", "name": "read", "arguments": {"path": "x"}},
        ]})];
        let live = json!({
            "run": {"runId": "r1", "inputs": ["r1"]},
            "generation": {"attempt": 2, "message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "Hel"},
            ]}, "retry": {"at": 1, "error": "overloaded"}},
            "tools": [
                {"callId": "c1", "name": "bash", "status": "running", "output": "built"},
                {"callId": "c2", "name": "read", "status": "pending"},
            ],
        });
        let rows = live_rows(&live, &entries)
            .into_iter()
            .map(|row| (row.kind, row.text, row.source))
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                (TuiEntryKind::Thinking, "hmm".to_owned(), Source::Thinking),
                (TuiEntryKind::Assistant, "Hel".to_owned(), Source::Text),
                (
                    TuiEntryKind::Tool,
                    "running bash\n$ make\nbuilt".to_owned(),
                    Source::Tool
                ),
            ]
        );
        assert_eq!(live_rows(&json!({}), &entries), []);
    }

    #[test]
    fn a_streaming_reply_grows_one_entry_that_becomes_the_saved_reply() {
        let mut app = TuiApp::default();
        app.push(TuiEntryKind::System, "banner");
        let mut transcript = Transcript::default();
        transcript.sync(&mut app, &view(json!([user("hi")]), json!({})), 0);
        assert_eq!(shown(&app)[1], (TuiEntryKind::User, "hi"));

        let partial = |text: &str| json!({"run": {"runId": "r1"}, "generation": {"attempt": 1, "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}}});
        transcript.sync(
            &mut app,
            &view(json!([user("hi")]), partial("Hel")),
            usize::MAX,
        );
        assert_eq!(app.live_entry_index, Some(2));
        // A note the TUI adds stays where it is.
        app.push(TuiEntryKind::System, "a note");
        transcript.sync(
            &mut app,
            &view(json!([user("hi")]), partial("Hello")),
            usize::MAX,
        );
        let reply = json!({"role": "assistant", "content": [{"type": "text", "text": "Hello!"}], "stopReason": "stop"});
        transcript.sync(
            &mut app,
            &view(json!([user("hi"), reply]), json!({"run": {"runId": "r1"}})),
            1,
        );
        assert_eq!(
            shown(&app),
            [
                (TuiEntryKind::System, "banner"),
                (TuiEntryKind::User, "hi"),
                (TuiEntryKind::Assistant, "Hello!"),
                (TuiEntryKind::System, "a note"),
            ]
        );
        assert_eq!(app.live_entry_index, None);
        assert_eq!(transcript.committed, 2);
    }

    #[test]
    fn a_tool_runs_then_its_result_replaces_it_and_a_rewrite_drops_rows() {
        let mut app = TuiApp::default();
        let mut transcript = Transcript::default();
        let call = json!({"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "ls"}}], "stopReason": "toolUse"});
        let entries = json!([user("go"), call]);
        let running = json!({"run": {"runId": "r1"}, "tools": [{"callId": "c1", "name": "bash", "status": "running", "output": "a"}]});
        transcript.sync(&mut app, &view(entries.clone(), running), 0);
        assert_eq!(app.live_tool_indices, [1]);
        let result = json!({"role": "toolResult", "toolCallId": "c1", "toolName": "bash", "content": [{"type": "text", "text": "a\nb"}], "isError": false});
        let done = json!({"run": {"runId": "r1"}, "tools": [{"callId": "c1", "name": "bash", "status": "done"}]});
        transcript.sync(&mut app, &view(json!([user("go"), call, result]), done), 2);
        assert_eq!(
            shown(&app),
            [
                (TuiEntryKind::User, "go"),
                (TuiEntryKind::Tool, "completed bash\n$ ls\na\nb"),
            ]
        );
        assert!(app.live_tool_indices.is_empty());
        // Compaction rewrites the entries: the rows follow, and the sync
        // names the rows it removed, which a row held elsewhere (the shell
        // row, #146) follows.
        let removed = transcript.sync(&mut app, &view(json!([user("summary")]), json!({})), 0);
        assert_eq!(removed, [1]);
        assert_eq!(shown(&app), [(TuiEntryKind::User, "summary")]);
        assert_eq!((transcript.rows.len(), transcript.committed), (1, 1));
    }

    #[test]
    fn views_differ_from_their_first_changed_entry() {
        assert_eq!(
            first_difference(
                &json!({"entries": [1, 2, 3]}),
                &json!({"entries": [1, 2, 4, 5]})
            ),
            2
        );
    }

    #[test]
    fn commands_for_this_process_are_refused_and_the_rest_are_prompts() {
        for (line, prompt) in [
            ("/bash ls", true),
            ("/read a.txt", true),
            ("/skill:review", true),
            ("/fix the bug", true),
            ("/resume", false),
            ("/new", false),
            ("/settings", false),
            ("/extension:assist go", false),
            ("!ls", false),
        ] {
            let name = line.split_whitespace().next().unwrap();
            assert_eq!(is_prompt_command(line, name), prompt, "{line}");
        }
    }
}
