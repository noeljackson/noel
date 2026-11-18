//! The driver's task engine (plan slice 3): what `Request::Task*` do.
//!
//! Records live in the session's journal through `Runtime::save_task`
//! (the session's task table and one record, together); the engine keeps
//! none in memory. A task runs by invoking its definition's phase at its
//! checkpoint; the phase's commits travel a channel back to the engine,
//! which applies them (and answers with the record the commit produced,
//! so the context's memos stay honest). Recovery falls out of the
//! journal: a reopened session's tasks load with their last saved
//! checkpoints, a task left `running` by a stop that was not a close
//! counts one restart, and at the limit it ends `restart_limit`
//! (upstream `harness-restart-failure`).

use std::sync::Arc;

use noel_core::controller::SessionController;
use noel_core::DurableTaskRecord;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::host::SdkHost;
use crate::tasks::{TaskCommit, TaskOutcome, TaskRecord, TaskRuntime, TaskState};

/// The controller's runtime, mutable, through the controller's own
/// configure (it is the one writer while no run is active).
fn controller_mut_of(controller: &mut SessionController<SdkHost>) -> RuntimeMut<'_> {
    RuntimeMut { controller }
}

/// A &mut Runtime borrowed through `SessionController::configure`, held
/// across one call site at a time.
pub(crate) struct RuntimeMut<'c> {
    controller: &'c mut SessionController<SdkHost>,
}

impl RuntimeMut<'_> {
    pub(crate) fn save_task(
        &mut self,
        record: DurableTaskRecord,
    ) -> Result<(), noel_core::SessionError> {
        self.controller
            .configure(|runtime, _| runtime.save_task(record))
    }
}

/// The state's serde shape on the wire (the record's `state` value).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub(crate) enum RecordState {
    Pending {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<Value>,
    },
    Running {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<Value>,
    },
    Sleeping {
        checkpoint: Value,
        until: u64,
    },
    WaitingOnSignals {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        signals: Vec<Value>,
    },
    Terminal {
        outcome: RecordOutcome,
    },
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub(crate) enum RecordOutcome {
    Completed { result: Value },
    Failed { error: String },
    Aborted,
    RestartLimit,
    Orphaned,
}

/// Creates a task of `kind` with `input`, saved pending at a null
/// checkpoint: the definition's first phase writes its initial
/// checkpoint as its first commit (its `initial` closure runs inside the
/// erased definition's invoke path), so a crash before that leaves a
/// task that cleanly restarts at the beginning.
pub(super) fn create(
    controller: &mut SessionController<SdkHost>,
    kind: &str,
    input: Value,
    background: bool,
) -> Result<String, String> {
    if !controller.host().tasks.erased.contains_key(kind) {
        return Err(format!("no task named {kind} is registered"));
    }
    let id = noel_core::controller::new_id();
    let record = DurableTaskRecord {
        id: id.clone(),
        kind: kind.to_owned(),
        version: 1,
        input,
        state: serde_json::to_value(RecordState::Pending {
            checkpoint: Value::Null,
            wake: None,
        })
        .expect("a task state serializes"),
        background,
        abort_requested: false,
        restarts: 0,
        memos: Default::default(),
    };
    controller_mut_of(controller)
        .save_task(record)
        .map_err(|error| error.to_string())?;
    Ok(id)
}

