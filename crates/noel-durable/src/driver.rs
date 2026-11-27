//! The harness's driver task: the worker's command loop, cut to the
//! operations a program sends. It owns the `SessionController` (the
//! runtime and the store), so a handle's methods are request/response
//! over it.
//!
//! A submission that starts a run replies at once (its [`Submission`]
//! carries the channel its settle resolves on), and the driver then
//! drives the run with the same inbox every request arrives on: what a
//! program asks meanwhile is handled through the run, as the worker's
//! commands are (`SessionController::run`'s busy handler), so steering,
//! withdrawal, abort and queue reads work while the run works.
//!
//! Opening the same storage twice gets the same work back: a run the
//! journal says was interrupted resumes before the first request is
//! served (the plan's premise, the server's crash loop).

use std::sync::{Arc, Mutex};

use noel_core::controller::{AbortPolicy, Busy, SessionController, Start};
use noel_core::{QueueItem, QueueMode, RunStatus, SubmissionKind};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::events::{Events, OwnedEvent, Subscription};
use crate::host::SdkHost;
use crate::submission::{Settled, Submission};
use crate::{Input, InputId, QueuedItem, SubmitError, Withdrawn};

/// What a handle asks the driver.
enum Request {
    Submit {
        input: Input,
        id: InputId,
        reply: oneshot::Sender<Result<Submission, SubmitError>>,
    },
    Withdraw {
        submission_id: String,
        reply: oneshot::Sender<Result<Withdrawn, String>>,
    },
    Abort {
        reply: oneshot::Sender<Result<(), String>>,
    },
    Queue {
        reply: oneshot::Sender<Vec<QueuedItem>>,
    },
    Events {
        reply: oneshot::Sender<Vec<OwnedEvent>>,
    },
    /// The SDK's task surface (slice 3): create, run, read, abort.
    TaskCreate {
        kind: String,
        input: serde_json::Value,
        background: bool,
        owner_task_id: Option<String>,
        reply: oneshot::Sender<Result<String, String>>,
    },
    TaskWait {
        task_id: String,
        reply: oneshot::Sender<crate::tasks::TaskRecord>,
    },
    TaskAbort {
        task_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    TaskJoin {
        task_ids: Vec<String>,
        policy: crate::tools::JoinPolicy,
        reply: oneshot::Sender<Vec<crate::tasks::TaskRecord>>,
    },
    /// The earliest sleeping deadline (the harness's wake).
    TaskWake {
        reply: oneshot::Sender<Option<u64>>,
    },
    /// Delivers a signal value under a key (upstream `harness.signal`).
    TaskSignal {
        task_id: String,
        signal: String,
        key: String,
        value: Value,
        reply: oneshot::Sender<crate::tasks::SignalResult>,
    },
    /// Re-run scheduling after an injected clock or registry change.
    Resume,
    Close {
        reply: oneshot::Sender<()>,
    },
}

/// The driver's public side.
pub struct Handle {
    requests: mpsc::UnboundedSender<Request>,
    events: Arc<Events>,
}

impl Clone for Handle {
    fn clone(&self) -> Handle {
        Handle {
            requests: self.requests.clone(),
            events: self.events.clone(),
        }
    }
}

impl Handle {
    pub(super) async fn submit(
        &self,
        input: Input,
        id: InputId,
    ) -> Result<Submission, SubmitError> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::Submit { input, id, reply })
            .map_err(|_| SubmitError::Refused("the harness is closed".into()))?;
        done.await
            .map_err(|_| SubmitError::Refused("the harness is closed".to_string()))?
    }

    pub(super) async fn withdraw(&self, submission_id: &str) -> Result<Withdrawn, String> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::Withdraw {
                submission_id: submission_id.to_owned(),
                reply,
            })
            .map_err(|_| "the harness is closed".to_string())?;
        done.await
            .map_err(|_| "the harness is closed".to_string())?
    }

    pub(super) async fn abort(&self) -> Result<(), String> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::Abort { reply })
            .map_err(|_| "the harness is closed".to_string())?;
        done.await
            .map_err(|_| "the harness is closed".to_string())?
    }

    pub(super) async fn queue(&self) -> Vec<QueuedItem> {
        let (reply, done) = oneshot::channel();
        if self.requests.send(Request::Queue { reply }).is_err() {
            return Vec::new();
        }
        done.await.unwrap_or_default()
    }

    pub(super) async fn take_events(&mut self) -> Vec<OwnedEvent> {
        let (reply, done) = oneshot::channel();
        if self.requests.send(Request::Events { reply }).is_err() {
            return Vec::new();
        }
        done.await.unwrap_or_default()
    }

    /// Creates a durable task of `kind` with `input`, returning its id.
    pub(crate) async fn task_create(
        &self,
        kind: &str,
        input: serde_json::Value,
        background: bool,
    ) -> Result<String, String> {
        self.task_create_owned(kind, input, background, None).await
    }

    pub(crate) async fn task_create_owned(
        &self,
        kind: &str,
        input: serde_json::Value,
        background: bool,
        owner_task_id: Option<String>,
    ) -> Result<String, String> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::TaskCreate {
                kind: kind.to_owned(),
                input,
                background,
                owner_task_id,
                reply,
            })
            .map_err(|_| "the harness is closed".to_string())?;
        done.await
            .map_err(|_| "the harness is closed".to_string())?
    }

    pub(crate) async fn task_join(
        &self,
        task_ids: Vec<String>,
        policy: crate::tools::JoinPolicy,
    ) -> Vec<crate::tasks::TaskRecord> {
        let (reply, done) = oneshot::channel();
        if self
            .requests
            .send(Request::TaskJoin {
                task_ids,
                policy,
                reply,
            })
            .is_err()
        {
            return Vec::new();
        }
        done.await.unwrap_or_default()
    }

    /// Waits for a task's terminal record (running it if it is runnable).
    pub(crate) async fn task_wait(&self, task_id: &str) -> crate::tasks::TaskRecord {
        let (reply, done) = oneshot::channel();
        if self
            .requests
            .send(Request::TaskWait {
                task_id: task_id.to_owned(),
                reply,
            })
            .is_err()
        {
            return crate::tasks::TaskRecord::orphaned(task_id);
        }
        done.await
            .unwrap_or_else(|_| crate::tasks::TaskRecord::orphaned(task_id))
    }

    /// The harness's wake: the earliest deadline of its sleeping tasks,
    /// or `None` when none sleeps (upstream `harness.wake()`).
    /// Delivers a signal value under a key (upstream `harness.signal`).
    pub(crate) async fn task_signal(
        &self,
        task_id: &str,
        signal: &str,
        key: &str,
        value: Value,
    ) -> crate::tasks::SignalResult {
        let (reply, done) = oneshot::channel();
        if self
            .requests
            .send(Request::TaskSignal {
                task_id: task_id.to_owned(),
                signal: signal.to_owned(),
                key: key.to_owned(),
                value,
                reply,
            })
            .is_err()
        {
            return crate::tasks::SignalResult::NotFound;
        }
        done.await.unwrap_or(crate::tasks::SignalResult::NotFound)
    }

    pub(crate) async fn wake(&self) -> Option<u64> {
        let (reply, done) = oneshot::channel();
        if self.requests.send(Request::TaskWake { reply }).is_err() {
            return None;
        }
        done.await.unwrap_or(None)
    }

    pub(crate) fn resume(&self) {
        let _ = self.requests.send(Request::Resume);
    }

    /// Marks a task aborted; its abort handler decides the outcome.
    pub(crate) async fn task_abort(&self, task_id: &str) -> Result<(), String> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::TaskAbort {
                task_id: task_id.to_owned(),
                reply,
            })
            .map_err(|_| "the harness is closed".to_string())?;
        done.await
            .map_err(|_| "the harness is closed".to_string())?
    }

    pub fn subscribe(&self) -> Subscription {
        self.events.subscribe()
    }

    pub(super) async fn close(self) -> Result<(), crate::Error> {
        let (reply, done) = oneshot::channel();
        self.requests
            .send(Request::Close { reply })
            .map_err(|_| anyhow::anyhow!("the harness is already closed"))?;
        let _ = done.await;
        // Dropping `self` drops the last sender: the driver's inbox closes,
        // a run it is driving ends as a close (no restart counted), and
        // the task finishes.
        Ok(())
    }
}

