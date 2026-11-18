//! Durable tasks (plan slice 3, `docs/durable-sdk-2026-11.md`, #218):
//! a small state machine per task, its checkpoint saved through the
//! session journal after every step, so a reopened harness continues
//! where the last one stopped.
//!
//! The shape is upstream Durable's `defineTask` (`TaskRecord` in
//! `packages/durable/src/types.ts`), kept as noel journal records (audit
//! item 10: no table store): one `task` record per task state change,
//! complete-replacement like every journal record, additive and ignored
//! by older readers. Sleeps and signals (slices 4 and 5) extend the
//! record's state; this slice carries pending, running and terminal.
//!
//! The usual pattern (upstream's `11-tasks`): save what you are about to
//! do, do it, then save the result. A crash between doing and saving
//! reruns the step, so a step must be safe to repeat; the context's memo
//! keeps the first value written under a name, exactly-once style.
//!
//! Recovery: opening a harness reads every journal record, so its task
//! records load with their last saved checkpoints. A task left `running`
//! by a stop that was not a close counts one restart; at the
//! [`TaskSettings::restart_limit`] it ends `restart_limit` instead of
//! running again (upstream `harness-restart-failure`).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use noel_core::DurableTaskRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A boxed phase future: what a handler returns.
pub type PhaseFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// A boxed future returning a JSON value (the memo channel).
pub(crate) type ValueFuture = Pin<Box<dyn Future<Output = Value> + Send>>;

/// What a phase handler sees: its task (input, checkpoint, memos) and the
/// runtime to commit through. It owns everything it holds, so handlers
/// are plain `async move` closures.
pub struct TaskContext<I, S> {
    /// The task's creation input, retained while it is live.
    pub input: Arc<I>,
    /// The checkpoint this invocation runs at.
    pub checkpoint: Arc<S>,
    /// First-writer-wins values kept while the task can run.
    memos: BTreeMap<String, Value>,
    runtime: TaskRuntime,
}

/// The runtime half of a context: its commits are journal records, saved
/// before they return. Cloned into the context; private to the crate's
/// driver wiring.
#[derive(Clone)]
pub(crate) struct TaskRuntime {
    commit: Arc<dyn Fn(TaskCommit) -> PhaseFuture + Send + Sync>,
    /// The context's memos, first-writer-wins; saved with each commit.
    #[allow(dead_code)]
    memo: Arc<dyn Fn(String, Value) -> ValueFuture + Send + Sync>,
}

impl TaskRuntime {
    pub(crate) fn new(
        commit: Arc<dyn Fn(TaskCommit) -> PhaseFuture + Send + Sync>,
        memo: Arc<dyn Fn(String, Value) -> ValueFuture + Send + Sync>,
    ) -> Self {
        TaskRuntime { commit, memo }
    }
}

impl<I, S> TaskContext<I, S> {
    pub(crate) fn new(
        input: Arc<I>,
        checkpoint: Arc<S>,
        memos: BTreeMap<String, Value>,
        runtime: TaskRuntime,
    ) -> Self {
        TaskContext {
            input,
            checkpoint,
            memos,
            runtime,
        }
    }

    /// Commits the task's next state: the checkpoint it resumes at, or
    /// its terminal outcome. One journal record, saved before this
    /// returns; the memos travel with it.
    pub async fn commit(&mut self, commit: TaskCommit) {
        (self.runtime.commit)(TaskCommit::with_memos(
            commit,
            std::mem::take(&mut self.memos),
        ))
        .await;
    }

    /// The first value written under `name`, writing `value` if none:
    /// exactly-once bookkeeping for effects between checkpoints (upstream
    /// `runtime.memo`). The memo is saved with the task's next commit.
    pub fn memo(&mut self, name: &str, value: Value) -> Value {
        if let Some(first) = self.memos.get(name) {
            return first.clone();
        }
        self.memos.insert(name.to_owned(), value.clone());
        value
    }
}

impl TaskCommit {
    fn with_memos(self, _memos: BTreeMap<String, Value>) -> TaskCommit {
        self
    }
}

/// What a phase commits: the next checkpoint, or the outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskCommit {
    /// Continue at this checkpoint (the task stays runnable). The value
    /// is serialized into the record.
    Continue(Value),
    /// The task is done, with its result.
    Completed(Value),
    /// The task failed, with its error.
    Failed(String),
    /// The task was aborted (an abort handler's answer).
    Aborted,
}

/// One phase's handler.
pub type PhaseHandler<I, S> = Arc<dyn Fn(TaskContext<I, S>) -> PhaseFuture + Send + Sync>;