/// Runs `task_id` to its terminal record, phase after phase. A task
/// already terminal answers at once; a runnable one runs now — waiting
/// is what starts work (upstream's `waitForTask` enables scheduling).
pub(super) async fn run_to_terminal(
    controller: &mut SessionController<SdkHost>,
    task_id: &str,
) -> TaskRecord {
    loop {
        let Some(record) = controller.runtime().session().tasks.get(task_id).cloned() else {
            return TaskRecord::orphaned(task_id);
        };
        // A woken tool park resumes the run it interrupted first (slice
        // 6): the replay reruns the parked call, whose execute returns
        // the result the run then completes with.
        if record.kind == "noel.tool-park"
            && let RecordState::WaitingOnSignals {
                checkpoint,
                until,
                signals,
            } = record_state(&record)
        {
            let now = controller.host().clock.now();
            let woken = crate::driver_tasks::first_signalled(controller, &record, &signals)
                .is_some()
                || until.is_some_and(|until| now >= until);
            if woken
                && let Some(run_id) = controller
                    .runtime()
                    .session()
                    .runs
                    .last()
                    .filter(|run| run.status == noel_core::RunStatus::Interrupted)
                    .map(|run| run.id.clone())
                && let Ok(start) = controller.resume(&run_id).await
            {
                let (sender, mut inbox) =
                    tokio::sync::mpsc::unbounded_channel::<crate::driver::Request>();
                drop(sender);
                let _ = controller
                    .run(start, &mut inbox, crate::driver::busy_noop)
                    .await;
            }
            let _ = checkpoint;
        }
        match record_state(&record) {
            RecordState::Terminal { outcome } => {
                return public(
                    &record,
                    TaskState::Terminal {
                        outcome: outcome_public(outcome),
                    },
                )
            }
            RecordState::Sleeping { checkpoint, until } => {
                // A due sleep runs at this pass (upstream: any scheduling
                // pass runs it, before its timer); one not due answers its
                // sleeping record, and a later wait runs it.
                let now = controller.host().clock.now();
                if now >= until {
                    invoke(controller, &record, checkpoint, false).await;
                } else {
                    return public(&record, TaskState::Sleeping { checkpoint, until });
                }
            }
            RecordState::WaitingOnSignals {
                checkpoint,
                until,
                signals,
            } => {
                // A wake is decided: a key holding a value no wake used
                // (delivered, or kept from an earlier wake), or the
                // deadline passing (which expires the keys).
                let now = controller.host().clock.now();
                let due = until.is_some_and(|until| now >= until);
                if let Some(wake) = first_signalled(controller, &record, &signals) {
                    let woken = with_wake(controller, &record, wake);
                    invoke(controller, &woken, checkpoint, false).await;
                } else if due {
                    expire_signals(controller, &record, &signals);
                    let woken = with_wake(
                        controller,
                        &record,
                        serde_json::to_value(crate::tasks::WakeReason::Expired).expect("wake"),
                    );
                    invoke(controller, &woken, checkpoint, false).await;
                } else if signals.is_empty() && until.is_none() {
                    // Cannot wait on nothing (upstream faults it); refuse
                    // at commit time would be better, so settle failed.
                    settle(
                        controller,
                        &record,
                        RecordOutcome::Failed {
                            error: "cannot sleep without until or signals".into(),
                        },
                    );
                    return public(
                        controller
                            .runtime()
                            .session()
                            .tasks
                            .get(task_id)
                            .unwrap_or(&record),
                        TaskState::Terminal {
                            outcome: TaskOutcome::Failed {
                                error: "cannot sleep without until or signals".into(),
                            },
                        },
                    );
                } else {
                    let deadline = until.filter(|until| *until > now);
                    let _ = deadline;
                    return public(
                        &record,
                        TaskState::WaitingOnSignals {
                            checkpoint,
                            until,
                            signals,
                        },
                    );
                }
            }
            RecordState::Pending { checkpoint, .. } | RecordState::Running { checkpoint, .. } => {
                invoke(controller, &record, checkpoint, false).await;
            }
        }
    }
}

/// Aborts a task: the abort mark is saved, then the abort handler runs
/// (a definition without one settles the task aborted).
pub(super) async fn abort(
    controller: &mut SessionController<SdkHost>,
    task_id: &str,
) -> Result<(), String> {
    let Some(mut record) = controller.runtime().session().tasks.get(task_id).cloned() else {
        return Err(format!("no task {task_id}"));
    };
    let state = record_state(&record);
    match state {
        RecordState::Terminal { .. } => Ok(()),
        RecordState::Pending { checkpoint, .. }
        | RecordState::Running { checkpoint, .. }
        | RecordState::Sleeping { checkpoint, .. }
        | RecordState::WaitingOnSignals { checkpoint, .. } => {
            record.abort_requested = true;
            controller_mut_of(controller)
                .save_task(record.clone())
                .map_err(|error| error.to_string())?;
            invoke(controller, &record, checkpoint, true).await;
            Ok(())
        }
    }
}

/// Recovery at open (slice 3's rule): every task left `running` by a
/// stop that was not a close counts one restart; one at the limit ends
/// `restart_limit`, the rest go back to pending at their checkpoints.
/// Runs before the driver serves its first request, after the run
/// resume.
pub(super) fn recover(controller: &mut SessionController<SdkHost>, restart_limit: u32) {
    let records = controller
        .runtime()
        .session()
        .tasks
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for record in records {
        if let RecordState::Running { checkpoint, .. } = record_state(&record) {
            let restarts = record.restarts + 1;
            if restarts >= restart_limit {
                let state = serde_json::to_value(RecordState::Terminal {
                    outcome: RecordOutcome::RestartLimit,
                })
                .expect("a task state serializes");
                let _ =
                    controller_mut_of(controller).save_task(DurableTaskRecord { state, ..record });
            } else {
                let state = serde_json::to_value(RecordState::Pending {
                    checkpoint,
                    wake: None,
                })
                .expect("a task state serializes");
                let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
                    restarts,
                    state,
                    ..record
                });
            }
        }
    }
}

