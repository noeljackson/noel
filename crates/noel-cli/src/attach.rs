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

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use noel_chord::delta::{Op, Seg};
use noel_chord::host::StateUpdate;
use noel_chord::{ChordError, ErrorCode};
use noel_config::LoadedConfig;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::server::client::{Client, Endpoint, Followed};
use crate::server::{new_request_id, valid_session_id};
use crate::{
    Activity, TuiApp, TuiEntry, TuiEntryKind, TuiTerminal, QUIT_CONFIRM_WINDOW,
    STREAM_RENDER_INTERVAL_MS,
};

/// How long the first view may take: starting a server and a worker.
const FIRST_VIEW: Duration = Duration::from_secs(30);

pub(crate) async fn run(
    session: &str,
    endpoint: Endpoint,
    config: LoadedConfig,
    owned_screen: bool,
) -> Result<()> {
    if !valid_session_id(session) {
        return Err(anyhow!("--attach: {session:?} is not a session id"));
    }
    let (sender, mut incoming) = mpsc::unbounded_channel();
    let reports = sender.clone();
    let place = match &endpoint {
        Endpoint::Local => "noel server".to_owned(),
        Endpoint::Ssh { destination, .. } => format!("noel server on {destination}"),
    };
    let client = Client::connect(
        endpoint,
        Arc::new(move |message| {
            let _ = reports.send(Incoming::Report(message));
        }),
    )
    .await?;
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

    // The first view comes before the terminal is taken, so an unknown
    // session is an error on the plain terminal.
    let mut early = Vec::new();
    let first = tokio::time::timeout(FIRST_VIEW, async {
        loop {
            tokio::select! {
                message = incoming.recv() => match message {
                    Some(Incoming::Followed(Followed::Update(
                        StateUpdate::Snapshot(view) | StateUpdate::Reset(view),
                    ))) => return Ok(view),
                    Some(Incoming::Followed(Followed::Update(StateUpdate::Error(error)))) => {
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
    let first = match first {
        Ok(view) => view,
        Err(error) => {
            follower.abort();
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
    app.push(
        TuiEntryKind::System,
        format!(
            "{}\nattached to session {session} on {place} · /help for commands, /quit to detach",
            crate::terminal_renderer(&config).banner()
        ),
    );
    if let Err(error) = crate::terminal_theme(&config) {
        app.push(TuiEntryKind::Error, format!("{error}; using system theme"));
    }
    let mut remote = Remote::new(session, client.clone(), sender);
    // What came before the view: starting a server, sending again.
    for message in early {
        remote.receive(&mut app, message);
    }
    remote.first(&mut app, first);
    let result = remote
        .drive(
            &mut app,
            &mut terminal,
            &mut viewport_height,
            &config,
            &mut incoming,
        )
        .await;
    follower.abort();
    crate::close_tui_terminal(terminal, restore, &app)?;
    client.close().await;
    result
}

/// An attach's error, naming an unknown session plainly.
fn attach_error(session: &str, error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<ChordError>() {
        Some(chord)
            if chord.code == ErrorCode::Invalid && chord.reason.as_deref() == Some("session") =>
        {
            anyhow!("no session {session} on noel server (`noel client ls` lists its sessions)")
        }
        _ => anyhow!("could not attach to session {session}: {error}"),
    }
}

/// What reaches the TUI from the server.
enum Incoming {
    /// The view, and what happened to the link or the hold.
    Followed(Followed),
    /// What the client did on its own: started a server, sent a call again.
    Report(String),
    /// A call's answer.
    Answer(Call, Result<Value>),
}

/// The calls the TUI makes.
enum Call {
    /// A submission, with its text.
    Submit(String),
    /// A withdrawal of a queued submission, with its text; `recall` puts the
    /// text back into an empty draft.
    Withdraw {
        id: String,
        text: String,
        recall: bool,
    },
    Abort,
    Configure,
    Compact,
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
    session: String,
    client: Client,
    sender: mpsc::UnboundedSender<Incoming>,
    /// The view as of the last update.
    view: Value,
    transcript: Transcript,
    /// The active run and when this TUI first saw it.
    run: Option<(String, Instant)>,
    /// The active run has run a tool (the queue's hint says so).
    saw_tools: bool,
    /// The run an abort was sent for.
    aborting: Option<String>,
    /// Queued submissions a withdrawal was sent for.
    withdrawing: Vec<String>,
    /// The last run as last shown: `(runId, status)`.
    last_run: Option<(String, String)>,
    /// Why the view is not current: its worker or the server was lost.
    waiting: Option<&'static str>,
}

impl Remote {
    fn new(session: &str, client: Client, sender: mpsc::UnboundedSender<Incoming>) -> Self {
        Self {
            session: session.to_owned(),
            client,
            sender,
            view: Value::Null,
            transcript: Transcript::default(),
            run: None,
            saw_tools: false,
            aborting: None,
            withdrawing: Vec::new(),
            last_run: None,
            waiting: None,
        }
    }

    /// The first view: the transcript, and its prompts as the editor's
    /// history, as reopening a session restores them.
    fn first(&mut self, app: &mut TuiApp, view: Value) {
        for entry in entries(&view) {
            if entry["role"] == "user" {
                app.editor_state.record_history(blocks(entry, "text"));
            }
        }
        self.last_run = last_run(&view);
        self.update(app, StateUpdate::Snapshot(view));
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
            Incoming::Answer(call, result) => self.answer(app, call, result),
        }
    }

    fn update(&mut self, app: &mut TuiApp, update: StateUpdate) {
        let (view, dirty) = match update {
            StateUpdate::Snapshot(view) | StateUpdate::Reset(view) => {
                if self.waiting.take().is_some() {
                    app.push(
                        TuiEntryKind::System,
                        "holding the session again: redrawn from its snapshot",
                    );
                }
                let dirty = first_difference(&self.view, &view);
                (view, dirty)
            }
            StateUpdate::Ops { ops, value } => (value, entries_changed_from(&ops)),
            StateUpdate::Error(error) => {
                app.push(TuiEntryKind::Error, format!("the session's view: {error}"));
                return;
            }
        };
        self.view = view;
        self.transcript.sync(app, &self.view, dirty);
        self.note_last_run(app);
    }

    /// Notes a run that another client's abort or a failure ended; a
    /// completed run, or one the server resumes, says nothing.
    fn note_last_run(&mut self, app: &mut TuiApp) {
        let now = last_run(&self.view);
        if now == self.last_run {
            return;
        }
        self.last_run.clone_from(&now);
        let Some((_, status)) = now else { return };
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
                    .is_none_or(|entry| entry["stopReason"] != "aborted") =>
            {
                app.push(TuiEntryKind::System, "aborted")
            }
            _ => {}
        }
    }

    /// The footer and the panels: the queue from `pi.inbox`, the run and
    /// what it does from `pi.live`, the model from `pi.agent`.
    fn refresh_chrome(&mut self, app: &mut TuiApp, config: &LoadedConfig) {
        let docs = &self.view["docs"];
        let live = &docs["pi.live"];
        app.queued_messages = docs["pi.inbox"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| item["content"].as_str().unwrap_or_default().to_owned())
            .collect();
        match live["run"]["runId"].as_str() {
            Some(run_id) => {
                if self.run.as_ref().map(|(id, _)| id.as_str()) != Some(run_id) {
                    self.run = Some((run_id.to_owned(), Instant::now()));
                    self.saw_tools = false;
                }
                self.saw_tools |= live["tools"]
                    .as_array()
                    .is_some_and(|tools| !tools.is_empty());
            }
            None => {
                self.run = None;
                self.aborting = None;
            }
        }
        app.turn_running = self.run.is_some();
        app.turn_saw_tools = self.saw_tools;
        let queued = app.queued_messages.len();
        app.status = if let Some(why) = self.waiting {
            format!("{why}… · /quit to detach")
        } else if let Some((_, started)) = &self.run {
            let status = crate::activity_status(*started, &activity(live), queued);
            if self.aborting.is_some() {
                status.replace("esc to interrupt", "aborting")
            } else {
                status
            }
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
        config: &LoadedConfig,
        incoming: &mut mpsc::UnboundedReceiver<Incoming>,
    ) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(STREAM_RENDER_INTERVAL_MS * 2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.refresh_chrome(app, config);
        crate::redraw_tui(terminal, app, config)?;
        loop {
            let mut changed = false;
            tokio::select! {
                message = incoming.recv() => {
                    // This holds a sender, so the channel stays open.
                    let Some(message) = message else { return Ok(()) };
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
                                    Action::Quit => return Ok(()),
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
                            Event::Resize(_, _) => crate::resize_tui_viewport(
                                terminal,
                                viewport_height,
                                app.owned_screen,
                            )?,
                            _ => {}
                        }
                    }
                    crate::copy_pending_selection(app).await;
                    // The spinner moves while a run is active.
                    changed |= app.turn_running || app.quit_armed_since.is_some();
                }
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
        self.transcript = Transcript {
            hidden: entries(&self.view).len(),
            ..Transcript::default()
        };
        self.transcript.sync(app, &self.view, 0);
    }

    // ─── keys ───────────────────────────────────────────────────────────────

    async fn key(&mut self, key: KeyEvent, app: &mut TuiApp, config: &LoadedConfig) -> Action {
        let bindings = crate::keybinding_map(config);
        let name = crate::key_event_name(&key);
        let bound = |action: &str| {
            name.as_deref()
                .is_some_and(|name| bindings.matches(action, name))
        };
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
            app.push(
                TuiEntryKind::System,
                "images are not available when attached",
            );
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
            return self.enter(app, config, true);
        }
        match key.code {
            KeyCode::Esc if app.turn_running => self.abort(),
            KeyCode::Esc => {
                app.clear_input();
                app.multiline = None;
            }
            KeyCode::Backspace => app.pop_input_char(),
            KeyCode::Delete => app.delete_input_char(),
            KeyCode::Up if app.input.is_empty() && !app.queued_messages.is_empty() => {
                self.withdraw_last()
            }
            KeyCode::Up => app.history_previous(),
            KeyCode::Down => app.history_next(),
            KeyCode::Enter => return self.enter(app, config, false),
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.push_input_char(ch)
            }
            _ => {}
        }
        Action::None
    }

    /// Sends the draft: a command, or a submission. While a run is active a
    /// submission is a follow-up, or with the steer key a steer.
    fn enter(&mut self, app: &mut TuiApp, config: &LoadedConfig, steer: bool) -> Action {
        let line = app.input.trim().to_owned();
        app.clear_input();
        if line.is_empty() {
            return Action::None;
        }
        if line.starts_with('/') || line.starts_with('!') {
            return self.command(&line, app, config, steer);
        }
        self.prompt(app, line, steer);
        Action::None
    }

    fn prompt(&mut self, app: &mut TuiApp, text: String, steer: bool) {
        app.editor_state.record_history(text.clone());
        let when_busy = match (app.turn_running, steer) {
            (false, _) => "reject",
            (true, true) => "steer",
            (true, false) => "followUp",
        };
        self.submit(text, when_busy);
    }

    /// A slash command. What has a meaning for the session is a call; what
    /// acts on this process's own session is not available; anything else
    /// is a prompt, which the worker expands as `noel` would (a skill, a
    /// template, a tool command such as `/bash`).
    fn command(
        &mut self,
        line: &str,
        app: &mut TuiApp,
        config: &LoadedConfig,
        steer: bool,
    ) -> Action {
        let (name, rest) = match line.split_once(char::is_whitespace) {
            Some((name, rest)) => (name, rest.trim()),
            None => (line, ""),
        };
        match name {
            "/quit" | "/exit" | "/q" => return Action::Quit,
            "/clear" => return Action::Clear,
            "/help" => app.push(
                TuiEntryKind::System,
                format!(
                    "{}\n\nattached to session {}: /model <provider/id>, /thinking <level>, /compact, /queue, /queue-clear, /interrupt and prompts reach the session; commands for this process's own sessions, settings and accounts are not available",
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
            "/session" => app.push(TuiEntryKind::System, self.describe()),
            "/copy" if app.transcript_view.borrow().has_selection() => {
                app.transcript_view.get_mut().request_copy();
            }
            "/model" if !rest.is_empty() => self.call(Call::Configure, json!({"model": rest})),
            "/model" | "/models" | "/scoped-models" => app.push(
                TuiEntryKind::System,
                "when attached, /model names the model: /model <provider/id>",
            ),
            "/thinking" if !rest.is_empty() => {
                self.call(Call::Configure, json!({"thinkingLevel": rest}))
            }
            "/thinking" => app.push(
                TuiEntryKind::System,
                format!(
                    "thinking: {}",
                    self.view["docs"]["pi.agent"]["thinkingLevel"]
                        .as_str()
                        .unwrap_or("-")
                ),
            ),
            "/compact" => {
                let mut args = json!({});
                if !rest.is_empty() {
                    args["instructions"] = json!(rest);
                }
                self.call(Call::Compact, args);
            }
            "/queue" if rest.is_empty() => {
                let queued = inbox(&self.view);
                app.push(
                    TuiEntryKind::System,
                    if queued.is_empty() {
                        "no queued submissions".to_owned()
                    } else {
                        queued
                            .iter()
                            .enumerate()
                            .map(|(index, (_, mode, text))| format!("{}. {mode}: {text}", index + 1))
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                );
            }
            "/queue-clear" => {
                for (id, _, text) in inbox(&self.view) {
                    self.withdraw(id, text, false);
                }
            }
            "/interrupt" => self.call(Call::Abort, json!({})),
            _ if is_prompt_command(line, name) => self.prompt(app, line.to_owned(), steer),
            _ => app.push(
                TuiEntryKind::System,
                format!("{name} is not available when attached"),
            ),
        }
        Action::None
    }

    fn describe(&self) -> String {
        let conversation = &self.view["conversation"];
        let agent = &self.view["docs"]["pi.agent"];
        let mut lines = vec![
            format!("session: {}", self.session),
            format!("cwd: {}", conversation["cwd"].as_str().unwrap_or_default()),
        ];
        if let Some(name) = conversation["name"].as_str() {
            lines.push(format!("name: {name}"));
        }
        lines.push(format!(
            "model: {} · thinking: {}",
            agent["model"].as_str().unwrap_or("-"),
            agent["thinkingLevel"].as_str().unwrap_or("-")
        ));
        lines.push(format!("messages: {}", entries(&self.view).len()));
        if let Some((run_id, status)) = &self.last_run {
            lines.push(format!("last run: {run_id} {status}"));
        }
        lines.push("attached to noel server".to_owned());
        lines.join("\n")
    }

    // ─── calls ──────────────────────────────────────────────────────────────

    /// Submits `text` with a new `requestId`, sent again with that id while
    /// it fails `unavailable`. A prompt that meets a run another client
    /// started meanwhile is queued after it, as Enter queues during a run.
    fn submit(&self, text: String, when_busy: &'static str) {
        let (client, session, sender) = (
            self.client.clone(),
            self.session.clone(),
            self.sender.clone(),
        );
        tokio::spawn(async move {
            let send = |when_busy: &str| {
                let args =
                    json!({"requestId": new_request_id(), "text": text, "whenBusy": when_busy});
                client.chat_call(&session, "submit", args, true)
            };
            let mut result = send(when_busy).await;
            if when_busy == "reject" && refused_busy(&result) {
                result = send("followUp").await;
            }
            let _ = sender.send(Incoming::Answer(Call::Submit(text), result));
        });
    }

    fn call(&self, call: Call, args: Value) {
        let method = match &call {
            Call::Submit(_) => "submit",
            Call::Withdraw { .. } => "withdraw",
            Call::Abort => "abort",
            Call::Configure => "configure",
            Call::Compact => "compact",
        };
        let (client, session, sender) = (
            self.client.clone(),
            self.session.clone(),
            self.sender.clone(),
        );
        tokio::spawn(async move {
            let result = client.chat_call(&session, method, args, false).await;
            let _ = sender.send(Incoming::Answer(call, result));
        });
    }

    /// Aborts the active run, once.
    fn abort(&mut self) {
        let Some((run_id, _)) = &self.run else { return };
        if self.aborting.as_ref() == Some(run_id) {
            return;
        }
        self.aborting = Some(run_id.clone());
        self.call(Call::Abort, json!({"expectedRunId": run_id}));
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
        let args = json!({ "submissionId": id });
        self.call(Call::Withdraw { id, text, recall }, args);
    }

    fn answer(&mut self, app: &mut TuiApp, call: Call, result: Result<Value>) {
        if let Call::Withdraw { id, .. } = &call {
            self.withdrawing.retain(|withdrawing| withdrawing != id);
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                let message = match &call {
                    Call::Submit(text) => format!("not sent: {} ({error})", one_line(text)),
                    Call::Abort => {
                        self.aborting = None;
                        format!("abort: {error}")
                    }
                    Call::Withdraw { text, .. } => {
                        format!("withdraw {}: {error}", one_line(text))
                    }
                    Call::Configure | Call::Compact => error.to_string(),
                };
                app.push(TuiEntryKind::Error, message);
                return;
            }
        };
        match call {
            Call::Submit(_) => {}
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
            Call::Configure => app.push(
                TuiEntryKind::System,
                format!(
                    "model: {} · thinking: {}",
                    value["model"].as_str().unwrap_or("-"),
                    value["thinkingLevel"].as_str().unwrap_or("-")
                ),
            ),
            Call::Compact => app.push(
                TuiEntryKind::System,
                format!(
                    "compacted: omitted {} message(s), retained {} message(s)",
                    value["omittedMessages"], value["retainedMessages"]
                ),
            ),
        }
    }
}

/// A refusal because a run is active: `invalid/busy`.
fn refused_busy(result: &Result<Value>) -> bool {
    result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<ChordError>())
        .is_some_and(|error| {
            error.code == ErrorCode::Invalid && error.reason.as_deref() == Some("busy")
        })
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
    if live.get("compaction").is_some() {
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

/// The first entry `ops` may have changed; none when they leave the entries
/// alone.
fn entries_changed_from(ops: &[Op]) -> usize {
    ops.iter()
        .filter_map(|op| {
            let Some(path) = op.path() else {
                return Some(0);
            };
            match path.first() {
                Some(Seg::Key(key)) if key == "entries" => {}
                Some(_) => return None,
                None => return Some(0),
            }
            Some(match (path.get(1), op) {
                (None, Op::Splice(_, index, _, _)) => *index as usize,
                (Some(Seg::Index(index)), _) => *index as usize,
                _ => 0,
            })
        })
        .min()
        .unwrap_or(usize::MAX)
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
    let entry = &entries[index];
    let mut rows = Vec::new();
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
    if let Some(error) = generation["retry"]["error"].as_str() {
        add(
            &mut rows,
            TuiEntryKind::System,
            format!("retrying (attempt {}): {error}", generation["attempt"]),
            Source::Tool,
        );
    }
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
    fn sync(&mut self, app: &mut TuiApp, view: &Value, dirty: usize) {
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
        for index in gone.into_iter().rev() {
            app.remove_entry(index);
            for (_, shown) in &mut self.rows {
                if *shown > index {
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
                    TuiEntryKind::System,
                    "retrying (attempt 2): overloaded".to_owned(),
                    Source::Tool
                ),
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
        // Compaction rewrites the entries: the rows follow.
        transcript.sync(&mut app, &view(json!([user("summary")]), json!({})), 0);
        assert_eq!(shown(&app), [(TuiEntryKind::User, "summary")]);
        assert_eq!((transcript.rows.len(), transcript.committed), (1, 1));
    }

    #[test]
    fn ops_name_the_first_entry_they_change() {
        let entries = |index| vec![Seg::Key("entries".into()), Seg::Index(index)];
        let live = vec![Seg::Key("docs".into()), Seg::Key("pi.live".into())];
        assert_eq!(
            entries_changed_from(&[Op::Append(live.clone(), "x".into())]),
            usize::MAX
        );
        assert_eq!(
            entries_changed_from(&[
                Op::Splice(vec![Seg::Key("entries".into())], 4, 0, vec![]),
                Op::Set(entries(2), json!({})),
            ]),
            2
        );
        assert_eq!(entries_changed_from(&[Op::Replace(json!({}))]), 0);
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