/// Starts the driver on `runtime` and returns its handle.
pub(super) async fn spawn(
    mut runtime: noel_core::Runtime,
    options: crate::HarnessOptions,
    storage: noel_core::storage::Storage,
) -> Result<Handle, crate::HarnessError> {
    let (requests, inbox) = mpsc::unbounded_channel();
    let events = Arc::new(Events::new());
    let host = SdkHost {
        clock: Arc::new(options.clock.clone()),
        providers: Arc::new(options.providers),
        tools: Mutex::new(options.tools),
        tool_runtime: None,
        events: events.clone(),
        waiters: Mutex::new(Vec::new()),
        tasks: options.tasks.clone(),
    };
    // The program's environment hook, if it gave one.
    if let Some(env) = options.env {
        runtime.set_env_provider(noel_core::EnvProvider::Hook(env));
    }
    let mut controller = SessionController::new(runtime, host, AbortPolicy::WithdrawQueue);
    // The program's tools are part of the runtime's offer, like the
    // worker's plugin tools: installed before the first run starts.
    controller.configure(|runtime, host| {
        let tools = std::mem::take(&mut *host.tools.lock().expect("tool registry"));
        host.tool_runtime = Some(tools.install(runtime, storage, host.clock.clone()));
    });
    // A run the journal says was interrupted resumes first, as the
    // server's crash loop resumes a restarted worker's run.
    let mut resume = None;
    if let Some(run_id) = controller
        .runtime()
        .session()
        .runs
        .last()
        .filter(|run| run.status == RunStatus::Interrupted)
        .map(|run| run.id.clone())
        && let Ok(start) = controller.resume(&run_id).await
    {
        resume = Some(start);
    }
    // Durable tasks recover at open (slice 3): those left running by a
    // stop that was not a close count a restart and go back to pending,
    // and one at the limit ends restart_limit.
    crate::driver_tasks::recover(&mut controller, options.task_settings.restart_limit);
    tokio::spawn(driver(controller, inbox, resume));
    Ok(Handle { requests, events })
}