/// Invokes one phase of `record` at `checkpoint`, wiring its commits to
/// the store through a channel the engine answers. `aborting` runs the
/// abort handler instead of the phase.
async fn invoke(
    controller: &mut SessionController<SdkHost>,
    record: &DurableTaskRecord,
    checkpoint: Value,
    aborting: bool,
) {
    // Mark running (one record), keeping the wake this invocation
    // resumes at (a wait's wake, stamped by the pass that woke it).
    let wake = record
        .state
        .get("wake")
        .cloned()
        .filter(|wake| !wake.is_null());
    let running = DurableTaskRecord {
        state: serde_json::to_value(RecordState::Running {
            checkpoint: checkpoint.clone(),
            wake: wake.clone(),
        })
        .expect("a task state serializes"),
        ..record.clone()
    };
    let _ = controller_mut_of(controller).save_task(running.clone());

    let Some(definition) = controller.host().tasks.erased.get(&record.kind).cloned() else {
        settle(controller, record, RecordOutcome::Orphaned);
        return;
    };
    let phase = phase_of(&checkpoint);
    let phase_name = (definition.phase_name)(&phase);
    // An unknown phase (a definition that changed) settles orphaned.
    if !aborting && phase_name.is_empty() {
        settle(controller, record, RecordOutcome::Orphaned);
        return;
    }

    // The commit channel: the engine applies each commit and answers
    // with the memos the record then holds, so the context's next memo
    // reads what was saved.
    let (commits, mut committed) = mpsc::unbounded_channel::<(TaskCommit, OneshotReply)>();
    let runtime = TaskRuntime::new(
        Arc::new(move |commit: TaskCommit| {
            let commits = commits.clone();
            Box::pin(async move {
                let (reply, done) = tokio::sync::oneshot::channel();
                if commits.send((commit, reply)).is_ok() {
                    let _ = done.await;
                }
            })
        }),
        // Memos never go to the store on their own: the context holds
        // them and they are saved with the next commit. This channel
        // answers at once (the closure keeps its own copy).
        {
            let memos = Arc::new(std::sync::Mutex::new(record.memos.clone()));
            Arc::new(move |name: String, value: Value| {
                let memos = memos.clone();
                Box::pin(async move {
                    let mut memos = memos.lock().expect("task memos");
                    if let Some(first) = memos.get(&name) {
                        return first.clone();
                    }
                    memos.insert(name, value.clone());
                    value
                }) as crate::tasks::ValueFuture
            })
        },
    );

    let invocation = {
        let record = Arc::new(running);
        let handler = if aborting {
            definition.abort.clone()
        } else {
            definition.invoke.clone()
        };
        // The phase future is spawned so the engine can serve its commits
        // meanwhile; it is awaited before this returns.
        tokio::spawn(async move {
            handler_phase(
                handler,
                record,
                definition_phase(&definition, &phase),
                runtime,
            )
            .await
        })
    };

    // Serve the phase's commits: apply each to the record through the
    // controller, and answer.
    let mut finished = false;
    while !finished {
        let next = committed.recv().await;
        match next {
            Some((commit, reply)) => {
                let memos = apply(controller, record, commit);
                let _ = reply.send(memos);
            }
            None => finished = true,
        }
    }
    let _ = invocation.await;
}

type OneshotReply = tokio::sync::oneshot::Sender<std::collections::BTreeMap<String, Value>>;

async fn handler_phase(
    handler: crate::tasks::PhaseHandlerErased,
    record: Arc<DurableTaskRecord>,
    phase: &'static str,
    runtime: TaskRuntime,
) {
    handler(record, phase, runtime).await
}

/// The 'static phase name an invocation runs ("" asks for the first).
fn definition_phase(definition: &crate::tasks::ErasedTask, phase: &str) -> &'static str {
    (definition.phase_name)(phase)
}

