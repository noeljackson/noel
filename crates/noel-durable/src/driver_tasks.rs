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
    Pending { checkpoint: Value },
    Running { checkpoint: Value },
    Sleeping { checkpoint: Value, until: u64 },
    Terminal { outcome: RecordOutcome },
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
            RecordState::Pending { checkpoint } | RecordState::Running { checkpoint } => {
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
        RecordState::Pending { checkpoint }
        | RecordState::Running { checkpoint }
        | RecordState::Sleeping { checkpoint, .. } => {
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
        if let RecordState::Running { checkpoint } = record_state(&record) {
            let restarts = record.restarts + 1;
            if restarts >= restart_limit {
                let state = serde_json::to_value(RecordState::Terminal {
                    outcome: RecordOutcome::RestartLimit,
                })
                .expect("a task state serializes");
                let _ =
                    controller_mut_of(controller).save_task(DurableTaskRecord { state, ..record });
            } else {
                let state = serde_json::to_value(RecordState::Pending { checkpoint })
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
    // Mark running (one record).
    let running = DurableTaskRecord {
        state: serde_json::to_value(RecordState::Running {
            checkpoint: checkpoint.clone(),
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
        TaskCommit::Continue(checkpoint) => RecordState::Pending { checkpoint },
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
