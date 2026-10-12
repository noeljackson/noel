//! `pi --mode rpc`: upstream pi's RPC protocol on stdin and stdout, plus
//! the extensions named in `get_state.rpcExtensions`. `run-id`: every run
//! has a `runId`, given in the prompt reply, `agent_start`, `agent_settled`
//! and `get_state` while it runs. `expected-run-id`: `steer`, `follow_up`
//! and `abort` take an `expectedRunId` and change nothing unless it names
//! the active run. `request-id`: a `prompt` that repeats a `requestId` gets
//! the run the first one started. `resume-run`: `get_state.lastRun` says
//! how the session's last run ended, and `resume_run` continues it if a
//! stop interrupted it.
//!
//! One task reads commands and drives the active run, so records keep the
//! order things happen in. While a run is active, commands that change the
//! session are refused and reads answer from a copy of the session that the
//! run's events keep current. stdout carries only protocol records.
//!
//! The session journal saves each run's start with its prompt, before the
//! prompt is answered, and its end before `agent_settled`. It saves the
//! queue too, before acknowledging what joins it.

use std::future::Future;
use std::io::{self, BufRead, Write};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use base64::Engine;
use pi_ai::ModelRef;
use pi_config::LoadedConfig;
use pi_core::{
    continue_turn_streaming_events, run_saved_prompt_streaming_events,
    run_user_turn_streaming_events_with_media, AgentError, CancellationToken, CompactionKind,
    ConversationMessage, MessageRole, RunControl, RunStart, RunStatus, Runtime, SessionState,
    SessionStore, ToolEvent, TurnEvent,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::rpc_wire::{
    Command, Contents, Data, Event, Input, LastRun, Model, Outcome, Prompt, Queue, Recorder, Reply,
    Request, Response, Source, State, StreamingBehavior, BUSY, QUEUED_REQUEST, RPC_EXTENSIONS,
};

type Lines = mpsc::UnboundedReceiver<String>;

/// Serves the protocol until stdin closes. EOF while idle exits; EOF during
/// a run stops it where it is, without settling it, and exits: the run
/// stays interrupted, for `resume_run`.
pub(crate) async fn run(runtime: Runtime, config: LoadedConfig, offline: bool) -> Result<()> {
    let mut lines = read_lines();
    let mut shared = Shared::default();
    shared.open_queue(&runtime);
    let mut rpc = Rpc {
        runtime,
        config,
        offline,
        shared: Mutex::new(shared),
    };
    while let Some(line) = lines.recv().await {
        if let Some(start) = rpc.command(&line).await {
            if !rpc.run(start, &mut lines).await {
                // The stopped run's tools died with it; leave none behind.
                pi_core::kill_running_tool_groups();
                break;
            }
        }
    }
    Ok(())
}

/// Splits stdin into lines on LF only, as upstream's JSONL reader does, so
/// U+2028 and U+2029 stay inside their JSON strings. A trailing CR goes, and
/// a last line without LF still counts.
fn read_lines() -> Lines {
    let (sender, lines) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut line = Vec::new();
        while matches!(stdin.read_until(b'\n', &mut line), Ok(1..)) {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if sender
                .send(String::from_utf8_lossy(&line).into_owned())
                .is_err()
            {
                break;
            }
            line.clear();
        }
    });
    lines
}

/// Writes one record as one line.
fn send(record: &impl Serialize) {
    let mut line = serde_json::to_vec(record).expect("RPC records serialize");
    line.push(b'\n');
    let mut stdout = io::stdout().lock();
    // A closed stdout means the client is gone; stdin's EOF ends the loop.
    let _ = stdout.write_all(&line).and_then(|()| stdout.flush());
}

/// Answers the command that starts `start`, before the run's records.
fn started(request: &Request, start: Start) -> Start {
    send(&Response::to(
        request,
        Ok(Some(Data::started(&start.run_id))),
    ));
    start
}

/// RPC mode between commands: the session runtime and what runs share.
struct Rpc {
    runtime: Runtime,
    config: LoadedConfig,
    offline: bool,
    /// Shared with the event callback of the active run's engine call.
    shared: Mutex<Shared>,
}

