//! The session controller (spec §9): one driver for the runs of one open
//! session, for every mode that drives runs by command. RPC mode is one
//! caller; the Mode 2 worker is the next.
//!
//! The controller owns the active run (its cancellation and its retry
//! backoff), the queue, and the submissions that reach the session:
//!
//! - **Submissions** (spec §3.1). A prompt, steer or follow-up may carry a
//!   `requestId`. A repeat returns the first submission and writes nothing,
//!   and a `requestId` used for another kind is refused; both are decided
//!   within the session, before any write. While a run is active a
//!   submission says what to do ([`WhenBusy`]): be refused, or queue as
//!   steering or as a follow-up.
//! - **The queue.** Each queued submission has its own id. An enqueue is
//!   saved before it is acknowledged; a submission with a `requestId` is
//!   saved with its identity, and the message that places it into a run
//!   saves its placement (see [`QueueItemId`]).
//! - **Withdraw** (spec §3.2) and **abort** (spec §3.3), whose effect on the
//!   queue each caller chooses ([`AbortPolicy`]).
//! - Resuming an interrupted run, compaction, and changes to the session's
//!   model and thinking level, which wait for no run to be active.
//!
//! It reports what happens as [`Event`]s, in order, to its [`Host`]. RPC
//! mode maps them to upstream pi's records.
//!
//! One task drives a run and handles what arrives meanwhile
//! ([`SessionController::run`]), so events keep the order things happen
//! in. While a run is active, its engine call holds the runtime: what
//! arrives is handled through [`Busy`], which reads the session from a copy
//! the run's events keep current.

use std::fmt;
use std::future::Future;
use std::sync::{Mutex, MutexGuard};

use noel_ai::{MediaInput, Provider};
use tokio::sync::mpsc;

use crate::{
    continue_turn_streaming_events, run_saved_prompt_streaming_events, user_message, AgentError,
    CancellationToken, CompactionKind, CompactionRecord, ConversationMessage, MessageRole,
    PastSubmission, QueueItem, QueueItemId, QueueMode, RetryAbort, Run, RunControl, RunStart,
    RunStatus, Runtime, SessionError, SessionState, SessionStore, SteeringMailbox, SubmissionKind,
    ToolEvent, TurnEvent, STEERING_JOIN,
};

/// What a mode supplies to the controller, and where its events go.
pub trait Host: Send {
    /// A provider for the session's active model with a usable credential,
    /// for one run or one compaction; `Err` refuses it.
    fn provider(
        &mut self,
        runtime: &Runtime,
    ) -> impl Future<Output = Result<Box<dyn Provider>, String>> + Send;

    /// A prompt's images as the run sends them; `Err` refuses the prompt.
    fn media(&self, images: &[Image]) -> Result<Vec<MediaInput>, String>;

    /// Typed input as it is placed: a skill command or a prompt template
    /// expanded ([`crate::expand_input`]).
    fn expand(&self, text: &str) -> String;

    /// What happened, in order.
    fn event(&mut self, event: Event<'_>);
}

/// An image of a submission, as pi's protocols carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Base64.
    pub data: String,
    pub mime_type: String,
}

/// Input for the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub kind: SubmissionKind,
    pub request_id: Option<String>,
    pub content: String,
    pub images: Vec<Image>,
    /// When it is queued: the run it is meant for. Nothing is queued
    /// unless that run is the active one.
    pub expected_run_id: Option<String>,
}

/// What a submission does while a run is active (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenBusy {
    /// Refused, and nothing is written.
    Reject,
    /// Queued for the run's next provider round.
    Steer,
    /// Queued for when the run would answer.
    FollowUp,
}

/// What an abort does to the queue (spec §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortPolicy {
    /// Published pi's rule, for RPC mode, the TUI and print mode: an abort
    /// stops the run and keeps the queue.
    KeepQueue,
    /// Durable's rule, for Mode 2: an abort also withdraws everything
    /// queued, unanswered.
    WithdrawQueue,
}

/// An idle submission's outcome.
pub enum Submitted {
    /// It started a run: answer it, then [`SessionController::run`] it.
    Started(Start),
    /// Its `requestId` names an earlier submission; nothing was written.
    Repeated(Repeat),
}

