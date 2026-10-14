//! `noel --mode rpc`: upstream pi's RPC protocol on stdin and stdout, plus
//! the extensions named in `get_state.rpcExtensions`. `run-id`: every run
//! has a `runId`, given in the prompt reply, `agent_start`, `agent_settled`
//! and `get_state` while it runs. `expected-run-id`: `steer`, `follow_up`
//! and `abort` take an `expectedRunId` and change nothing unless it names
//! the active run. `request-id`: a `prompt` that repeats a `requestId` gets
//! the run the first one started. `resume-run`: `get_state.lastRun` says
//! how the session's last run ended, and `resume_run` continues it if a
//! stop interrupted it.
//!
//! A `prompt`, `steer` or `follow_up` that is a skill command or a prompt
//! template expands when it arrives, as upstream's does: the queue and the
//! session hold the expansion.
//!
//! This module maps the protocol onto the session controller
//! ([`noel_core::controller`]), which drives the runs, keeps the queue and
//! saves both: commands in, records out. An abort keeps the queue, as
//! upstream's does. Only a `prompt`'s `requestId` is read, as before;
//! the protocol has no submission ids to report.
//!
//! One task reads commands and drives the active run, so records keep the
//! order things happen in. While a run is active, commands that change the
//! session are refused and reads answer from a copy of the session that the
//! run's events keep current. stdout carries only protocol records.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use base64::Engine;
use noel_ai::{MediaInput, ModelRef};
use noel_config::LoadedConfig;
use noel_core::controller::{
    AbortPolicy, Busy, CallInput, Enqueued, Event as ControllerEvent, Host, Image,
    SessionController, Start, Submission, SubmitError, Submitted, WhenBusy,
};
use noel_core::{
    expand_input, QueueItem, QueueMode, Runtime, SessionState, SessionStore, SubmissionKind,
    TurnEvent,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::rpc_wire::{
    queue_update, Command, Data, Event, Input, LastRun, Model, Prompt, Recorder, Reply, Request,
    Response, Source, State, StreamingBehavior, BUSY, QUEUED_REQUEST, RPC_EXTENSIONS,
};

type Lines = mpsc::UnboundedReceiver<String>;

/// Serves the protocol until stdin closes. EOF while idle exits; EOF during
/// a run stops it where it is, without settling it, and exits: the run
/// stays interrupted, for `resume_run`.
pub(crate) async fn run(runtime: Runtime, config: LoadedConfig, offline: bool) -> Result<()> {
    let mut lines = read_lines();
    let host = RpcHost {
        config,
        offline,
        source: None,
        run: None,
    };
    let mut controller = SessionController::new(runtime, host, AbortPolicy::KeepQueue);
    while let Some(line) = lines.recv().await {
        if let Some(start) = command(&mut controller, &line).await {
            if !controller.run(start, &mut lines, busy_command).await {
                // The stopped run's tools died with it; leave none behind.
                noel_core::kill_running_tool_groups();
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

/// Sends account failover's notices as upstream's notifications.
fn send_notices() {
    for notice in crate::account_failover::drain_notices() {
        send(&Event::warning(noel_core::controller::new_id(), &notice));
    }
}

/// Answers the command that starts `start`, before the run's records.
fn started(request: &Request, start: Start) -> Start {
    send(&Response::to(
        request,
        Ok(Some(Data::started(start.run_id()))),
    ));
    start
}

/// RPC mode's side of the controller: the configuration runs are made
/// with, and the records of the active run.
struct RpcHost {
    config: LoadedConfig,
    offline: bool,
    /// What the next run's replies name, found with its provider.
    source: Option<Source>,
    run: Option<RunRecords>,
}

/// The active run's records, and the commands they answer.
struct RunRecords {
    recorder: Recorder,
    /// What `get_state` reports while the run cannot change it.
    state: State,
    /// Aborts to answer once the run settles.
    aborts: Vec<Option<Value>>,
    /// `abort_retry` commands to answer once the retry they ended is
    /// reported as ended.
    retry_aborts: Vec<Option<Value>>,
}

impl RunRecords {
    fn answer_retry_aborts(&mut self) {
        for id in self.retry_aborts.drain(..) {
            send(&Response::new(id.as_ref(), Some("abort_retry"), Ok(None)));
        }
    }
}

impl RpcHost {
    fn active(&mut self) -> &mut RunRecords {
        self.run.as_mut().expect("a run is active")
    }
}

impl Host for RpcHost {
    /// A provider for the active model with a usable credential, and what
    /// the run's replies name. Each run gets its own provider, so
    /// `set_model` applies to the next run.
    async fn provider(&mut self, runtime: &Runtime) -> Result<Box<dyn noel_ai::Provider>, String> {
        let provider = crate::provider_for_runtime(runtime, &self.config, self.offline)
            .await
            .map_err(|error| error.to_string())?;
        // The provider exists, so the active model is configured.
        let model = model(runtime, &self.config).ok_or("the active model is not configured")?;
        self.source = Some(Source {
            api: model.api,
            provider: model.provider,
            model: model.id,
            thinking_level: thinking_level(runtime, &self.config),
        });
        Ok(provider)
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

    fn event(&mut self, event: ControllerEvent<'_>) {
        match event {
            ControllerEvent::RunStarted { run_id, runtime } => {
                let source = self.source.take().expect("a run starts with its provider");
                let mut state = state(runtime, &self.config, 0);
                state.is_streaming = true;
                self.run = Some(RunRecords {
                    recorder: Recorder::start(run_id.to_owned(), source, &mut |record| {
                        send(&record)
                    }),
                    state,
                    aborts: Vec::new(),
                    retry_aborts: Vec::new(),
                });
            }
            ControllerEvent::CallStarted { input } => self.active().recorder.call(match input {
                CallInput::Prompt => Some(Prompt::Initial),
                CallInput::FollowUps => Some(Prompt::FollowUps),
                CallInput::Resume => None,
            }),
            ControllerEvent::Turn { event, queue } => {
                send_notices();
                let run = self.active();
                run.recorder
                    .record(event, queue, &mut |record| send(&record));
                if matches!(event, TurnEvent::RetryFinished { .. }) {
                    run.answer_retry_aborts();
                }
            }
            ControllerEvent::CallReturned {
                added,
                session,
                queue,
            } => {
                let recorder = &mut self.active().recorder;
                recorder.unreported(added, session, queue, &mut |record| send(&record));
            }
            ControllerEvent::QueueChanged { queue } => send(&queue_update(queue)),
            ControllerEvent::Placed { .. } | ControllerEvent::Withdrawn { .. } => {}
            ControllerEvent::CompactionStarted => {
                send(&Event::CompactionStart { reason: "manual" })
            }
            ControllerEvent::CompactionEnded { result } => send(&Event::CompactionEnd {
                reason: "manual",
                result: result.ok().map(Into::into),
                aborted: false,
                will_retry: false,
                error_message: result.err(),
            }),
            ControllerEvent::SaveFailed { error } => eprintln!("noel: {error}"),
            ControllerEvent::RunSettled { outcome, .. } => {
                let Some(mut run) = self.run.take() else {
                    return;
                };
                run.answer_retry_aborts();
                send_notices();
                run.recorder.finish(outcome, &mut |record| send(&record));
                for id in run.aborts {
                    send(&Response::new(id.as_ref(), Some("abort"), Ok(None)));
                }
            }
        }
    }
}

/// A `prompt`, `steer` or `follow_up` as a submission. RPC names runs by a
/// prompt's `requestId`; a queued message's is not read.
fn submission(input: &Input, kind: SubmissionKind) -> Submission {
    Submission {
        kind,
        request_id: input
            .request_id
            .clone()
            .filter(|_| kind == SubmissionKind::Prompt),
        content: input.message.clone(),
        images: input
            .images
            .iter()
            .map(|image| Image {
                data: image.data.clone(),
                mime_type: image.mime_type.clone(),
            })
            .collect(),
        expected_run_id: input.expected_run_id.clone(),
    }
}

/// What a prompt sent during a run does.
fn when_busy(input: &Input) -> WhenBusy {
    match input.streaming_behavior {
        None => WhenBusy::Reject,
        Some(StreamingBehavior::Steer) => WhenBusy::Steer,
        Some(StreamingBehavior::FollowUp) => WhenBusy::FollowUp,
    }
}

/// The reply to a refused submission, in upstream's words where it has
/// them.
fn refusal(error: SubmitError) -> String {
    match error {
        SubmitError::Busy => BUSY.into(),
        SubmitError::QueuedPrompt => QUEUED_REQUEST.into(),
        error => error.to_string(),
    }
}

/// The reply to a queued message.
fn enqueued(result: Result<Enqueued, SubmitError>) -> Reply {
    match result.map_err(refusal)? {
        Enqueued::Queued(_) => Ok(Some(Data::queued())),
        Enqueued::Repeated(repeat) => Ok(Some(Data::repeated(&repeat))),
    }
}

/// `clear_queue`'s reply: what was queued.
fn cleared(result: Result<Vec<QueueItem>, String>) -> Reply {
    let (steering, follow_up): (Vec<_>, Vec<_>) = result?
        .into_iter()
        .partition(|item| item.id.mode == QueueMode::Steer);
    let texts = |items: Vec<QueueItem>| items.into_iter().map(|item| item.content).collect();
    Ok(Some(Data::Queue {
        steering: texts(steering),
        follow_up: texts(follow_up),
    }))
}

/// Handles a command while no run is active; a prompt that starts a run,
/// or a `resume_run`, is returned to run.
async fn command(controller: &mut SessionController<RpcHost>, line: &str) -> Option<Start> {
    let request = match Request::parse(line) {
        Ok(request) => request,
        Err(error) => {
            send(&Response::parse_error(error));
            return None;
        }
    };
    let reply = match &request.command {
        Err(error) => Err(error.clone()),
        Ok(Command::Prompt(input)) => {
            match controller
                .submit(submission(input, SubmissionKind::Prompt))
                .await
            {
                Ok(Submitted::Started(start)) => return Some(started(&request, start)),
                Ok(Submitted::Repeated(repeat)) => Ok(Some(Data::repeated(&repeat))),
                Err(error) => Err(refusal(error)),
            }
        }
        Ok(Command::ResumeRun { expected_run_id }) => {
            match controller.resume(expected_run_id).await {
                Ok(start) => return Some(started(&request, start)),
                Err(error) => Err(error),
            }
        }
        Ok(Command::Steer(input)) => {
            enqueued(controller.enqueue(QueueMode::Steer, submission(input, SubmissionKind::Steer)))
        }
        Ok(Command::FollowUp(input)) => enqueued(controller.enqueue(
            QueueMode::FollowUp,
            submission(input, SubmissionKind::FollowUp),
        )),
        Ok(Command::Abort { expected_run_id }) => {
            controller.abort(expected_run_id.as_deref()).map(|()| None)
        }
        // No run, so no retry is waiting.
        Ok(Command::AbortRetry) => Ok(None),
        Ok(Command::SetAutoRetry { enabled }) => {
            controller.configure(|runtime, _| runtime.set_retry_enabled(*enabled));
            Ok(None)
        }
        Ok(Command::ClearQueue) => cleared(controller.clear_queue()),
        Ok(Command::NewSession { parent_session }) => {
            new_session(controller, parent_session.as_deref())
        }
        Ok(Command::SwitchSession { session_path }) => switch_session(controller, session_path),
        Ok(Command::GetState) => {
            let pending = controller.queue().pending();
            let (runtime, host) = controller.parts();
            Ok(Some(Data::State(Box::new(state(
                runtime,
                &host.config,
                pending,
            )))))
        }
        Ok(Command::GetMessages) => Ok(Some(Data::messages(controller.runtime().session()))),
        Ok(Command::GetLastAssistantText) => Ok(Some(Data::last_assistant_text(
            controller.runtime().session(),
        ))),
        Ok(Command::GetCommands) => Ok(Some(Data::commands(&controller.host().config))),
        Ok(Command::SetModel { provider, model_id }) => {
            controller.configure(|runtime, host| set_model(runtime, host, provider, model_id))
        }
        Ok(Command::SetThinkingLevel { level }) => {
            controller.configure(|runtime, host| set_thinking_level(runtime, host, level))
        }
        Ok(Command::SetSteeringMode { mode } | Command::SetFollowUpMode { mode }) => mode.reply(),
        Ok(Command::Compact {
            custom_instructions,
        }) => controller
            .compact(custom_instructions.as_deref())
            .await
            .map(|record| Some(Data::Compaction((&record).into()))),
        Ok(Command::SetSessionName { name }) => {
            controller.configure(|runtime, _| set_session_name(runtime, name))
        }
        Ok(Command::ExtensionUiResponse) => return None,
        Ok(Command::Other) => request.unknown(),
    };
    send_notices();
    send(&Response::to(&request, reply));
    None
}

/// Handles a command while a run is active.
fn busy_command(line: String, busy: &mut Busy<'_, RpcHost>) {
    let request = match Request::parse(&line) {
        Ok(request) => request,
        Err(error) => return send(&Response::parse_error(error)),
    };
    let reply = match &request.command {
        Err(error) => Err(error.clone()),
        Ok(Command::Prompt(input)) => {
            enqueued(busy.submit(submission(input, SubmissionKind::Prompt), when_busy(input)))
        }
        Ok(Command::Steer(input)) => {
            enqueued(busy.enqueue(QueueMode::Steer, submission(input, SubmissionKind::Steer)))
        }
        Ok(Command::FollowUp(input)) => enqueued(busy.enqueue(
            QueueMode::FollowUp,
            submission(input, SubmissionKind::FollowUp),
        )),
        Ok(Command::Abort { expected_run_id }) => match busy.abort(expected_run_id.as_deref()) {
            // Answered once the run settles.
            Ok(()) => {
                busy.host().active().aborts.push(request.id.clone());
                return;
            }
            Err(error) => Err(error),
        },
        Ok(Command::AbortRetry) => {
            if busy.abort_retry() {
                // Answered once the retry is reported as ended.
                busy.host().active().retry_aborts.push(request.id.clone());
                return;
            }
            Ok(None)
        }
        Ok(Command::ClearQueue) => cleared(busy.clear_queue()),
        Ok(Command::GetState) => {
            let pending_message_count = busy.queue().pending();
            let message_count = busy.session().messages.len();
            let run = busy.host().active();
            Ok(Some(Data::State(Box::new(State {
                is_compacting: run.recorder.compacting(),
                message_count,
                pending_message_count,
                run_id: Some(run.recorder.id().to_owned()),
                ..run.state.clone()
            }))))
        }
        Ok(Command::GetMessages) => Ok(Some(Data::messages(busy.session()))),
        Ok(Command::GetLastAssistantText) => Ok(Some(Data::last_assistant_text(busy.session()))),
        Ok(Command::GetCommands) => Ok(Some(Data::commands(&busy.host().config))),
        Ok(Command::SetSteeringMode { mode } | Command::SetFollowUpMode { mode }) => mode.reply(),
        Ok(
            Command::ResumeRun { .. }
            | Command::SetAutoRetry { .. }
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
        Ok(Command::ExtensionUiResponse) => return,
        Ok(Command::Other) => request.unknown(),
    };
    send(&Response::to(&request, reply));
}

/// The `get_state` fields while no run is active.
fn state(runtime: &Runtime, config: &LoadedConfig, pending_message_count: usize) -> State {
    let session = runtime.session();
    State {
        model: model(runtime, config),
        thinking_level: thinking_level(runtime, config),
        is_streaming: false,
        is_compacting: false,
        steering_mode: "all",
        follow_up_mode: "all",
        session_file: runtime
            .store()
            .map(|store| store.path().display().to_string()),
        session_id: session.session_id.clone(),
        session_name: session.name.clone(),
        auto_compaction_enabled: runtime.systems().compaction.enabled,
        message_count: session.messages.len(),
        pending_message_count,
        rpc_extensions: RPC_EXTENSIONS,
        run_id: None,
        last_run: LastRun::of(runtime),
    }
}

/// The active model as upstream's model object.
fn model(runtime: &Runtime, config: &LoadedConfig) -> Option<Model> {
    let model = runtime.session().active_model.as_ref()?;
    Model::configured(config, model)
}

/// The thinking level in effect: clamped to the model, `off` for a model
/// without reasoning.
fn thinking_level(runtime: &Runtime, config: &LoadedConfig) -> String {
    crate::effective_thinking_level(runtime, config).to_owned()
}

/// Reports a change of the level in effect since `before`.
fn report_thinking_level(runtime: &Runtime, config: &LoadedConfig, before: &str) {
    let after = thinking_level(runtime, config);
    if after != before {
        send(&Event::ThinkingLevelChanged { level: &after });
    }
}

fn new_session(controller: &mut SessionController<RpcHost>, parent_session: Option<&str>) -> Reply {
    if parent_session.is_some() {
        return Err("new_session does not support parentSession".into());
    }
    controller.replace_session(|runtime, host| {
        if runtime.store().is_some() {
            crate::start_new_session(runtime, &host.config).map_err(|e| e.to_string())?;
        } else {
            // Without a journal (`--no-session`), the same reset in memory.
            let current = runtime.session();
            let mut session = SessionState::new(current.session_id.clone(), current.cwd.clone());
            session.active_model = current.active_model.clone();
            session.active_thinking_level = current.active_thinking_level.clone();
            session.active_account = current.active_account.clone();
            session.workspace = current.workspace.clone();
            runtime.replace_session(session, None);
        }
        Ok(Some(Data::Cancelled { cancelled: false }))
    })
}

fn switch_session(controller: &mut SessionController<RpcHost>, session_path: &str) -> Reply {
    controller.replace_session(|runtime, host| {
        let path = crate::resolve_session_reference(&host.config.paths.session_dir, session_path)
            .map_err(|error| error.to_string())?;
        let (store, session) = SessionStore::open(path).map_err(|error| error.to_string())?;
        runtime.replace_session(session, Some(store));
        Ok(Some(Data::Cancelled { cancelled: false }))
    })
}

/// Selects the session's model, as upstream does: one of the models that
/// can run now, by exact provider and id. The thinking level follows the
/// switch (`switch_model`). Unlike the TUI's `/model`, the default model in
/// settings stays as it is.
fn set_model(runtime: &mut Runtime, host: &RpcHost, provider: &str, model_id: &str) -> Reply {
    let model = ModelRef {
        provider: provider.to_owned(),
        id: model_id.to_owned(),
    };
    let info = Some(&model)
        .filter(|model| crate::provider_usable(&host.config, &model.provider, host.offline))
        .and_then(|model| crate::configured_model(&host.config, model))
        .map(Model::from)
        .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
    let before = thinking_level(runtime, &host.config);
    crate::switch_model(runtime, &host.config, &model, None).map_err(|error| error.to_string())?;
    report_thinking_level(runtime, &host.config, &before);
    Ok(Some(Data::Model(Box::new(info))))
}

/// Sets the thinking level, clamped to what the model supports as upstream
/// clamps it.
fn set_thinking_level(runtime: &mut Runtime, host: &RpcHost, level: &str) -> Reply {
    let level = crate::normalized_thinking_level(level)
        .ok_or_else(|| format!("Unknown thinking level: {level}"))?;
    let before = thinking_level(runtime, &host.config);
    crate::set_thinking_level(runtime, &host.config, level).map_err(|error| error.to_string())?;
    report_thinking_level(runtime, &host.config, &before);
    Ok(None)
}

fn set_session_name(runtime: &mut Runtime, name: &str) -> Reply {
    let name = name.trim();
    if name.is_empty() {
        return Err("Session name cannot be empty".into());
    }
    runtime
        .rename_session(Some(name.to_owned()))
        .map_err(|error| error.to_string())?;
    send(&Event::SessionInfoChanged { name });
    Ok(None)
}
