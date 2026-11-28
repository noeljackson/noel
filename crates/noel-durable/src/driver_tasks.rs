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

    pub(crate) fn save_task_with_documents(
        &mut self,
        record: DurableTaskRecord,
        documents: Vec<noel_core::DurableDocumentWrite>,
    ) -> Result<(), noel_core::SessionError> {
        self.controller
            .configure(|runtime, _| runtime.save_task_with_documents(record, documents))
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
    Completing {
        outcome: RecordOutcome,
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
pub(super) fn create_owned(
    controller: &mut SessionController<SdkHost>,
    kind: &str,
    input: Value,
    background: bool,
    owner_task_id: Option<String>,
) -> Result<String, String> {
    if !controller.host().tasks.erased.contains_key(kind) {
        return Err(format!("no task named {kind} is registered"));
    }
    if let Some(owner) = &owner_task_id {
        let Some(record) = controller.runtime().session().tasks.get(owner) else {
            return Err(format!("no owner task {owner}"));
        };
        if matches!(record_state(record), RecordState::Terminal { .. }) {
            return Err(format!("owner task {owner} is terminal"));
        }
    }
    let id = noel_core::controller::new_id();
    let record = DurableTaskRecord {
        id: id.clone(),
        kind: kind.to_owned(),
        version: 1,
        input,
        owner_task_id,
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

/// Waits under one of Durable's child join policies.
pub(super) async fn join(
    controller: &mut SessionController<SdkHost>,
    task_ids: &[String],
    policy: crate::tools::JoinPolicy,
) -> Vec<TaskRecord> {
    if task_ids.is_empty() {
        return Vec::new();
    }
    match policy {
        crate::tools::JoinPolicy::Any => {
            if let Some(record) = task_ids.iter().find_map(|id| {
                controller
                    .runtime()
                    .session()
                    .tasks
                    .get(id)
                    .filter(|record| matches!(record_state(record), RecordState::Terminal { .. }))
                    .cloned()
            }) {
                let outcome = match record_state(&record) {
                    RecordState::Terminal { outcome } => outcome_public(outcome),
                    _ => unreachable!(),
                };
                return vec![public(&record, TaskState::Terminal { outcome })];
            }
            vec![Box::pin(run_to_terminal(controller, &task_ids[0])).await]
        }
        crate::tools::JoinPolicy::AllSettled => {
            let mut records = Vec::with_capacity(task_ids.len());
            for id in task_ids {
                records.push(Box::pin(run_to_terminal(controller, id)).await);
            }
            records
        }
        crate::tools::JoinPolicy::FailFast => {
            let mut records = Vec::with_capacity(task_ids.len());
            for (index, id) in task_ids.iter().enumerate() {
                let record = Box::pin(run_to_terminal(controller, id)).await;
                let failed = !matches!(
                    record.state,
                    TaskState::Terminal {
                        outcome: TaskOutcome::Completed { .. }
                    }
                );
                records.push(record);
                if failed {
                    for rest in &task_ids[index + 1..] {
                        let _ = abort(controller, rest).await;
                        records.push(Box::pin(run_to_terminal(controller, rest)).await);
                    }
                    break;
                }
            }
            records
        }
    }
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
            RecordState::Completing { outcome } => {
                let children = child_ids(controller, &record.id, false);
                for child in children {
                    let _ = Box::pin(run_to_terminal(controller, &child)).await;
                }
                settle(controller, &record, outcome.clone());
                return public(
                    controller
                        .runtime()
                        .session()
                        .tasks
                        .get(task_id)
                        .unwrap_or(&record),
                    TaskState::Terminal {
                        outcome: outcome_public(outcome),
                    },
                );
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
    if !controller.runtime().session().tasks.contains_key(task_id) {
        return Err(format!("no task {task_id}"));
    }
    let mut tree = Vec::new();
    let mut frontier = child_ids(controller, task_id, false);
    while let Some(next) = frontier.pop() {
        frontier.extend(child_ids(controller, &next, false));
        tree.push(next);
    }
    for child in tree.into_iter().rev() {
        abort_one(controller, &child).await?;
    }
    abort_one(controller, task_id).await
}

async fn abort_one(
    controller: &mut SessionController<SdkHost>,
    task_id: &str,
) -> Result<(), String> {
    let Some(mut record) = controller.runtime().session().tasks.get(task_id).cloned() else {
        return Err(format!("no task {task_id}"));
    };
    if matches!(record_state(&record), RecordState::Terminal { .. }) {
        return Ok(());
    }
    let checkpoint = record
        .state
        .get("checkpoint")
        .cloned()
        .unwrap_or(Value::Null);
    record.abort_requested = true;
    controller_mut_of(controller)
        .save_task(record.clone())
        .map_err(|error| error.to_string())?;
    invoke(controller, &record, checkpoint, true).await;
    Ok(())
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
        // Suspendable tool calls have their own recovery path: the model
        // re-enters the call by call id and the tool registry resumes its
        // committed park. A task definition never owns these records.
        if record.kind.starts_with("pi.tool/") {
            continue;
        }
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
    let (actions, mut action) = mpsc::unbounded_channel::<InvocationAction>();
    let owner = record.id.clone();
    let runtime = TaskRuntime::new(
        {
            let actions = actions.clone();
            Arc::new(move |commit: TaskCommit, memos, documents| {
                let actions = actions.clone();
                Box::pin(async move {
                    let (reply, done) = tokio::sync::oneshot::channel();
                    if actions
                        .send(InvocationAction::Commit {
                            commit,
                            memos,
                            documents,
                            reply,
                        })
                        .is_ok()
                    {
                        let _ = done.await;
                    }
                })
            })
        },
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
        {
            let actions = actions.clone();
            let owner = owner.clone();
            Arc::new(move |kind: String, input: Value, background: bool| {
                let actions = actions.clone();
                let owner = owner.clone();
                Box::pin(async move {
                    let (reply, done) = tokio::sync::oneshot::channel();
                    actions
                        .send(InvocationAction::Create {
                            owner,
                            kind,
                            input,
                            background,
                            reply,
                        })
                        .map_err(|_| "the task invocation ended".to_string())?;
                    done.await
                        .map_err(|_| "the task invocation ended".to_string())?
                }) as crate::tasks::CreateFuture
            })
        },
        {
            let actions = actions.clone();
            Arc::new(move |task_id: String| {
                let actions = actions.clone();
                Box::pin(async move {
                    let (reply, done) = tokio::sync::oneshot::channel();
                    if actions
                        .send(InvocationAction::Wait {
                            task_id: task_id.clone(),
                            reply,
                        })
                        .is_err()
                    {
                        return TaskRecord::orphaned(&task_id);
                    }
                    done.await
                        .unwrap_or_else(|_| TaskRecord::orphaned(&task_id))
                }) as crate::tasks::RecordFuture
            })
        },
        {
            let actions = actions.clone();
            Arc::new(move |task_id: String| {
                let actions = actions.clone();
                Box::pin(async move {
                    let (reply, done) = tokio::sync::oneshot::channel();
                    actions
                        .send(InvocationAction::Abort { task_id, reply })
                        .map_err(|_| "the task invocation ended".to_string())?;
                    done.await
                        .map_err(|_| "the task invocation ended".to_string())?
                }) as crate::tasks::UnitFuture
            })
        },
        {
            let actions = actions.clone();
            Arc::new(move |document: String, key: String| {
                let actions = actions.clone();
                Box::pin(async move {
                    let (reply, done) = tokio::sync::oneshot::channel();
                    if actions
                        .send(InvocationAction::ReadDocument {
                            document,
                            key,
                            reply,
                        })
                        .is_err()
                    {
                        return None;
                    }
                    done.await.ok().flatten()
                }) as crate::tasks::ReadFuture
            })
        },
    );
    drop(actions);

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
        let next = action.recv().await;
        match next {
            Some(InvocationAction::Commit {
                commit,
                memos,
                documents,
                reply,
            }) => {
                let memos = apply(controller, record, commit, memos, documents);
                let _ = reply.send(memos);
            }
            Some(InvocationAction::Create {
                owner,
                kind,
                input,
                background,
                reply,
            }) => {
                let result = create_owned(controller, &kind, input, background, Some(owner));
                let _ = reply.send(result);
            }
            Some(InvocationAction::Wait { task_id, reply }) => {
                let record = Box::pin(run_to_terminal(controller, &task_id)).await;
                let _ = reply.send(record);
            }
            Some(InvocationAction::Abort { task_id, reply }) => {
                let result = Box::pin(abort(controller, &task_id)).await;
                let _ = reply.send(result);
            }
            Some(InvocationAction::ReadDocument {
                document,
                key,
                reply,
            }) => {
                let value = controller
                    .runtime()
                    .session()
                    .documents
                    .get(&document)
                    .and_then(|document| document.get(&key))
                    .cloned();
                let _ = reply.send(value);
            }
            None => finished = true,
        }
    }
    let _ = invocation.await;
}

type OneshotReply = tokio::sync::oneshot::Sender<std::collections::BTreeMap<String, Value>>;

enum InvocationAction {
    Commit {
        commit: TaskCommit,
        memos: std::collections::BTreeMap<String, Value>,
        documents: Vec<noel_core::DurableDocumentWrite>,
        reply: OneshotReply,
    },
    Create {
        owner: String,
        kind: String,
        input: Value,
        background: bool,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Wait {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<TaskRecord>,
    },
    Abort {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    ReadDocument {
        document: String,
        key: String,
        reply: tokio::sync::oneshot::Sender<Option<Value>>,
    },
}

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
    memos: std::collections::BTreeMap<String, Value>,
    documents: Vec<noel_core::DurableDocumentWrite>,
) -> std::collections::BTreeMap<String, Value> {
    let document_count = documents.len();
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
        TaskCommit::Completed(result) => {
            terminal_or_completing(controller, &record.id, RecordOutcome::Completed { result })
        }
        TaskCommit::Failed(error) => {
            terminal_or_completing(controller, &record.id, RecordOutcome::Failed { error })
        }
        TaskCommit::Aborted => {
            terminal_or_completing(controller, &record.id, RecordOutcome::Aborted)
        }
    };
    let state = serde_json::to_value(state).expect("a task state serializes");
    let _ = controller_mut_of(controller).save_task_with_documents(
        DurableTaskRecord {
            state,
            memos: memos.clone(),
            ..record.clone()
        },
        documents,
    );
    if document_count > 0 {
        controller
            .host()
            .events
            .record(crate::OwnedEvent::DocumentsCommitted {
                writes: document_count,
            });
    }
    memos
}

fn terminal_or_completing(
    controller: &SessionController<SdkHost>,
    task_id: &str,
    outcome: RecordOutcome,
) -> RecordState {
    if child_ids(controller, task_id, true).is_empty() {
        RecordState::Terminal { outcome }
    } else {
        RecordState::Completing { outcome }
    }
}

fn child_ids(controller: &SessionController<SdkHost>, owner: &str, live_only: bool) -> Vec<String> {
    controller
        .runtime()
        .session()
        .tasks
        .values()
        .filter(|task| task.owner_task_id.as_deref() == Some(owner))
        .filter(|task| !live_only || !matches!(record_state(task), RecordState::Terminal { .. }))
        .map(|task| task.id.clone())
        .collect()
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
        RecordState::Terminal { .. } | RecordState::Completing { .. } => SignalResult::Closed,
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

pub(crate) fn record_state(record: &DurableTaskRecord) -> RecordState {
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
        owner_task_id: record.owner_task_id.clone(),
        state,
        background: record.background,
        abort_requested: record.abort_requested,
        restarts: record.restarts,
        memos: record.memos.clone(),
    }
}

pub(super) fn public_record(record: &DurableTaskRecord) -> TaskRecord {
    let state = match record_state(record) {
        RecordState::Pending { checkpoint, wake } => TaskState::Pending {
            checkpoint,
            wake: wake.and_then(|wake| serde_json::from_value(wake).ok()),
        },
        RecordState::Running { checkpoint, wake } => TaskState::Running {
            checkpoint,
            wake: wake.and_then(|wake| serde_json::from_value(wake).ok()),
        },
        RecordState::Sleeping { checkpoint, until } => TaskState::Sleeping { checkpoint, until },
        RecordState::WaitingOnSignals {
            checkpoint,
            until,
            signals,
        } => TaskState::WaitingOnSignals {
            checkpoint,
            until,
            signals,
        },
        RecordState::Completing { outcome } => TaskState::Completing {
            outcome: outcome_public(outcome),
        },
        RecordState::Terminal { outcome } => TaskState::Terminal {
            outcome: outcome_public(outcome),
        },
    };
    public(record, state)
}

pub(super) fn inspect(
    session: &noel_core::SessionState,
    registry: &crate::TaskRegistry,
    queued_submissions: usize,
    read_only: bool,
) -> crate::HarnessInspection {
    let tasks = session
        .tasks
        .values()
        .filter_map(|record| {
            let public = public_record(record);
            if matches!(public.state, TaskState::Terminal { .. }) {
                return None;
            }
            let state = if !registry.erased.contains_key(&record.kind)
                && !record.kind.starts_with("pi.tool/")
            {
                crate::TaskInspectionState::Blocked {
                    reason: crate::BlockedReason::MissingTask,
                }
            } else {
                match public.state {
                    TaskState::Pending { .. } => crate::TaskInspectionState::Pending,
                    TaskState::Running { .. } => crate::TaskInspectionState::Running,
                    TaskState::Sleeping { until, .. } => {
                        crate::TaskInspectionState::Sleeping { until: Some(until) }
                    }
                    TaskState::WaitingOnSignals { until, .. } => {
                        crate::TaskInspectionState::Sleeping { until }
                    }
                    TaskState::Completing { .. } => crate::TaskInspectionState::Completing,
                    TaskState::Terminal { .. } => unreachable!(),
                }
            };
            Some(crate::TaskInspection {
                record: public,
                state,
            })
        })
        .collect();
    crate::HarnessInspection {
        scheduling: if read_only {
            crate::SchedulingState::Paused
        } else {
            crate::SchedulingState::Running
        },
        tasks,
        queued_submissions,
    }
}

pub(super) fn task_graph(
    session: &noel_core::SessionState,
    registry: &crate::TaskRegistry,
) -> crate::TaskGraph {
    let tasks = session
        .tasks
        .values()
        .filter_map(|record| {
            let public = public_record(record);
            let state = crate::inspection::graph_state(&public.state)?;
            let blocked = (!registry.erased.contains_key(&record.kind)
                && !record.kind.starts_with("pi.tool/"))
            .then_some(crate::BlockedReason::MissingTask);
            Some((
                record.id.clone(),
                crate::TaskGraphNode {
                    id: record.id.clone(),
                    kind: record.kind.clone(),
                    conversation_id: public.conversation_id,
                    owner: record.owner_task_id.clone(),
                    background: record.background,
                    abort_requested: record.abort_requested,
                    state,
                    blocked,
                },
            ))
        })
        .collect();
    crate::TaskGraph { tasks }
}