async fn driver(
    mut controller: SessionController<SdkHost>,
    mut inbox: mpsc::UnboundedReceiver<Request>,
    resume: Option<Start>,
) {
    // The resumed run drives first: its settle is in the event stream, and
    // requests that arrive while it works are served through it.
    if let Some(start) = resume
        && !drive(&mut controller, start, &mut inbox, None).await
    {
        return;
    }
    while let Some(request) = inbox.recv().await {
        match request {
            Request::Submit { input, id, reply } => {
                submit(&mut controller, input, id, reply, &mut inbox).await;
            }
            Request::Withdraw {
                submission_id,
                reply,
            } => {
                let outcome = controller
                    .withdraw(&submission_id)
                    .map_err(|error| error.to_string());
                let _ = reply.send(outcome);
            }
            Request::Abort { reply } => {
                // With no run active there is nothing to abort.
                let _ = reply.send(Ok(()));
            }
            Request::Queue { reply } => {
                let queue = controller
                    .queue()
                    .items()
                    .into_iter()
                    .map(queued_item)
                    .collect();
                let _ = reply.send(queue);
            }
            Request::Events { reply } => {
                let _ = reply.send(controller.host().events.take());
            }
            Request::TaskCreate {
                kind,
                input,
                background,
                owner_task_id,
                reply,
            } => {
                let _ = reply.send(crate::driver_tasks::create_owned(
                    &mut controller,
                    &kind,
                    input,
                    background,
                    owner_task_id,
                ));
            }
            Request::TaskWait { task_id, reply } => {
                let record = crate::driver_tasks::run_to_terminal(&mut controller, &task_id).await;
                let _ = reply.send(record);
            }
            Request::TaskAbort { task_id, reply } => {
                let tool = controller
                    .host()
                    .tool_runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.abort(&task_id));
                let outcome = if tool {
                    Ok(())
                } else {
                    crate::driver_tasks::abort(&mut controller, &task_id).await
                };
                let _ = reply.send(outcome);
            }
            Request::TaskJoin {
                task_ids,
                policy,
                reply,
            } => {
                let records = crate::driver_tasks::join(&mut controller, &task_ids, policy).await;
                let _ = reply.send(records);
            }
            Request::TaskWake { reply } => {
                let tools = controller
                    .host()
                    .tool_runtime
                    .as_ref()
                    .and_then(|runtime| runtime.wake());
                let tasks = crate::driver_tasks::wake(&controller);
                let _ = reply.send(tools.into_iter().chain(tasks).min());
            }
            Request::TaskSignal {
                task_id,
                signal,
                key,
                value,
                reply,
            } => {
                let tool = controller
                    .host()
                    .tool_runtime
                    .as_ref()
                    .map(|runtime| runtime.signal(&task_id, &signal, &key, value.clone()))
                    .unwrap_or(crate::tasks::SignalResult::NotFound);
                let outcome = if tool == crate::tasks::SignalResult::NotFound {
                    crate::driver_tasks::signal(&mut controller, &task_id, &signal, &key, value)
                } else {
                    tool
                };
                let _ = reply.send(outcome);
            }
            Request::Resume => {
                if let Some(runtime) = &controller.host().tool_runtime {
                    runtime.resume();
                }
            }
            Request::Close { reply } => {
                let _ = reply.send(());
                return;
            }
        }
    }
}