/// The outcome of a submission that is queued, or would be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enqueued {
    /// Queued, and saved.
    Queued(QueueItemId),
    /// Its `requestId` names an earlier submission; nothing was written.
    Repeated(Repeat),
}

/// The earlier submission a repeated `requestId` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repeat {
    /// For a submission that started a run, the run's id.
    pub submission_id: String,
    pub kind: SubmissionKind,
    pub state: SubmissionState,
}

/// Where a submission is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionState {
    Queued(QueueMode),
    /// Placed into run `run_id`, which has not ended: it is running, or a
    /// stop interrupted it.
    Placed {
        run_id: String,
    },
    /// Placed into run `run_id`, which ended with `status`: answered if it
    /// completed, unanswered otherwise.
    Settled {
        run_id: String,
        status: RunStatus,
    },
    /// Withdrawn from the queue, unanswered.
    Withdrawn,
}

/// Why a submission was refused. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitError {
    /// A run is active, and the submission said to reject.
    Busy,
    /// A prompt with a `requestId` cannot be queued: send it when no run
    /// is active.
    QueuedPrompt,
    /// The `requestId` names a submission of another kind.
    Reused {
        request_id: String,
        kind: SubmissionKind,
    },
    Refused(String),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("a run is active"),
            Self::QueuedPrompt => formatter.write_str(
                "a prompt with a requestId cannot be queued; send it when no run is active",
            ),
            Self::Reused { request_id, kind } => write!(
                formatter,
                "requestId {request_id} names a {} submission",
                kind.as_str()
            ),
            Self::Refused(reason) => formatter.write_str(reason),
        }
    }
}

/// What withdrawing a submission found (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// It was queued: it left the queue unanswered, with reason `aborted`.
    Withdrawn,
    /// It is in a run that has not ended.
    AlreadyPlaced,
    /// It has ended.
    Settled,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Aborted,
    Failed(String),
}

impl Outcome {
    pub fn status(&self) -> RunStatus {
        match self {
            Outcome::Completed => RunStatus::Completed,
            Outcome::Aborted => RunStatus::Aborted,
            Outcome::Failed(_) => RunStatus::Failed,
        }
    }
}

/// What an engine call of a run starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallInput {
    /// The submission that started the run, saved with its start.
    Prompt,
    /// Follow-ups taken from the queue.
    FollowUps,
    /// The saved context of an interrupted run.
    Resume,
}