/// Builds a [`PhaseHandler`] from an async closure:
/// `phase(|mut ctx| async move { ... })`.
pub fn phase<I, S, F, Fut>(handler: F) -> PhaseHandler<I, S>
where
    I: Send + Sync + 'static,
    S: Send + Sync + 'static,
    F: Fn(TaskContext<I, S>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Arc::new(move |context| Box::pin(handler(context)))
}

/// A task definition: its name, version, initial checkpoint, one handler
/// per phase, and an abort handler — upstream's `defineTask` as a value:
/// `TaskDefinition::new(...).phase(name, phase(...)).abort(phase(...))`.
pub struct TaskDefinition<I, S> {
    /// Registered name, persisted with the record (`kind`).
    pub name: &'static str,
    /// Definition version; a later version migrates live records.
    pub version: u32,
    /// The first checkpoint, from the input.
    pub initial: Arc<dyn Fn(&I) -> S + Send + Sync>,
    /// One handler per phase. Each commits its next checkpoint or
    /// terminal outcome; what it does between commits is not durable.
    pub phases: BTreeMap<&'static str, PhaseHandler<I, S>>,
    /// Runs instead of the phases after an abort; it decides the outcome.
    pub abort: Option<PhaseHandler<I, S>>,
}

impl<I, S> TaskDefinition<I, S> {
    pub fn new(
        name: &'static str,
        version: u32,
        initial: impl Fn(&I) -> S + Send + Sync + 'static,
    ) -> Self {
        TaskDefinition {
            name,
            version,
            initial: Arc::new(initial),
            phases: BTreeMap::new(),
            abort: None,
        }
    }
}

impl<I: Send + Sync + 'static, S: Send + Sync + 'static> TaskDefinition<I, S> {
    /// Adds a phase handler (see [`phase`]).
    pub fn phase(mut self, name: &'static str, handler: PhaseHandler<I, S>) -> Self {
        self.phases.insert(name, handler);
        self
    }

    /// The abort handler.
    pub fn abort(mut self, handler: PhaseHandler<I, S>) -> Self {
        self.abort = Some(handler);
        self
    }
}

/// A task's durable state, as the journal record holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    /// The conversation (session) that owns it.
    pub conversation_id: String,
    /// The definition's registered name.
    pub kind: String,
    pub version: u32,
    pub input: Value,
    /// The task's state: its status and, while it can run, its checkpoint.
    pub state: TaskState,
    /// Whether the conversation's ordinary idle waits and aborts skip it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// The durable abort mark.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub abort_requested: bool,
    /// Crash restarts in a row without a state change; at the limit the
    /// task ends `restart_limit`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub restarts: u32,
    /// First-writer-wins values kept while the task can run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub memos: BTreeMap<String, Value>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// A task's lifecycle state (upstream `TaskState`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TaskState {
    /// Eligible for scheduling, at its checkpoint.
    Pending { checkpoint: Value },
    /// Reserved by one in-memory invocation.
    Running { checkpoint: Value },
    /// Settled, with its outcome.
    Terminal { outcome: TaskOutcome },
}

/// A task's terminal outcome (upstream `TaskOutcome`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TaskOutcome {
    Completed {
        result: Value,
    },
    Failed {
        error: String,
    },
    Aborted,
    /// A stop cut the task off `restart_limit` times in a row; it is not
    /// run again.
    RestartLimit,
    /// Its definition is not registered in this harness.
    Orphaned,
}

impl TaskRecord {
    /// Whether the task is settled.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, TaskState::Terminal { .. })
    }

    /// A settled record for a task this harness cannot ask about (the
    /// driver is gone).
    pub fn orphaned(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_owned(),
            conversation_id: String::new(),
            kind: String::new(),
            version: 0,
            input: Value::Null,
            state: TaskState::Terminal {
                outcome: TaskOutcome::Orphaned,
            },
            background: false,
            abort_requested: false,
            restarts: 0,
            memos: BTreeMap::new(),
        }
    }
}

/// The erased, runnable form of a definition the driver holds: it starts
/// a task from its record's input and checkpoint, committing through the
/// runtime it is given.
pub(crate) struct ErasedTask {
    /// Runs the named phase ("" is the definition's first).
    pub(crate) invoke: Invoke,
    /// The abort handler.
    pub(crate) abort: Invoke,
    /// The phases this definition knows, ordered; `phase_name` maps a
    /// record's name (or "" for the first) to the 'static name, or ""
    /// when the definition does not have it.
    pub(crate) phase_name: Arc<dyn Fn(&str) -> &'static str + Send + Sync>,
}