/// What both the command loop and the engine's events use. One task runs
/// both, so the lock is never contended.
#[derive(Default)]
struct Shared {
    /// The queue lives here; the session's copy is read when it opens.
    queue: Queue,
    /// Where the queue is saved (none with `--no-session`), and what was
    /// saved last.
    store: Option<SessionStore>,
    saved: Contents,
    run: Option<Run>,
}

/// The active run.
struct Run {
    recorder: Recorder,
    cancellation: CancellationToken,
    /// The session as the run has left it so far, for reads.
    session: SessionState,
    /// What `get_state` reports while the run cannot change it.
    state: State,
    /// Aborts to answer once the run settles.
    aborts: Vec<Option<Value>>,
}

/// A run accepted to start, or to continue.
struct Start {
    run_id: String,
    provider: Box<dyn pi_ai::Provider>,
    source: Source,
    call: Call,
}

/// What an engine call of a run starts from.
enum Call {
    /// The prompt that started the run, saved with its start.
    Prompt(ConversationMessage),
    /// Follow-ups taken from the queue.
    FollowUps(String),
    /// The saved context of an interrupted run.
    Resume,
}

impl Call {
    fn prompt(&self) -> Option<Prompt> {
        match self {
            Call::Prompt(_) => Some(Prompt::Initial),
            Call::FollowUps(_) => Some(Prompt::FollowUps),
            Call::Resume => None,
        }
    }
}

impl Rpc {
    fn shared(&mut self) -> &mut Shared {
        self.shared.get_mut().expect("rpc state")
    }

    /// Handles a command while no run is active; a prompt that starts a
    /// run, or a `resume_run`, is returned to run.
    async fn command(&mut self, line: &str) -> Option<Start> {
        let request = match Request::parse(line) {
            Ok(request) => request,
            Err(error) => {
                send(&Response::parse_error(error));
                return None;
            }
        };
        let reply = match &request.command {
            Err(error) => Err(error.clone()),
            Ok(Command::Prompt(input)) => match self.repeated(input) {
                Some(data) => Ok(Some(data)),
                None => match self.start(input).await {
                    Ok(start) => return Some(started(&request, start)),
                    Err(error) => Err(error),
                },
            },
            Ok(Command::ResumeRun { expected_run_id }) => {
                match self.resume(expected_run_id).await {
                    Ok(start) => return Some(started(&request, start)),
                    Err(error) => Err(error),
                }
            }
            Ok(Command::Steer(input)) => self.shared().enqueue(StreamingBehavior::Steer, input),
            Ok(Command::FollowUp(input)) => {
                self.shared().enqueue(StreamingBehavior::FollowUp, input)
            }
            Ok(Command::Abort { expected_run_id }) => self.abort(expected_run_id),
            Ok(Command::ClearQueue) => self.shared().clear_queue(),
            Ok(Command::NewSession { parent_session }) => {
                self.new_session(parent_session.as_deref())
            }
            Ok(Command::SwitchSession { session_path }) => self.switch_session(session_path),
            Ok(Command::GetState) => Ok(Some(Data::State(Box::new(self.state())))),
            Ok(Command::GetMessages) => Ok(Some(Data::messages(self.runtime.session()))),
            Ok(Command::GetLastAssistantText) => {
                Ok(Some(Data::last_assistant_text(self.runtime.session())))
            }
            Ok(Command::SetModel { provider, model_id }) => self.set_model(provider, model_id),
            Ok(Command::SetThinkingLevel { level }) => self.set_thinking_level(level),
            Ok(Command::SetSteeringMode { mode } | Command::SetFollowUpMode { mode }) => {
                mode.reply()
            }
            Ok(Command::Compact {
                custom_instructions,
            }) => self.compact(custom_instructions.as_deref()).await,
            Ok(Command::SetSessionName { name }) => self.set_session_name(name),
            Ok(Command::Other) => request.unknown(),
        };
        send(&Response::to(&request, reply));
        None
    }