/// What the controller reports, in order.
pub enum Event<'a> {
    /// A run starts, or an interrupted one continues. Its first engine call
    /// follows.
    RunStarted {
        run_id: &'a str,
        runtime: &'a Runtime,
    },
    /// An engine call of the active run starts.
    CallStarted { input: CallInput },
    /// What the engine reported, with the queue as it is now: steering
    /// and follow-ups leave it as they are placed.
    Turn {
        event: &'a TurnEvent,
        queue: &'a Queue,
    },
    /// An engine call returned: `added` are the messages it saved to
    /// `session`, reported or not.
    CallReturned {
        added: &'a [ConversationMessage],
        session: &'a SessionState,
        queue: &'a Queue,
    },
    /// Queued submissions were placed into the active run.
    Placed {
        run_id: &'a str,
        submissions: &'a [QueueItemId],
    },
    /// Queued submissions were withdrawn, unanswered.
    Withdrawn { submissions: &'a [QueueItem] },
    /// The queue changed other than by placing into the run: an enqueue,
    /// a withdrawal, or another session's queue.
    QueueChanged { queue: &'a Queue },
    /// A compaction asked for while no run is active.
    CompactionStarted,
    CompactionEnded {
        result: Result<&'a CompactionRecord, &'a str>,
    },
    /// A save the controller could not report as a refusal failed: how a
    /// run ended, or the queue after a placement.
    SaveFailed { error: &'a SessionError },
    /// The run settled; how it ended is saved.
    RunSettled {
        run_id: &'a str,
        outcome: &'a Outcome,
    },
}

/// Queued submissions: steering, then follow-ups.
#[derive(Debug, Default)]
pub struct Queue {
    /// Shared with the active run, whose provider rounds take from it.
    steering: SteeringMailbox,
    follow_ups: Vec<QueueItem>,
}

impl Queue {
    pub fn pending(&self) -> usize {
        self.steering.pending() + self.follow_ups.len()
    }

    /// The steering's texts.
    pub fn steering(&self) -> Vec<String> {
        self.steering.list()
    }

    /// The follow-ups' texts.
    pub fn follow_ups(&self) -> Vec<String> {
        let texts = self.follow_ups.iter().map(|item| item.content.clone());
        texts.collect()
    }

    /// Every queued submission, steering first.
    pub fn items(&self) -> Vec<QueueItem> {
        let mut items = self.steering.items();
        items.extend(self.follow_ups.iter().cloned());
        items
    }

    fn texts(&self) -> (Vec<String>, Vec<String>) {
        (self.steering(), self.follow_ups())
    }

    /// Makes `items` the queue. The steering mailbox stays the one the
    /// active run reads.
    fn replace(&mut self, items: Vec<QueueItem>) {
        self.steering.drain();
        self.follow_ups.clear();
        for item in items {
            match item.id.mode {
                QueueMode::Steer => self.steering.push(item),
                QueueMode::FollowUp => self.follow_ups.push(item),
            }
        }
    }
}

/// A run accepted to start, or to continue: answer the command that asked
/// for it, then [`SessionController::run`] it.
pub struct Start {
    run_id: String,
    provider: Box<dyn Provider>,
    call: Call,
}

impl Start {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
}

/// What an engine call of a run starts from.
enum Call {
    /// The prompt that started the run, saved with its start.
    Prompt(ConversationMessage),
    /// Follow-ups taken from the queue, saved as the call starts.
    FollowUps {
        message: ConversationMessage,
        placed: Vec<QueueItemId>,
    },
    /// The saved context of an interrupted run.
    Resume,
}

impl Call {
    fn input(&self) -> CallInput {
        match self {
            Call::Prompt(_) => CallInput::Prompt,
            Call::FollowUps { .. } => CallInput::FollowUps,
            Call::Resume => CallInput::Resume,
        }
    }
}

/// One open session's driver.
pub struct SessionController<H> {
    runtime: Runtime,
    /// Shared with the event callback of the active run's engine call.
    live: Mutex<Live<H>>,
}

/// What both the command handling and the engine's events use. One task
/// runs both, so the lock is never contended.
struct Live<H> {
    host: H,
    policy: AbortPolicy,
    /// The queue lives here; the session's copy is read when it opens.
    queue: Queue,
    /// Where the queue is saved (none without a journal), and what was
    /// saved last.
    store: Option<SessionStore>,
    saved: Vec<QueueItem>,
    /// Submissions that left the queue, oldest first.
    past: Vec<PastSubmission>,
    run: Option<ActiveRun>,
}

/// The active run.
struct ActiveRun {
    id: String,
    cancellation: CancellationToken,
    retry: RetryAbort,
    /// The session as the run has left it so far, for reads.
    session: SessionState,
}

impl<H: Host> SessionController<H> {
    /// Takes over `runtime`'s session, and its saved queue.
    pub fn new(runtime: Runtime, host: H, policy: AbortPolicy) -> Self {
        let mut live = Live {
            host,
            policy,
            queue: Queue::default(),
            store: None,
            saved: Vec::new(),
            past: Vec::new(),
            run: None,
        };
        live.open(&runtime);
        Self {
            runtime,
            live: Mutex::new(live),
        }
    }

    fn live(&mut self) -> &mut Live<H> {
        self.live.get_mut().expect("controller state")
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn host(&mut self) -> &mut H {
        &mut self.live().host
    }

    pub fn queue(&mut self) -> &Queue {
        &self.live().queue
    }

    /// The runtime and the host together, to read both.
    pub fn parts(&mut self) -> (&Runtime, &mut H) {
        let host = &mut self.live.get_mut().expect("controller state").host;
        (&self.runtime, host)
    }

    /// Changes the session (its model, thinking level, name or settings)
    /// while no run is active.
    pub fn configure<T>(&mut self, change: impl FnOnce(&mut Runtime, &mut H) -> T) -> T {
        let host = &mut self.live.get_mut().expect("controller state").host;
        change(&mut self.runtime, host)
    }

    /// Opens another session in the runtime with `open`; its saved queue
    /// becomes the queue.
    pub fn replace_session<T, E>(
        &mut self,
        open: impl FnOnce(&mut Runtime, &mut H) -> Result<T, E>,
    ) -> Result<T, E> {
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        let opened = open(runtime, &mut live.host)?;
        if live.open(runtime) {
            live.host.event(Event::QueueChanged { queue: &live.queue });
        }
        Ok(opened)
    }

    /// A submission while no run is active: a repeated `requestId` gets
    /// its first submission, and anything else starts a run, saved with
    /// its prompt. Steering and follow-ups still queued join that run.
    pub async fn submit(&mut self, submission: Submission) -> Result<Submitted, SubmitError> {
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        if let Some(repeat) = live.repeat(&submission, &runtime.session().runs)? {
            return Ok(Submitted::Repeated(repeat));
        }
        let media = live
            .host
            .media(&submission.images)
            .map_err(SubmitError::Refused)?;
        let provider = live
            .host
            .provider(runtime)
            .await
            .map_err(SubmitError::Refused)?;
        let run = RunStart {
            id: new_id(),
            request_id: submission.request_id,
            kind: submission.kind,
        };
        let run_id = run.id.clone();
        let content = live.host.expand(&submission.content);
        let prompt = runtime
            .start_run(run, content, media)
            .map_err(|error| SubmitError::Refused(error.to_string()))?;
        Ok(Submitted::Started(Start {
            run_id,
            provider,
            call: Call::Prompt(prompt),
        }))
    }

    /// Queues a submission as `mode` though no run is active; the next run
    /// takes it.
    pub fn enqueue(
        &mut self,
        mode: QueueMode,
        submission: Submission,
    ) -> Result<Enqueued, SubmitError> {
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        live.enqueue(mode, submission, &runtime.session().runs)
    }

    /// Withdraws a queued submission (spec §3.2).
    pub fn withdraw(&mut self, submission_id: &str) -> Result<Withdrawal, String> {
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        live.withdraw(submission_id, &runtime.session().runs)
    }

    /// Withdraws everything queued, and returns it.
    pub fn clear_queue(&mut self) -> Result<Vec<QueueItem>, String> {
        self.live().clear_queue()
    }

    /// An abort while no run is active ends an interrupted last run as
    /// aborted, so it is never resumed. `expected_run_id`, when given,
    /// must name that run.
    pub fn abort(&mut self, expected_run_id: Option<&str>) -> Result<(), String> {
        let interrupted = self
            .runtime
            .session()
            .runs
            .last()
            .filter(|run| run.status == RunStatus::Interrupted)
            .map(|run| run.id.clone());
        match (interrupted, expected_run_id) {
            (Some(run_id), Some(expected)) if run_id != expected => {
                return Err(format!(
                    "expectedRunId {expected} does not match the interrupted run {run_id}"
                ))
            }
            (Some(run_id), _) => self
                .runtime
                .finish_run(&run_id, RunStatus::Aborted)
                .map_err(|error| error.to_string())?,
            (None, _) => fence(expected_run_id, None)?,
        }
        let live = self.live();
        if live.policy == AbortPolicy::WithdrawQueue {
            live.clear_queue()?;
        }
        Ok(())
    }

    /// Accepts continuing the session's last run, if a stop interrupted it.
    pub async fn resume(&mut self, expected_run_id: &str) -> Result<Start, String> {
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
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        let provider = live.host.provider(runtime).await?;
        runtime.resume_run(expected_run_id);
        Ok(Start {
            run_id: expected_run_id.to_owned(),
            provider,
            call: Call::Resume,
        })
    }

    /// Compacts the session now.
    pub async fn compact(&mut self, focus: Option<&str>) -> Result<CompactionRecord, String> {
        let Self { runtime, live } = self;
        let live = live.get_mut().expect("controller state");
        let provider = live.host.provider(runtime).await?;
        live.host.event(Event::CompactionStarted);
        let result = runtime
            .compact_messages(
                provider.as_ref(),
                CompactionKind::Manual,
                focus.unwrap_or(""),
            )
            .await
            .map_err(|error| error.to_string());
        live.host.event(Event::CompactionEnded {
            result: result.as_ref().map_err(String::as_str),
        });
        result
    }

    /// Drives a run until it settles: its first engine call, then one per
    /// batch of follow-ups queued meanwhile. What arrives in `inbox`
    /// meanwhile is handled by `handle`, through [`Busy`]. How the run
    /// ended is saved before [`Event::RunSettled`].
    ///
    /// Returns `false` if `inbox` closed during the run: the run stops
    /// where it is, unsettled, and stays interrupted for a later resume.
    /// The controller should then be dropped.
    pub async fn run<T>(
        &mut self,
        start: Start,
        inbox: &mut mpsc::UnboundedReceiver<T>,
        mut handle: impl FnMut(T, &mut Busy<'_, H>),
    ) -> bool {
        let Start {
            run_id,
            provider,
            call,
        } = start;
        let Self { runtime, live } = self;
        let control = {
            let live = live.get_mut().expect("controller state");
            let control = RunControl {
                steering: live.queue.steering.clone(),
                run_id: Some(run_id.clone()),
                ..RunControl::default()
            };
            let mut session = runtime.session().clone();
            if matches!(call, Call::Prompt(_)) {
                // The engine reports the saved prompt, and the copy takes it
                // then.
                session.messages.pop();
            }
            live.run = Some(ActiveRun {
                id: run_id.clone(),
                cancellation: control.cancellation.clone(),
                retry: control.retry.clone(),
                session,
            });
            live.host.event(Event::RunStarted {
                run_id: &run_id,
                runtime: &*runtime,
            });
            control
        };
        let live = &*live;
        let mut next = Some(call);
        let outcome = loop {
            let Some(call) = next.take() else {
                break Outcome::Completed;
            };
            lock(live).host.event(Event::CallStarted {
                input: call.input(),
            });
            // Where the messages this call saves start, for the records of
            // those it does not report; a saved prompt is one of them.
            let before =
                runtime.session().messages.len() - usize::from(matches!(call, Call::Prompt(_)));
            let on_event = |event: &TurnEvent| lock(live).turn(event);
            let turn = async {
                let provider = provider.as_ref();
                match call {
                    Call::Prompt(prompt) => {
                        run_saved_prompt_streaming_events(
                            runtime, provider, prompt, &control, on_event,
                        )
                        .await
                    }
                    Call::FollowUps { message, placed } => {
                        runtime.push_placed_message(message.clone(), placed)?;
                        run_saved_prompt_streaming_events(
                            runtime, provider, message, &control, on_event,
                        )
                        .await
                    }
                    Call::Resume => {
                        continue_turn_streaming_events(runtime, provider, &control, on_event).await
                    }
                }
            };
            let Some(result) = drive(turn, inbox, live, &mut handle).await else {
                return false;
            };
            let mut live = lock(live);
            live.call_returned(runtime.session(), before);
            match result {
                Ok(_) => next = live.take_follow_ups(),
                Err(AgentError::Cancelled) => break Outcome::Aborted,
                Err(error) => break Outcome::Failed(error.to_string()),
            }
        };
        if let Err(error) = runtime.finish_run(&run_id, outcome.status()) {
            lock(live).host.event(Event::SaveFailed { error: &error });
        }
        lock(live).settle(&run_id, &outcome);
        true
    }
}

/// The controller while a run is active: what may change meanwhile.
pub struct Busy<'a, H> {
    live: &'a mut Live<H>,
}

impl<H: Host> Busy<'_, H> {
    fn active(&self) -> &ActiveRun {
        self.live.run.as_ref().expect("a run is active")
    }

    pub fn run_id(&self) -> &str {
        &self.active().id
    }

    /// The session as the run has left it so far.
    pub fn session(&self) -> &SessionState {
        &self.active().session
    }

    pub fn queue(&self) -> &Queue {
        &self.live.queue
    }

    pub fn host(&mut self) -> &mut H {
        &mut self.live.host
    }

    /// The runs a submission or a withdrawal is looked up in.
    fn runs(&self) -> Vec<Run> {
        self.session().runs.clone()
    }

    /// A submission while the run is active: a repeated `requestId` gets
    /// its first submission; anything else does what `when_busy` says.
    pub fn submit(
        &mut self,
        submission: Submission,
        when_busy: WhenBusy,
    ) -> Result<Enqueued, SubmitError> {
        let runs = self.runs();
        if let Some(repeat) = self.live.repeat(&submission, &runs)? {
            return Ok(Enqueued::Repeated(repeat));
        }
        let mode = match when_busy {
            WhenBusy::Reject => return Err(SubmitError::Busy),
            WhenBusy::Steer => QueueMode::Steer,
            WhenBusy::FollowUp => QueueMode::FollowUp,
        };
        self.live.enqueue(mode, submission, &runs)
    }

    /// Queues a submission as `mode`.
    pub fn enqueue(
        &mut self,
        mode: QueueMode,
        submission: Submission,
    ) -> Result<Enqueued, SubmitError> {
        let runs = self.runs();
        self.live.enqueue(mode, submission, &runs)
    }

    pub fn withdraw(&mut self, submission_id: &str) -> Result<Withdrawal, String> {
        let runs = self.runs();
        self.live.withdraw(submission_id, &runs)
    }

    pub fn clear_queue(&mut self) -> Result<Vec<QueueItem>, String> {
        self.live.clear_queue()
    }

    /// Stops the run, which then settles as aborted. `expected_run_id`,
    /// when given, must name it. The policy decides what happens to the
    /// queue.
    pub fn abort(&mut self, expected_run_id: Option<&str>) -> Result<(), String> {
        fence(expected_run_id, Some(self.run_id()))?;
        if self.live.policy == AbortPolicy::WithdrawQueue {
            if let Err(error) = self.live.clear_queue_saving() {
                self.live.host.event(Event::SaveFailed { error: &error });
            }
        }
        self.active().cancellation.cancel();
        Ok(())
    }

    /// Ends the backoff of a retry the run is waiting out; `false` if
    /// there is none.
    pub fn abort_retry(&mut self) -> bool {
        self.active().retry.abort()
    }
}

/// Polls the engine call until it ends, handling what arrives meanwhile.
/// `None` if `inbox` closed first.
async fn drive<O, T, H: Host>(
    turn: impl Future<Output = O>,
    inbox: &mut mpsc::UnboundedReceiver<T>,
    live: &Mutex<Live<H>>,
    handle: &mut impl FnMut(T, &mut Busy<'_, H>),
) -> Option<O> {
    tokio::pin!(turn);
    loop {
        tokio::select! {
            biased;
            result = &mut turn => return Some(result),
            item = inbox.recv() => {
                let mut live = lock(live);
                handle(item?, &mut Busy { live: &mut *live });
            }
        }
    }
}

fn lock<H>(live: &Mutex<Live<H>>) -> MutexGuard<'_, Live<H>> {
    live.lock().expect("controller state")
}

/// `expected_run_id`, when given, must name the active run.
fn fence(expected_run_id: Option<&str>, active: Option<&str>) -> Result<(), String> {
    let Some(expected) = expected_run_id else {
        return Ok(());
    };
    match active {
        Some(active) if active == expected => Ok(()),
        Some(active) => Err(format!(
            "expectedRunId {expected} does not match the active run {active}"
        )),
        None => Err(format!(
            "expectedRunId {expected} does not match: no run is active"
        )),
    }
}

/// Where a submission placed into run `run_id` is now.
fn placed_state(run_id: &str, runs: &[Run]) -> SubmissionState {
    match runs.iter().rev().find(|run| run.id == run_id) {
        Some(run) if run.status.is_finished() => SubmissionState::Settled {
            run_id: run.id.clone(),
            status: run.status,
        },
        _ => SubmissionState::Placed {
            run_id: run_id.to_owned(),
        },
    }
}

impl<H: Host> Live<H> {
    /// Takes the queue saved with the session `runtime` has open, and
    /// whether that changed what is queued. Submissions saved without an
    /// identity get one now.
    fn open(&mut self, runtime: &Runtime) -> bool {
        let session = runtime.session();
        let items = session.saved_queue_items().into_iter();
        let items = items
            .map(|(mode, content, id)| QueueItem {
                id: id.unwrap_or_else(|| QueueItemId {
                    submission_id: new_id(),
                    request_id: None,
                    mode,
                }),
                content,
            })
            .collect::<Vec<_>>();
        let before = self.queue.texts();
        self.queue.replace(items.clone());
        self.saved = items;
        self.past = session.past_submissions.clone();
        self.store = runtime.store().cloned();
        self.queue.texts() != before
    }

    /// The earlier submission `submission`'s `requestId` names, looked up
    /// in `runs`, the queue and the submissions that left it. One of
    /// another kind refuses it.
    fn repeat(&self, submission: &Submission, runs: &[Run]) -> Result<Option<Repeat>, SubmitError> {
        let Some(request_id) = submission.request_id.as_deref() else {
            return Ok(None);
        };
        let named = |id: &QueueItemId| id.request_id.as_deref() == Some(request_id);
        let started = runs
            .iter()
            .rev()
            .find(|run| run.request_id.as_deref() == Some(request_id))
            .map(|run| Repeat {
                submission_id: run.id.clone(),
                kind: run.kind,
                state: placed_state(&run.id, runs),
            });
        let queued = || {
            let items = self.queue.items().into_iter();
            items
                .map(|item| item.id)
                .find(|id| named(id))
                .map(|id| Repeat {
                    submission_id: id.submission_id,
                    kind: id.mode.into(),
                    state: SubmissionState::Queued(id.mode),
                })
        };
        let past = || {
            let past = self.past.iter().rev().find(|past| named(&past.id))?;
            Some(Repeat {
                submission_id: past.id.submission_id.clone(),
                kind: past.id.mode.into(),
                state: match &past.run_id {
                    Some(run_id) => placed_state(run_id, runs),
                    None => SubmissionState::Withdrawn,
                },
            })
        };
        match started.or_else(queued).or_else(past) {
            Some(repeat) if repeat.kind != submission.kind => Err(SubmitError::Reused {
                request_id: request_id.to_owned(),
                kind: repeat.kind,
            }),
            found => Ok(found),
        }
    }

    /// Queues `submission` as `mode`, once it is saved.
    fn enqueue(
        &mut self,
        mode: QueueMode,
        submission: Submission,
        runs: &[Run],
    ) -> Result<Enqueued, SubmitError> {
        if let Some(repeat) = self.repeat(&submission, runs)? {
            return Ok(Enqueued::Repeated(repeat));
        }
        if submission.request_id.is_some() && submission.kind != SubmissionKind::from(mode) {
            return Err(SubmitError::QueuedPrompt);
        }
        let active = self.run.as_ref().map(|run| run.id.as_str());
        fence(submission.expected_run_id.as_deref(), active).map_err(SubmitError::Refused)?;
        if !submission.images.is_empty() {
            return Err(SubmitError::Refused(
                "queued messages are text only; send images with a prompt that starts a run".into(),
            ));
        }
        let id = QueueItemId {
            submission_id: new_id(),
            request_id: submission.request_id,
            mode,
        };
        let item = QueueItem {
            id: id.clone(),
            content: self.host.expand(&submission.content),
        };
        let mut items = self.queue.items();
        let at = match mode {
            QueueMode::Steer => items
                .iter()
                .take_while(|item| item.id.mode == QueueMode::Steer)
                .count(),
            QueueMode::FollowUp => items.len(),
        };
        items.insert(at, item);
        self.commit(items, Vec::new())
            .map_err(|error| SubmitError::Refused(error.to_string()))?;
        Ok(Enqueued::Queued(id))
    }

    fn withdraw(&mut self, submission_id: &str, runs: &[Run]) -> Result<Withdrawal, String> {
        let (withdrawn, items): (Vec<_>, Vec<_>) = self
            .queue
            .items()
            .into_iter()
            .partition(|item| item.id.submission_id == submission_id);
        if !withdrawn.is_empty() {
            self.commit(items, withdrawn)
                .map_err(|error| error.to_string())?;
            return Ok(Withdrawal::Withdrawn);
        }
        let state = if let Some(run) = runs.iter().rev().find(|run| run.id == submission_id) {
            placed_state(&run.id, runs)
        } else {
            let past = (self.past.iter().rev()).find(|past| past.id.submission_id == submission_id);
            match past.map(|past| past.run_id.as_deref()) {
                Some(Some(run_id)) => placed_state(run_id, runs),
                Some(None) => SubmissionState::Withdrawn,
                None => return Err(format!("no submission {submission_id} in this session")),
            }
        };
        Ok(match state {
            SubmissionState::Placed { .. } => Withdrawal::AlreadyPlaced,
            _ => Withdrawal::Settled,
        })
    }

    fn clear_queue(&mut self) -> Result<Vec<QueueItem>, String> {
        self.clear_queue_saving().map_err(|error| error.to_string())
    }

    fn clear_queue_saving(&mut self) -> Result<Vec<QueueItem>, SessionError> {
        let items = self.queue.items();
        self.commit(Vec::new(), items.clone())?;
        Ok(items)
    }

    /// Saves `items` as the queue, steering first, withdrawing
    /// `withdrawn`; then makes it the queue and reports it. A save that
    /// fails changes nothing.
    fn commit(
        &mut self,
        items: Vec<QueueItem>,
        withdrawn: Vec<QueueItem>,
    ) -> Result<(), SessionError> {
        let ids = withdrawn
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>();
        self.save(&items, &ids)?;
        self.queue.replace(items);
        if !withdrawn.is_empty() {
            let past = ids
                .into_iter()
                .map(|id| PastSubmission { id, run_id: None });
            self.past.extend(past);
            self.host.event(Event::Withdrawn {
                submissions: &withdrawn,
            });
        }
        self.host.event(Event::QueueChanged { queue: &self.queue });
        Ok(())
    }

    /// Saves the queue as `items` unless it is saved so already.
    fn save(&mut self, items: &[QueueItem], withdrawn: &[QueueItemId]) -> Result<(), SessionError> {
        let withdraws = withdrawn.iter().any(QueueItemId::is_durable);
        if items == self.saved.as_slice() && !withdraws {
            return Ok(());
        }
        if let Some(store) = &self.store {
            store.record_queue_items(items, withdrawn)?;
        }
        self.saved = items.to_vec();
        Ok(())
    }

    /// Placed submissions leave the saved queue once they are saved with
    /// the message that places them. Should this fail, a restart takes
    /// them out again, by that message.
    fn save_delivered(&mut self) {
        let items = self.queue.items();
        if let Err(error) = self.save(&items, &[]) {
            self.host.event(Event::SaveFailed { error: &error });
        }
    }

    /// Records `placed` as placed into the active run.
    fn place(&mut self, placed: &[QueueItemId]) {
        let Some(run) = &self.run else {
            return;
        };
        if placed.is_empty() {
            return;
        }
        let run_id = run.id.clone();
        self.past.extend(placed.iter().map(|id| PastSubmission {
            id: id.clone(),
            run_id: Some(run_id.clone()),
        }));
        self.host.event(Event::Placed {
            run_id: &run_id,
            submissions: placed,
        });
    }

    /// The steering the run's rounds placed since this was last asked.
    fn place_steering(&mut self) {
        let placed = self.queue.steering.take_delivered();
        self.place(&placed);
    }

    /// An event from the active run's engine call.
    fn turn(&mut self, event: &TurnEvent) {
        let Some(run) = &mut self.run else {
            return;
        };
        run.mirror(event);
        self.host.event(Event::Turn {
            event,
            queue: &self.queue,
        });
        if matches!(event, TurnEvent::MessageAdded(message) if message.role == MessageRole::User) {
            self.place_steering();
            self.save_delivered();
        }
    }

    /// An engine call returned: report what it saved and read the session
    /// afresh (a compaction rewrites it).
    fn call_returned(&mut self, session: &SessionState, before: usize) {
        let added = session.messages.get(before..).unwrap_or_default();
        self.host.event(Event::CallReturned {
            added,
            session,
            queue: &self.queue,
        });
        if let Some(run) = &mut self.run {
            run.session = session.clone();
        }
        self.place_steering();
        self.save_delivered();
    }

    /// The queued follow-ups as the next engine call, placed into the run.
    fn take_follow_ups(&mut self) -> Option<Call> {
        if self.queue.follow_ups.is_empty() {
            return None;
        }
        let items = std::mem::take(&mut self.queue.follow_ups);
        let texts = items.iter().map(|item| item.content.as_str());
        let text = texts.collect::<Vec<_>>().join(STEERING_JOIN);
        let placed = items.into_iter().map(|item| item.id).collect::<Vec<_>>();
        self.place(&placed);
        Some(Call::FollowUps {
            message: user_message(text, Vec::new()),
            placed,
        })
    }

    /// The run settled: it is no longer active.
    fn settle(&mut self, run_id: &str, outcome: &Outcome) {
        self.place_steering();
        self.run = None;
        self.host.event(Event::RunSettled { run_id, outcome });
    }
}

impl ActiveRun {
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
                    aborted: false,
                });
            }
            _ => {}
        }
    }
}

/// A random UUID (version 4), naming a run or a submission.
pub fn new_id() -> String {
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