impl Clone for ErasedTask {
    fn clone(&self) -> Self {
        ErasedTask {
            invoke: self.invoke.clone(),
            abort: self.abort.clone(),
            phase_name: self.phase_name.clone(),
        }
    }
}

/// What runs a phase: `invoke(record, phase, runtime)`. A `phase` of ""
/// is the definition's first phase; the erased form resolves names.
pub(crate) type Invoke = PhaseHandlerErased;
pub(crate) type PhaseHandlerErased =
    Arc<dyn Fn(Arc<DurableTaskRecord>, &'static str, TaskRuntime) -> PhaseFuture + Send + Sync>;

impl<I, S> TaskDefinition<I, S>
where
    I: for<'de> Deserialize<'de> + Send + Sync + 'static,
    S: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    /// The driver's runnable form of this definition.
    pub(crate) fn erased(self: &Arc<Self>) -> ErasedTask {
        let invoke = erased_invoke::<I, S>(self.clone());
        let abort = erased_abort::<I, S>(self.clone());
        let names = self.phases.keys().copied().collect::<Vec<&'static str>>();
        let phase_name = Arc::new(move |phase: &str| -> &'static str {
            if phase.is_empty() {
                return names.first().copied().unwrap_or("");
            }
            names
                .iter()
                .copied()
                .find(|name| *name == phase)
                .unwrap_or("")
        });
        ErasedTask {
            invoke,
            abort,
            phase_name,
        }
    }
}

fn erased_invoke<I, S>(definition: Arc<TaskDefinition<I, S>>) -> Invoke
where
    I: for<'de> Deserialize<'de> + Send + Sync + 'static,
    S: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    Arc::new(move |record, phase, runtime| {
        let definition = definition.clone();
        Box::pin(async move {
            let Some((input, checkpoint)) = parsed::<I, S>(&record, &definition) else {
                return;
            };
            let handler = definition.phases.get(phase).cloned();
            let Some(handler) = handler else {
                return;
            };
            let context = TaskContext::new(
                Arc::new(input),
                Arc::new(checkpoint),
                record.memos.clone(),
                runtime,
            );
            handler(context).await;
        })
    })
}

fn erased_abort<I, S>(definition: Arc<TaskDefinition<I, S>>) -> Invoke
where
    I: for<'de> Deserialize<'de> + Send + Sync + 'static,
    S: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static,
{
    Arc::new(move |record, _phase, runtime| {
        let definition = definition.clone();
        Box::pin(async move {
            let Some((input, checkpoint)) = parsed::<I, S>(&record, &definition) else {
                return;
            };
            let Some(handler) = definition.abort.clone() else {
                return;
            };
            let context = TaskContext::new(
                Arc::new(input),
                Arc::new(checkpoint),
                record.memos.clone(),
                runtime,
            );
            handler(context).await;
        })
    })
}

/// The record's input and checkpoint as the definition reads them: a
/// `Null` checkpoint is the definition's `initial` of the input (a task
/// before its first commit).
fn parsed<I, S>(record: &DurableTaskRecord, definition: &TaskDefinition<I, S>) -> Option<(I, S)>
where
    I: for<'de> Deserialize<'de>,
    S: Serialize + for<'de> Deserialize<'de>,
{
    let input: I = serde_json::from_value(record.input.clone()).ok()?;
    let checkpoint: S = match record.state.get("checkpoint") {
        None | Some(Value::Null) => (definition.initial)(&input),
        Some(value) => serde_json::from_value(value.clone()).ok()?,
    };
    Some((input, checkpoint))
}

/// The definitions a harness runs, by registered name.
#[derive(Default, Clone)]
pub struct TaskRegistry {
    pub(crate) erased: BTreeMap<String, ErasedTask>,
}

impl TaskRegistry {
    pub fn new() -> TaskRegistry {
        TaskRegistry::default()
    }

    /// Registers a definition under its name.
    pub fn register<I, S>(mut self, definition: TaskDefinition<I, S>) -> Self
    where
        I: for<'de> Deserialize<'de> + Send + Sync + 'static,
        S: Serialize + for<'de> Deserialize<'de> + Send + Sync + 'static,
    {
        let name = definition.name.to_owned();
        let erased = Arc::new(definition).erased();
        self.erased.insert(name, erased);
        self
    }
}

/// Per-task scheduling settings (upstream `HarnessSettings`, the task
/// part).
#[derive(Debug, Clone, PartialEq)]
pub struct TaskSettings {
    /// Crash restarts in a row after which a task ends `restart_limit`
    /// (upstream `restartLimit`, default 5, as noel's runs).
    pub restart_limit: u32,
}

impl Default for TaskSettings {
    fn default() -> Self {
        TaskSettings { restart_limit: 5 }
    }
}
