//! `pi --mode rpc`: upstream pi's RPC protocol on stdin and stdout, plus
//! the extensions named in `get_state.rpcExtensions`. `run-id`: every run
//! has a `runId`, given in the prompt reply, `agent_start`, `agent_settled`
//! and `get_state` while it runs. `expected-run-id`: `steer`, `follow_up`
//! and `abort` take an `expectedRunId` and change nothing unless it names
//! the active run.
//!
//! One task reads commands and drives the active run, so records keep the
//! order things happen in. While a run is active, commands that change the
//! session are refused and reads answer from a copy of the session that the
//! run's events keep current. stdout carries only protocol records.

use std::future::Future;
use std::io::{self, BufRead, Write};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use base64::Engine;
use pi_ai::{MediaInput, ModelRef};
use pi_config::LoadedConfig;
use pi_core::{
    run_user_turn_streaming_events_with_media, AgentError, CancellationToken, CompactionKind,
    ConversationMessage, MessageRole, RunControl, Runtime, SessionState, SessionStore, ToolEvent,
    TurnEvent,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::rpc_wire::{
    Command, Data, Event, Input, Model, Outcome, Prompt, Queue, Recorder, Reply, Request, Response,
    Source, State, StreamingBehavior, BUSY, RPC_EXTENSIONS,
};

type Lines = mpsc::UnboundedReceiver<String>;

/// Serves the protocol until stdin closes. EOF while idle exits; EOF during
/// a run stops it where it is, without settling it, and exits.
pub(crate) async fn run(runtime: Runtime, config: LoadedConfig, offline: bool) -> Result<()> {
    let mut lines = read_lines();
    let mut rpc = Rpc {
        runtime,
        config,
        offline,
        shared: Mutex::new(Shared::default()),
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
    queue: Queue,
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

/// A prompt accepted to start a run.
struct Start {
    run_id: String,
    provider: Box<dyn pi_ai::Provider>,
    source: Source,
    prompt: String,
    media: Vec<MediaInput>,
}

impl Rpc {
    fn shared(&mut self) -> &mut Shared {
        self.shared.get_mut().expect("rpc state")
    }

    /// Handles a command while no run is active; a prompt that starts one
    /// is returned to run.
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
            Ok(Command::Prompt(input)) => match self.start(input).await {
                Ok(start) => {
                    send(&Response::to(
                        &request,
                        Ok(Some(Data::started(&start.run_id))),
                    ));
                    return Some(start);
                }
                Err(error) => Err(error),
            },
            Ok(Command::Steer(input)) => self.shared().enqueue(StreamingBehavior::Steer, input),
            Ok(Command::FollowUp(input)) => {
                self.shared().enqueue(StreamingBehavior::FollowUp, input)
            }
            Ok(Command::Abort { expected_run_id }) => {
                self.shared().fence(expected_run_id).map(|()| None)
            }
            Ok(Command::ClearQueue) => Ok(Some(self.shared().clear_queue())),
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

    /// Checks a prompt before accepting it: its images, and a provider for
    /// the active model with a usable credential. Each run gets its own, so
    /// `set_model` applies to the next run.
    async fn start(&self, input: &Input) -> Result<Start, String> {
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
        let provider = crate::provider_for_runtime(&self.runtime, &self.config, self.offline)
            .await
            .map_err(|error| error.to_string())?;
        // The provider exists, so the active model is configured.
        let model = self.model().ok_or("the active model is not configured")?;
        Ok(Start {
            run_id: run_id(),
            provider,
            source: Source {
                api: model.api,
                provider: model.provider,
                model: model.id,
                thinking_level: self.thinking_level(),
            },
            prompt: input.message.clone(),
            media,
        })
    }

    /// Runs a prompt, and the follow-ups queued meanwhile, as one run until
    /// it settles. Returns `false` if stdin closed during the run.
    async fn run(&mut self, start: Start, lines: &mut Lines) -> bool {
        let Start {
            run_id,
            provider,
            source,
            prompt,
            media,
        } = start;
        let control = RunControl {
            steering: self.shared().queue.steering.clone(),
            cancellation: CancellationToken::default(),
        };
        let mut state = self.state();
        state.is_streaming = true;
        let run = Run {
            recorder: Recorder::start(run_id, source, &mut |record| send(&record)),
            cancellation: control.cancellation.clone(),
            session: self.runtime.session().clone(),
            state,
            aborts: Vec::new(),
        };
        self.shared().run = Some(run);

        let Rpc {
            runtime, shared, ..
        } = self;
        let shared = &*shared;
        let mut call = Some((prompt, media, Prompt::Initial));
        let outcome = loop {
            let Some((prompt, media, kind)) = call.take() else {
                break Outcome::Completed;
            };
            lock(shared).next_call(kind);
            let before = runtime.session().messages.len();
            let turn = run_user_turn_streaming_events_with_media(
                runtime,
                provider.as_ref(),
                prompt,
                media,
                &control,
                |event| lock(shared).record(event),
            );
            let Some(result) = drive(turn, lines, shared).await else {
                return false;
            };
            let mut guard = lock(shared);
            guard.call_returned(runtime.session(), before);
            match result {
                Ok(_) => {
                    call = guard
                        .queue
                        .take_follow_ups()
                        .map(|prompt| (prompt, Vec::new(), Prompt::FollowUps));
                }
                Err(AgentError::Cancelled) => break Outcome::Aborted,
                Err(error) => break Outcome::Failed(error.to_string()),
            }
        };
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
        self.shared().forget_queue();
        Ok(Some(Data::Cancelled { cancelled: false }))
    }

    fn switch_session(&mut self, session_path: &str) -> Reply {
        let path = crate::resolve_session_reference(&self.config.paths.session_dir, session_path)
            .map_err(|error| error.to_string())?;
        let (store, session) = SessionStore::open(path).map_err(|error| error.to_string())?;
        self.runtime.replace_session(session, Some(store));
        self.shared().forget_queue();
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
            Ok(Command::Prompt(input)) => match input.streaming_behavior {
                Some(behavior) => self.enqueue(behavior, input),
                None => Err(BUSY.into()),
            },
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
            Ok(Command::ClearQueue) => Ok(Some(self.clear_queue())),
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
                Command::NewSession { .. }
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

    /// Queues a message, as steering or a follow-up.
    fn enqueue(&mut self, behavior: StreamingBehavior, input: &Input) -> Reply {
        self.fence(&input.expected_run_id)?;
        if !input.images.is_empty() {
            return Err(
                "queued messages are text only; send images with a prompt that starts a run".into(),
            );
        }
        let message = input.message.clone();
        match behavior {
            StreamingBehavior::Steer => self.queue.steering.send(message),
            StreamingBehavior::FollowUp => self.queue.follow_ups.push(message),
        }
        send(&self.queue.update());
        Ok(Some(Data::queued()))
    }

    fn clear_queue(&mut self) -> Data {
        let cleared = self.queue.clear();
        send(&self.queue.update());
        cleared
    }

    /// A new or switched-to session starts with an empty queue.
    fn forget_queue(&mut self) {
        if self.queue.pending() > 0 {
            self.clear_queue();
        }
    }

    /// The next engine call of the active run starts with `prompt`.
    fn next_call(&mut self, prompt: Prompt) {
        self.active().recorder.call(prompt);
    }

    /// An event from the active run's engine call.
    fn record(&mut self, event: &TurnEvent) {
        let Self { queue, run } = self;
        let Some(run) = run else {
            return;
        };
        run.mirror(event);
        run.recorder
            .record(event, queue, &mut |record| send(&record));
    }

    /// An engine call returned: report what it saved unreported and read
    /// the session afresh (a compaction rewrites it).
    fn call_returned(&mut self, session: &SessionState, before: usize) {
        let Self { queue, run } = self;
        let Some(run) = run else {
            return;
        };
        let added = session.messages.get(before..).unwrap_or_default();
        run.recorder
            .unreported(added, session, queue, &mut |record| send(&record));
        run.session = session.clone();
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
                    media: Vec::new(),
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