/// Applies a commit to the task's record: the next state (and memos, as
/// the context reported them through its own copy) as one journal
/// record. Returns the memos saved.
fn apply(
    controller: &mut SessionController<SdkHost>,
    record: &DurableTaskRecord,
    commit: TaskCommit,
) -> std::collections::BTreeMap<String, Value> {
    let memos = controller
        .runtime()
        .session()
        .tasks
        .get(&record.id)
        .map(|record| record.memos.clone())
        .unwrap_or_default();
    let state = match commit {
        TaskCommit::Sleep { until, checkpoint } => RecordState::Sleeping { checkpoint, until },
        TaskCommit::Wait {
            until,
            signals,
            checkpoint,
        } => RecordState::WaitingOnSignals {
            checkpoint,
            until,
            signals: signals
                .into_iter()
                .map(|(signal, key)| serde_json::json!({ "signal": signal, "key": key }))
                .collect(),
        },
        TaskCommit::Continue(checkpoint) => RecordState::Pending {
            checkpoint,
            wake: None,
        },
        TaskCommit::Completed(result) => RecordState::Terminal {
            outcome: RecordOutcome::Completed { result },
        },
        TaskCommit::Failed(error) => RecordState::Terminal {
            outcome: RecordOutcome::Failed { error },
        },
        TaskCommit::Aborted => RecordState::Terminal {
            outcome: RecordOutcome::Aborted,
        },
    };
    let state = serde_json::to_value(state).expect("a task state serializes");
    let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
        state,
        ..record.clone()
    });
    memos
}

fn settle(
    controller: &mut SessionController<SdkHost>,
    record: &DurableTaskRecord,
    outcome: RecordOutcome,
) {
    let state = serde_json::to_value(RecordState::Terminal { outcome }).expect("serializes");
    let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
        state,
        ..record.clone()
    });
}

/// The first of `signals` whose key holds a value no wake used, as a
/// wake (checked in the task's memos).
fn first_signalled(
    _controller: &SessionController<SdkHost>,
    record: &DurableTaskRecord,
    signals: &[Value],
) -> Option<Value> {
    for key in signals {
        let (Some(signal), Some(key)) = (
            key.get("signal").and_then(Value::as_str),
            key.get("key").and_then(Value::as_str),
        ) else {
            continue;
        };
        let memo = format!("pi.signal.{signal}/{key}");
        if let Some(state) = record.memos.get(&memo)
            && state.get("value").is_some()
        {
            return Some(
                serde_json::to_value(crate::tasks::WakeReason::Signal {
                    signal: signal.to_owned(),
                    key: key.to_owned(),
                })
                .expect("a wake serializes"),
            );
        }
    }
    None
}

/// Saves the record with `wake` on its state, for the invocation that
/// resumes at it to read.
fn with_wake(
    controller: &mut SessionController<SdkHost>,
    record: &DurableTaskRecord,
    wake: Value,
) -> DurableTaskRecord {
    let mut state = record.state.clone();
    state["wake"] = wake;
    let woken = DurableTaskRecord {
        state,
        ..record.clone()
    };
    let _ = controller_mut_of(controller).save_task(woken.clone());
    woken
}

/// Expires a wait's keys at its deadline: a later delivery is closed
/// (upstream expires in the delivery path once due).
fn expire_signals(
    controller: &mut SessionController<SdkHost>,
    record: &DurableTaskRecord,
    signals: &[Value],
) {
    let mut memos = record.memos.clone();
    for key in signals {
        let (Some(signal), Some(key)) = (
            key.get("signal").and_then(Value::as_str),
            key.get("key").and_then(Value::as_str),
        ) else {
            continue;
        };
        let memo = format!("pi.signal.{signal}/{key}");
        memos.insert(memo, serde_json::json!({ "expired": true }));
    }
    let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
        memos,
        ..record.clone()
    });
}