    /// The reply to a prompt whose `requestId` already started a run.
    fn repeated(&self, input: &Input) -> Option<Data> {
        let request_id = input.request_id.as_deref()?;
        let run = self.runtime.session().run_for_request(request_id)?;
        Some(Data::repeated(run))
    }

    /// Checks a prompt (its images, and a provider for the active model
    /// with a usable credential), then saves it with its run's start. Each
    /// run gets its own provider, so `set_model` applies to the next run.
    async fn start(&mut self, input: &Input) -> Result<Start, String> {
        if !input.images.is_empty() && crate::images_blocked(&self.config) {
            return Err("images are blocked by settings".into());
        }
        let media = input
            .images
            .iter()
            .map(|image| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .map_err(|error| format!("invalid image data: {error}"))?;
                crate::media_input_from_bytes(bytes, image.mime_type.clone(), None, &self.config)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<_, _>>()?;
        let (provider, source) = self.provider().await?;
        let run = RunStart {
            id: run_id(),
            request_id: input.request_id.clone(),
        };
        let run_id = run.id.clone();
        let prompt = self
            .runtime
            .start_run(run, input.message.clone(), media)
            .map_err(|error| error.to_string())?;
        Ok(Start {
            run_id,
            provider,
            source,
            call: Call::Prompt(prompt),
        })
    }

    /// Accepts `resume_run` for the session's last run, if a stop
    /// interrupted it.
    async fn resume(&mut self, expected_run_id: &str) -> Result<Start, String> {
        let Some(run) = self.runtime.session().runs.last() else {
            return Err(format!(
                "expectedRunId {expected_run_id} does not match: the session has no run"
            ));
        };
        if run.id != expected_run_id {
            return Err(format!(
                "expectedRunId {expected_run_id} does not match the last run {}",
                run.id
            ));
        }
        if run.status != RunStatus::Interrupted {
            return Err(format!("run {expected_run_id} is not interrupted"));
        }
        let (provider, source) = self.provider().await?;
        self.runtime.resume_run(expected_run_id);
        Ok(Start {
            run_id: expected_run_id.to_owned(),
            provider,
            source,
            call: Call::Resume,
        })
    }

    /// A provider for the active model with a usable credential, and what
    /// the run's replies name.
    async fn provider(&self) -> Result<(Box<dyn pi_ai::Provider>, Source), String> {
        let provider = crate::provider_for_runtime(&self.runtime, &self.config, self.offline)
            .await
            .map_err(|error| error.to_string())?;
        // The provider exists, so the active model is configured.
        let model = self.model().ok_or("the active model is not configured")?;
        let source = Source {
            api: model.api,
            provider: model.provider,
            model: model.id,
            thinking_level: self.thinking_level(),
        };
        Ok((provider, source))
    }

    /// Drives a run until it settles: its first engine call, then one per
    /// batch of follow-ups queued meanwhile. How it ended is saved before
    /// `agent_settled`. Returns `false` if stdin closed during the run, which
    /// leaves it interrupted.
    async fn run(&mut self, start: Start, lines: &mut Lines) -> bool {
        let Start {
            run_id,
            provider,
            source,
            call,
        } = start;
        let control = RunControl {
            steering: self.shared().queue.steering.clone(),
            cancellation: CancellationToken::default(),
        };
        let mut state = self.state();
        state.is_streaming = true;
        let mut session = self.runtime.session().clone();
        if matches!(call, Call::Prompt(_)) {
            // The engine reports the saved prompt, and the copy takes it then.
            session.messages.pop();
        }
        let run = Run {
            recorder: Recorder::start(run_id.clone(), source, &mut |record| send(&record)),
            cancellation: control.cancellation.clone(),
            session,
            state,
            aborts: Vec::new(),
        };
        self.shared().run = Some(run);

        let Rpc {
            runtime, shared, ..
        } = self;
        let shared = &*shared;
        let mut next = Some(call);
        let outcome = loop {
            let Some(call) = next.take() else {
                break Outcome::Completed;
            };
            lock(shared).next_call(call.prompt());
            // Where the messages this call saves start, for the records of
            // those it does not report; a saved prompt is one of them.
            let before =
                runtime.session().messages.len() - usize::from(matches!(call, Call::Prompt(_)));
            let on_event = |event: &TurnEvent| lock(shared).record(event);
            let turn = async {
                let provider = provider.as_ref();
                match call {
                    Call::Prompt(prompt) => {
                        run_saved_prompt_streaming_events(
                            runtime, provider, prompt, &control, on_event,
                        )
                        .await
                    }
                    Call::FollowUps(prompt) => {
                        run_user_turn_streaming_events_with_media(
                            runtime,
                            provider,
                            prompt,
                            Vec::new(),
                            &control,
                            on_event,
                        )
                        .await
                    }
                    Call::Resume => {
                        continue_turn_streaming_events(runtime, provider, &control, on_event).await
                    }
                }
            };
            let Some(result) = drive(turn, lines, shared).await else {
                return false;
            };
            let mut guard = lock(shared);
            guard.call_returned(runtime.session(), before);
            match result {
                Ok(_) => next = guard.queue.take_follow_ups().map(Call::FollowUps),
                Err(AgentError::Cancelled) => break Outcome::Aborted,
                Err(error) => break Outcome::Failed(error.to_string()),
            }
        };
        if let Err(error) = runtime.finish_run(&run_id, outcome.status()) {
            eprintln!("pi: {error}");
        }
        self.shared().settle(outcome);
        true
    }

    /// The `get_state` fields while no run is active.
    fn state(&mut self) -> State {
        let model = self.model();
        let thinking_level = self.thinking_level();
        let pending_message_count = self.shared().queue.pending();
        let session = self.runtime.session();
        State {
            model,
            thinking_level,
            is_streaming: false,
            is_compacting: false,
            steering_mode: "all",
            follow_up_mode: "all",
            session_file: self
                .runtime
                .store()
                .map(|store| store.path().display().to_string()),
            session_id: session.session_id.clone(),
            session_name: session.name.clone(),
            auto_compaction_enabled: self.runtime.systems().compaction.enabled,
            message_count: session.messages.len(),
            pending_message_count,
            rpc_extensions: RPC_EXTENSIONS,
            run_id: None,
            last_run: LastRun::of(&self.runtime),
        }
    }

    /// `abort` while no run is active ends an interrupted last run, so it
    /// is never resumed.
    fn abort(&mut self, expected_run_id: &Option<String>) -> Reply {
        let interrupted = self
            .runtime
            .session()
            .runs
            .last()
            .filter(|run| run.status == RunStatus::Interrupted)
            .map(|run| run.id.clone());
        match (interrupted, expected_run_id) {
            (Some(run_id), Some(expected)) if run_id != *expected => Err(format!(
                "expectedRunId {expected} does not match the interrupted run {run_id}"
            )),
            (Some(run_id), _) => self
                .runtime
                .finish_run(&run_id, RunStatus::Aborted)
                .map(|()| None)
                .map_err(|error| error.to_string()),
            (None, _) => self.shared().fence(expected_run_id).map(|()| None),
        }
    }

    /// Another session is open: its saved queue becomes the queue.
    fn reopen_queue(&mut self) {
        let shared = self.shared.get_mut().expect("rpc state");
        if shared.open_queue(&self.runtime) {
            send(&shared.queue.update());
        }
    }

    /// The active model as upstream's model object.
    fn model(&self) -> Option<Model> {
        let model = self.runtime.session().active_model.as_ref()?;
        Model::configured(&self.config, model)
    }

    fn thinking_level(&self) -> String {
        self.runtime
            .session()
            .active_model
            .as_ref()
            .and_then(|model| crate::active_thinking_level(&self.runtime, &self.config, model))
            .unwrap_or_else(|| "off".into())
    }

    fn new_session(&mut self, parent_session: Option<&str>) -> Reply {
        if parent_session.is_some() {
            return Err("new_session does not support parentSession".into());
        }
        if self.runtime.store().is_some() {
            crate::start_new_session(&mut self.runtime, &self.config).map_err(|e| e.to_string())?;
        } else {
            // Without a journal (`--no-session`), the same reset in memory.
            let current = self.runtime.session();
            let mut session = SessionState::new(current.session_id.clone(), current.cwd.clone());
            session.active_model = current.active_model.clone();
            session.active_thinking_level = current.active_thinking_level.clone();
            session.active_account = current.active_account.clone();
            self.runtime.replace_session(session, None);
        }
        self.reopen_queue();
        Ok(Some(Data::Cancelled { cancelled: false }))
    }

    fn switch_session(&mut self, session_path: &str) -> Reply {
        let path = crate::resolve_session_reference(&self.config.paths.session_dir, session_path)
            .map_err(|error| error.to_string())?;
        let (store, session) = SessionStore::open(path).map_err(|error| error.to_string())?;
        self.runtime.replace_session(session, Some(store));
        self.reopen_queue();
        Ok(Some(Data::Cancelled { cancelled: false }))
    }

    /// Selects the session's model; unlike the TUI's `/model`, the default
    /// model in settings stays as it is.
    fn set_model(&mut self, provider: &str, model_id: &str) -> Reply {
        let model = ModelRef {
            provider: provider.to_owned(),
            id: model_id.to_owned(),
        };
        let info = Model::configured(&self.config, &model)
            .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
        let thinking = crate::active_thinking_level(&self.runtime, &self.config, &model);
        self.runtime
            .set_active_model(Some(model))
            .and_then(|()| self.runtime.set_active_thinking_level(thinking))
            .map_err(|error| error.to_string())?;
        Ok(Some(Data::Model(Box::new(info))))
    }

    fn set_thinking_level(&mut self, level: &str) -> Reply {
        let level = crate::normalized_thinking_level(level)
            .ok_or_else(|| format!("Unknown thinking level: {level}"))?;
        if let Some(model) = &self.runtime.session().active_model {
            if level != "off" && !crate::model_thinking_levels(model).contains(&level) {
                return Err(format!(
                    "{}/{} does not support thinking level {level}",
                    model.provider, model.id
                ));
            }
        }
        let before = self.thinking_level();
        self.runtime
            .set_active_thinking_level(Some(level.to_owned()))
            .map_err(|error| error.to_string())?;
        let after = self.thinking_level();
        if after != before {
            send(&Event::ThinkingLevelChanged { level: &after });
        }
        Ok(None)
    }

    fn set_session_name(&mut self, name: &str) -> Reply {
        let name = name.trim();
        if name.is_empty() {
            return Err("Session name cannot be empty".into());
        }
        self.runtime
            .rename_session(Some(name.to_owned()))
            .map_err(|error| error.to_string())?;
        send(&Event::SessionInfoChanged { name });
        Ok(None)
    }

    /// Compacts now. Commands sent meanwhile wait until it is done.
    async fn compact(&mut self, focus: Option<&str>) -> Reply {
        let provider = crate::provider_for_runtime(&self.runtime, &self.config, self.offline)
            .await
            .map_err(|error| error.to_string())?;
        send(&Event::CompactionStart { reason: "manual" });
        let result = self
            .runtime
            .compact_messages(
                provider.as_ref(),
                CompactionKind::Manual,
                focus.unwrap_or(""),
            )
            .await
            .map_err(|error| error.to_string());
        send(&Event::CompactionEnd {
            reason: "manual",
            result: result.as_ref().ok().map(Into::into),
            aborted: false,
            will_retry: false,
            error_message: result.as_ref().err().map(String::as_str),
        });
        Ok(Some(Data::Compaction((&result?).into())))
    }
}

/// Polls the engine call until it ends, handling the commands that arrive
/// meanwhile. `None` if stdin closed first.
async fn drive<T>(
    turn: impl Future<Output = T>,
    lines: &mut Lines,
    shared: &Mutex<Shared>,
) -> Option<T> {
    tokio::pin!(turn);
    loop {
        tokio::select! {
            biased;
            result = &mut turn => return Some(result),
            line = lines.recv() => lock(shared).command(&line?),
        }
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().expect("rpc state")
}

impl Shared {
    fn active(&mut self) -> &mut Run {
        self.run.as_mut().expect("a run is active")
    }

    /// Handles a command while a run is active.
    fn command(&mut self, line: &str) {
        let request = match Request::parse(line) {
            Ok(request) => request,
            Err(error) => return send(&Response::parse_error(error)),
        };
        let reply = match &request.command {
            Err(error) => Err(error.clone()),
            Ok(Command::Prompt(input)) => {
                let request_id = input.request_id.as_deref();
                let repeated = request_id
                    .and_then(|request_id| self.active().session.run_for_request(request_id))
                    .map(Data::repeated);
                match (repeated, input.streaming_behavior) {
                    (Some(data), _) => Ok(Some(data)),
                    (None, None) => Err(BUSY.into()),
                    (None, Some(_)) if request_id.is_some() => Err(QUEUED_REQUEST.into()),
                    (None, Some(behavior)) => self.enqueue(behavior, input),
                }
            }
            Ok(Command::Steer(input)) => self.enqueue(StreamingBehavior::Steer, input),
            Ok(Command::FollowUp(input)) => self.enqueue(StreamingBehavior::FollowUp, input),
            Ok(Command::Abort { expected_run_id }) => match self.fence(expected_run_id) {
                // Answered once the run settles.
                Ok(()) => {
                    let run = self.active();
                    run.cancellation.cancel();
                    run.aborts.push(request.id.clone());
                    return;
                }
                Err(error) => Err(error),
            },
            Ok(Command::ClearQueue) => self.clear_queue(),
            Ok(Command::GetState) => {
                let pending_message_count = self.queue.pending();
                let run = self.active();
                Ok(Some(Data::State(Box::new(State {
                    is_compacting: run.recorder.compacting(),
                    message_count: run.session.messages.len(),
                    pending_message_count,
                    run_id: Some(run.recorder.id().to_owned()),
                    ..run.state.clone()
                }))))
            }
            Ok(Command::GetMessages) => Ok(Some(Data::messages(&self.active().session))),
            Ok(Command::GetLastAssistantText) => {
                Ok(Some(Data::last_assistant_text(&self.active().session)))
            }
            Ok(Command::SetSteeringMode { mode } | Command::SetFollowUpMode { mode }) => {
                mode.reply()
            }
            Ok(
                Command::ResumeRun { .. }
                | Command::NewSession { .. }
                | Command::SwitchSession { .. }
                | Command::SetModel { .. }
                | Command::SetThinkingLevel { .. }
                | Command::Compact { .. }
                | Command::SetSessionName { .. },
            ) => Err(format!(
                "Cannot {} while the agent is running; wait for agent_settled",
                request.kind.as_deref().unwrap_or_default()
            )),
            Ok(Command::Other) => request.unknown(),
        };
        send(&Response::to(&request, reply));
    }

    /// `expectedRunId`, when given, must name the active run.
    fn fence(&self, expected_run_id: &Option<String>) -> Result<(), String> {
        let Some(expected) = expected_run_id else {
            return Ok(());
        };
        match &self.run {
            Some(run) if run.recorder.id() == expected => Ok(()),
            Some(run) => Err(format!(
                "expectedRunId {expected} does not match the active run {}",
                run.recorder.id()
            )),
            None => Err(format!(
                "expectedRunId {expected} does not match: no run is active"
            )),
        }
    }

    /// Queues a message, as steering or a follow-up, once it is saved.
    fn enqueue(&mut self, behavior: StreamingBehavior, input: &Input) -> Reply {
        self.fence(&input.expected_run_id)?;
        if !input.images.is_empty() {
            return Err(
                "queued messages are text only; send images with a prompt that starts a run".into(),
            );
        }
        let (mut steering, mut follow_ups) = self.queue.contents();
        match behavior {
            StreamingBehavior::Steer => steering.push(input.message.clone()),
            StreamingBehavior::FollowUp => follow_ups.push(input.message.clone()),
        }
        self.set_queue((steering, follow_ups))?;
        Ok(Some(Data::queued()))
    }

    fn clear_queue(&mut self) -> Reply {
        let (steering, follow_up) = self.queue.contents();
        self.set_queue(Contents::default())?;
        Ok(Some(Data::Queue {
            steering,
            follow_up,
        }))
    }

    /// Saves `contents`, then makes it the queue and reports it. A save
    /// that fails changes nothing.
    fn set_queue(&mut self, contents: Contents) -> Result<(), String> {
        self.save_queue(&contents)
            .map_err(|error| error.to_string())?;
        self.queue.replace(contents);
        send(&self.queue.update());
        Ok(())
    }

    /// Saves the queue with the session unless it is saved already.
    fn save_queue(&mut self, contents: &Contents) -> Result<(), pi_core::SessionError> {
        if *contents == self.saved {
            return Ok(());
        }
        if let Some(store) = &self.store {
            store.record_queue(contents.0.clone(), contents.1.clone())?;
        }
        self.saved = contents.clone();
        Ok(())
    }

    /// Delivered messages leave the saved queue once they are saved as
    /// user messages. Should this fail, a restart restores them.
    fn save_delivered(&mut self) {
        let contents = self.queue.contents();
        if let Err(error) = self.save_queue(&contents) {
            eprintln!("pi: {error}");
        }
    }

    /// Takes the queue saved with the session `runtime` has open, and
    /// whether that changed the queue.
    fn open_queue(&mut self, runtime: &Runtime) -> bool {
        let (steering, follow_ups) = runtime.session().saved_queue();
        let saved = (steering.to_vec(), follow_ups.to_vec());
        let changed = self.queue.contents() != saved;
        self.queue.replace(saved.clone());
        self.saved = saved;
        self.store = runtime.store().cloned();
        changed
    }

    /// The next engine call of the active run starts with `prompt`, or
    /// continues the run.
    fn next_call(&mut self, prompt: Option<Prompt>) {
        self.active().recorder.call(prompt);
    }

    /// An event from the active run's engine call.
    fn record(&mut self, event: &TurnEvent) {
        let Self { queue, run, .. } = self;
        let Some(run) = run else {
            return;
        };
        run.mirror(event);
        run.recorder
            .record(event, queue, &mut |record| send(&record));
        if matches!(event, TurnEvent::MessageAdded(message) if message.role == MessageRole::User) {
            self.save_delivered();
        }
    }

    /// An engine call returned: report what it saved unreported and read
    /// the session afresh (a compaction rewrites it).
    fn call_returned(&mut self, session: &SessionState, before: usize) {
        let Self { queue, run, .. } = self;
        let Some(run) = run else {
            return;
        };
        let added = session.messages.get(before..).unwrap_or_default();
        run.recorder
            .unreported(added, session, queue, &mut |record| send(&record));
        run.session = session.clone();
        self.save_delivered();
    }

    /// Ends the active run's records and answers the aborts it waited for.
    fn settle(&mut self, outcome: Outcome) {
        let Some(run) = self.run.take() else {
            return;
        };
        run.recorder.finish(outcome, &mut |record| send(&record));
        for id in run.aborts {
            send(&Response::new(id.as_ref(), Some("abort"), Ok(None)));
        }
    }
}

impl Run {
    /// Keeps the session copy in step with what the engine saves.
    fn mirror(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::MessageAdded(message) => self.session.messages.push(message.clone()),
            TurnEvent::ToolOutput {
                call,
                output,
                images,
                is_error,
                ..
            } => {
                self.session.tool_history.push(ToolEvent {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    result: output.clone(),
                    is_error: *is_error,
                });
                self.session.messages.push(ConversationMessage {
                    role: MessageRole::Tool,
                    content: output.clone(),
                    thinking: String::new(),
                    media: images.clone(),
                    tool_call_id: Some(call.id.clone()),
                    tool_name: Some(call.name.clone()),
                    tool_calls: Vec::new(),
                });
            }
            _ => {}
        }
    }
}

/// A random UUID (version 4) naming a run.
fn run_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system randomness");
    bytes[6] = 0x40 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