/// Admits `input` and, when it starts a run, drives that run, replying
/// first so the submitter can watch the events while its answer is
/// worked on. `settle` resolves when the run ends.
async fn submit(
    controller: &mut SessionController<SdkHost>,
    input: Input,
    id: InputId,
    reply: oneshot::Sender<Result<Submission, SubmitError>>,
    inbox: &mut mpsc::UnboundedReceiver<Request>,
) {
    let submission = core_submission(&input, &id);
    // A run is never active here: the driver drives one to its end inline,
    // and a submit that arrives while it works is served through the run's
    // busy handler (busy_request) instead of this path.
    let admitted = match &input {
        Input::Continue => controller.continue_run(id.request_id()).await,
        _ => controller.submit(submission).await,
    };
    let admitted = match admitted {
        Ok(admitted) => admitted,
        Err(error) => {
            let _ = reply.send(Err(error.into()));
            return;
        }
    };
    match admitted {
        noel_core::controller::Submitted::Repeated(repeat) => {
            let _ = reply.send(Ok(Submission::repeated(repeat)));
        }
        noel_core::controller::Submitted::Started(start) => {
            let run_id = start.run_id().to_owned();
            let (settle, done) = oneshot::channel::<Settled>();
            let _ = reply.send(Ok(Submission::running(run_id, done)));
            drive(controller, start, inbox, Some(settle)).await;
        }
    }
}

/// A run driven to its end. What arrives while it works is handled
/// through the controller's busy path (`SessionController::run`'s
/// handler), and `settle`, when given, is resolved with how it ended.
async fn drive(
    controller: &mut SessionController<SdkHost>,
    start: Start,
    inbox: &mut mpsc::UnboundedReceiver<Request>,
    settle: Option<oneshot::Sender<Settled>>,
) -> bool {
    let finished = controller.run(start, inbox, busy_request).await;
    let settled = if !finished {
        Settled::closed()
    } else {
        // The run settled: the host recorded how in the events; the last
        // RunSettled is the run's own. The log keeps the rest for the
        // host to take.
        let last = controller
            .host()
            .events
            .peek()
            .iter()
            .rev()
            .find_map(|event| match event {
                OwnedEvent::RunSettled { run_id, outcome } => {
                    Some(Settled::from_outcome(run_id.clone(), outcome.clone()))
                }
                _ => None,
            })
            .unwrap_or_else(Settled::closed);
        // The settle stays in the log: the host's take is the program's
        // view of the whole run.
        last
    };
    if let Some(settle) = settle {
        let _ = settle.send(settled.clone());
    }
    // Everything queued or placed into this run answers with its settle.
    let waiters = std::mem::take(&mut *controller.host().waiters.lock().expect("waiters"));
    for waiter in waiters {
        let _ = waiter.send(settled.clone());
    }
    finished
}