/// Delivers a signal value under a key (upstream `harness.signal`):
/// the value waits in the task's memos until a wake uses it. A task that
/// ended, is abort-marked, or whose key expired answers `Closed`.
pub(super) fn signal(
    controller: &mut SessionController<SdkHost>,
    task_id: &str,
    signal: &str,
    key: &str,
    value: Value,
) -> crate::tasks::SignalResult {
    use crate::tasks::SignalResult;
    let Some(record) = controller.runtime().session().tasks.get(task_id).cloned() else {
        return SignalResult::NotFound;
    };
    if record.abort_requested {
        return SignalResult::Closed;
    }
    let state = record_state(&record);
    match state {
        RecordState::Terminal { .. } => SignalResult::Closed,
        RecordState::WaitingOnSignals { until, signals, .. } => {
            // A delivery after the deadline closed expires the key.
            let now = controller.host().clock.now();
            if until.is_some_and(|until| now >= until) {
                expire_signals(controller, &record, &signals);
                return SignalResult::Closed;
            }
            let memo = format!("pi.signal.{signal}/{key}");
            let mut memos = record.memos.clone();
            if memos.get(&memo).is_some_and(|state| {
                state.get("value").is_some() || state.get("expired") == Some(&Value::Bool(true))
            }) {
                return SignalResult::Duplicate;
            }
            memos.insert(memo, serde_json::json!({ "version": 1, "value": value }));
            let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
                memos,
                ..record.clone()
            });
            SignalResult::Delivered
        }
        // A task not waiting still accepts a delivery (upstream: the
        // value waits in the task; its wait then wakes at once).
        RecordState::Pending { .. }
        | RecordState::Running { .. }
        | RecordState::Sleeping { .. } => {
            let memo = format!("pi.signal.{signal}/{key}");
            let mut memos = record.memos.clone();
            if memos.get(&memo).is_some_and(|state| {
                state.get("value").is_some() || state.get("expired") == Some(&Value::Bool(true))
            }) {
                return SignalResult::Duplicate;
            }
            memos.insert(memo, serde_json::json!({ "version": 1, "value": value }));
            let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
                memos,
                ..record.clone()
            });
            SignalResult::Delivered
        }
    }
}

/// Parks a tool call (slice 6): a task that waits on the park's signals
/// (or deadline), resuming the run when it wakes — the run's replay then
/// reruns the replay-safe call, whose execute reads what it waited for
/// and returns the result (upstream's parked ToolTask).
pub(super) async fn park_tool(
    controller: &mut SessionController<SdkHost>,
    park: crate::tools::ToolPark,
) {
    // The park is a task: a definition-free wait the driver holds.
    let id = noel_core::controller::new_id();
    let signals = park
        .signals
        .iter()
        .map(|(signal, key)| serde_json::json!({ "signal": signal, "key": key }))
        .collect::<Vec<_>>();
    let state = serde_json::to_value(RecordState::WaitingOnSignals {
        checkpoint: serde_json::json!({
            "kind": "tool-park",
            "call_id": park.call_id,
            "tool": park.tool,
            "checkpoint": park.checkpoint,
        }),
        until: park.until,
        signals,
    })
    .expect("a task state serializes");
    let _ = controller_mut_of(controller).save_task(DurableTaskRecord {
        id: id.clone(),
        kind: "noel.tool-park".into(),
        version: 1,
        input: park.arguments,
        state,
        background: false,
        abort_requested: false,
        restarts: 0,
        memos: Default::default(),
    });
    // The run the park interrupted stays open (it was cancelled by the
    // park itself); a host's wait_task on the park, or a wake poll,
    // resumes it once woken. Nothing more here: the run's cancellation
    // already ended this pass.
}

/// The earliest deadline of the session's sleeping tasks (the harness's
/// wake): a host arms its own wakeup with it (upstream `onWake` hosts,
/// like a Durable Object alarm).
pub(super) fn wake(controller: &SessionController<SdkHost>) -> Option<u64> {
    controller
        .runtime()
        .session()
        .tasks
        .values()
        .filter_map(|record| match record_state(record) {
            RecordState::Sleeping { until, .. } => Some(until),
            _ => None,
        })
        .min()
}

fn record_state(record: &DurableTaskRecord) -> RecordState {
    serde_json::from_value(record.state.clone()).unwrap_or(RecordState::Pending {
        checkpoint: Value::Null,
        wake: None,
    })
}

fn phase_of(checkpoint: &Value) -> String {
    checkpoint
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn outcome_public(outcome: RecordOutcome) -> TaskOutcome {
    match outcome {
        RecordOutcome::Completed { result } => TaskOutcome::Completed { result },
        RecordOutcome::Failed { error } => TaskOutcome::Failed { error },
        RecordOutcome::Aborted => TaskOutcome::Aborted,
        RecordOutcome::RestartLimit => TaskOutcome::RestartLimit,
        RecordOutcome::Orphaned => TaskOutcome::Orphaned,
    }
}

fn public(record: &DurableTaskRecord, state: TaskState) -> TaskRecord {
    TaskRecord {
        id: record.id.clone(),
        conversation_id: String::new(),
        kind: record.kind.clone(),
        version: record.version,
        input: record.input.clone(),
        state,
        background: record.background,
        abort_requested: record.abort_requested,
        restarts: record.restarts,
        memos: record.memos.clone(),
    }
}