/// What arrived while the run worked, applied through `Busy` (the
/// worker's `busy_command`).
fn busy_request(request: Request, run: &mut Busy<'_, SdkHost>) {
    match request {
        Request::Submit { input, id, reply } => {
            let submission = core_submission(&input, &id);
            // A submission admitted while a run works waits for that run's
            // settle, as one that started it does; one already settled
            // (a repeat of an answered submission) does not.
            let outcome = run
                .submit(submission, when_busy(&input))
                .map_err(SubmitError::from);
            let outcome = outcome.map(|enqueued| match enqueued {
                noel_core::controller::Enqueued::Queued(_) => {
                    let (settle, done) = oneshot::channel::<Settled>();
                    run.host().waiters.lock().expect("waiters").push(settle);
                    Submission::queued(mode_of(&input), done)
                }
                noel_core::controller::Enqueued::Repeated(repeat) => match &repeat.state {
                    noel_core::controller::SubmissionState::Settled { .. }
                    | noel_core::controller::SubmissionState::Withdrawn => {
                        Submission::repeated(repeat)
                    }
                    _ => {
                        let (settle, done) = oneshot::channel::<Settled>();
                        run.host().waiters.lock().expect("waiters").push(settle);
                        Submission::repeated_with_wait(repeat, done)
                    }
                },
            });
            let _ = reply.send(outcome);
        }
        Request::Withdraw {
            submission_id,
            reply,
        } => {
            let outcome = run
                .withdraw(&submission_id)
                .map_err(|error| error.to_string());
            let _ = reply.send(outcome);
        }
        Request::Abort { reply } => {
            if let Some(runtime) = &run.host().tool_runtime {
                runtime.abort_all();
            }
            let outcome = run.abort_with(None, AbortPolicy::WithdrawQueue);
            let _ = reply.send(outcome.map_err(|error| error.to_string()));
        }
        Request::Queue { reply } => {
            let queue = run.queue().items().into_iter().map(queued_item).collect();
            let _ = reply.send(queue);
        }
        Request::Events { reply } => {
            let _ = reply.send(run.host().events.take());
        }
        // Tasks run while no run is active (slice 3); a conversation's
        // owned tasks during a run are slice 7's.
        Request::TaskCreate { reply, .. } => {
            let _ = reply.send(Err("a run is active; tasks run between runs".to_string()));
        }
        Request::TaskWait { task_id, reply } => {
            let _ = reply.send(crate::tasks::TaskRecord::orphaned(&task_id));
        }
        Request::TaskAbort { task_id, reply } => {
            let outcome = run
                .host()
                .tool_runtime
                .as_ref()
                .filter(|runtime| runtime.abort(&task_id))
                .map(|_| Ok(()))
                .unwrap_or_else(|| {
                    Err(format!(
                        "a run is active; task {task_id} cannot be aborted through it"
                    ))
                });
            let _ = reply.send(outcome);
        }
        Request::TaskJoin { reply, .. } => {
            let _ = reply.send(Vec::new());
        }
        Request::TaskWake { reply } => {
            let wake = run
                .host()
                .tool_runtime
                .as_ref()
                .and_then(|runtime| runtime.wake());
            let _ = reply.send(wake);
        }
        Request::TaskSignal {
            task_id,
            signal,
            key,
            value,
            reply,
        } => {
            let outcome = run
                .host()
                .tool_runtime
                .as_ref()
                .map(|runtime| runtime.signal(&task_id, &signal, &key, value))
                .unwrap_or(crate::tasks::SignalResult::NotFound);
            let _ = reply.send(outcome);
        }
        Request::Resume => {
            if let Some(runtime) = &run.host().tool_runtime {
                runtime.resume();
            }
        }
        Request::Close { reply } => {
            let _ = reply.send(());
        }
    }
}

/// What an input does while a run is active (spec §3.1).
fn when_busy(input: &Input) -> noel_core::controller::WhenBusy {
    match input {
        Input::Steer { .. } => noel_core::controller::WhenBusy::Steer,
        Input::FollowUp { .. } | Input::Prompt { .. } => noel_core::controller::WhenBusy::FollowUp,
        // A continue never queues (Durable): a busy conversation refuses.
        Input::Continue => noel_core::controller::WhenBusy::Reject,
    }
}

fn mode_of(input: &Input) -> &'static str {
    match input {
        Input::Steer { .. } => "steer",
        _ => "followUp",
    }
}

/// The noel-core submission a program's input admits.
fn core_submission(input: &Input, id: &InputId) -> noel_core::controller::Submission {
    let (kind, content, images) = match input {
        Input::Prompt { content, images } => (
            SubmissionKind::Prompt,
            content.clone(),
            images
                .iter()
                .map(|image| noel_core::controller::Image {
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                })
                .collect::<Vec<_>>(),
        ),
        Input::Steer { content } => (SubmissionKind::Steer, content.clone(), Vec::new()),
        Input::FollowUp { content } => (SubmissionKind::FollowUp, content.clone(), Vec::new()),
        Input::Continue => (SubmissionKind::Continue, String::new(), Vec::new()),
    };
    noel_core::controller::Submission {
        kind,
        request_id: id.request_id(),
        content,
        images,
        expected_run_id: None,
    }
}

impl InputId {
    pub(crate) fn request_id(&self) -> Option<String> {
        match self {
            InputId::Fresh => None,
            InputId::RequestId(id) => Some(id.clone()),
        }
    }
}

fn queued_item(item: QueueItem) -> QueuedItem {
    QueuedItem {
        submission_id: item.id.submission_id,
        request_id: item.id.request_id,
        mode: match item.id.mode {
            QueueMode::Steer => "steer",
            QueueMode::FollowUp => "followUp",
        },
        content: item.content,
    }
}
