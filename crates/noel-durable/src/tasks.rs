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
    /// The wake this invocation resumed at, if any.
    wake: Option<WakeReason>,
    /// First-writer-wins values kept while the task can run.
    memos: BTreeMap<String, Value>,
    runtime: TaskRuntime,
}

/// The runtime half of a context: its commits are journal records, saved
/// before they return. Cloned into the context; private to the crate's
/// driver wiring.
#[derive(Clone)]
pub(crate) struct TaskRuntime {
    commit: Arc<dyn Fn(TaskCommit, BTreeMap<String, Value>) -> PhaseFuture + Send + Sync>,
    /// The context's memos, first-writer-wins; saved with each commit.
    #[allow(dead_code)]
    memo: Arc<dyn Fn(String, Value) -> ValueFuture + Send + Sync>,
}

impl TaskRuntime {
    pub(crate) fn new(
        commit: Arc<dyn Fn(TaskCommit, BTreeMap<String, Value>) -> PhaseFuture + Send + Sync>,
        memo: Arc<dyn Fn(String, Value) -> ValueFuture + Send + Sync>,
    ) -> Self {
        TaskRuntime { commit, memo }
    }
}

impl<I, S> TaskContext<I, S> {
    pub(crate) fn new(
        input: Arc<I>,
        checkpoint: Arc<S>,
        wake: Option<WakeReason>,
        memos: BTreeMap<String, Value>,
        runtime: TaskRuntime,
    ) -> Self {
        TaskContext {
            input,
            checkpoint,
            wake,
            memos,
            runtime,
        }
    }

    /// Commits the task's next state: the checkpoint it resumes at, or
    /// its terminal outcome. One journal record, saved before this
    /// returns; the memos travel with it.
    pub async fn commit(&mut self, commit: TaskCommit) {
        (self.runtime.commit)(commit, std::mem::take(&mut self.memos)).await;
    }

    /// Sleeps until `until` on the harness clock, committing
    /// `checkpoint` to resume at (upstream `runtime.sleepUntil`).
    pub async fn sleep_until(&mut self, until: u64, checkpoint: Value) {
        self.commit(TaskCommit::Sleep { until, checkpoint }).await;
    }

    /// Waits for a signal: parks until one of `signals` holds a value no
    /// wake used, or `until` passes, resuming at `checkpoint` with the
    /// wake as its reason (upstream sleeps with `signals`).
    pub async fn wait_for<T>(
        &mut self,
        signals: &[SignalKey<T>],
        until: Option<u64>,
        checkpoint: Value,
    ) {
        self.commit(TaskCommit::Wait {
            until,
            signals: signals
                .iter()
                .map(|key| (key.signal.to_owned(), key.key.clone()))
                .collect(),
            checkpoint,
        })
        .await;
    }

    /// The wake this invocation resumed at, if any (upstream
    /// `task.state.wake`): a phase reads it to know why it runs.
    pub fn wake(&self) -> Option<&WakeReason> {
        self.wake.as_ref()
    }

    /// A signal key's committed state (upstream `runtime.readSignal`):
    /// the value delivered under it, `Expired` if its deadline passed
    /// first, or `None` while nothing holds it.
    pub async fn read_signal<T: serde::de::DeserializeOwned>(
        &mut self,
        token: &SignalToken<T>,
        key: &str,
    ) -> Option<SignalState<T>> {
        let name = token.memo_name(key);
        let state = self.memos.get(&name).cloned()?;
        if state.get("expired") == Some(&Value::Bool(true)) {
            return Some(SignalState::Expired);
        }
        let value = state.get("value")?.clone();
        serde_json::from_value(value).ok().map(SignalState::Value)
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

/// What a phase commits: the next checkpoint, a sleep, or the outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskCommit {
    /// Continue at this checkpoint (the task stays runnable). The value
    /// is serialized into the record.
    Continue(Value),
    /// Sleep until `until` on the harness clock, resuming at
    /// `checkpoint` once due (upstream `runtime.sleepUntil`). The sleep
    /// is stored with the checkpoint, so the task holds no invocation
    /// meanwhile, and a harness opened later continues it once its time
    /// has come.
    Sleep { until: u64, checkpoint: Value },
    /// Wait for a signal: park until one of `signals` holds a value no
    /// wake used, or `until` passes, whichever comes first, resuming at
    /// `checkpoint` with the wake as its reason (upstream sleeps with
    /// `signals`). Needs a deadline or at least one key.
    Wait {
        until: Option<u64>,
        signals: Vec<(String, String)>,
        checkpoint: Value,
    },
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
///
/// The checkpoint type `S` must round-trip the JSON the phases commit
/// (as `Serialize` writes it, so `Deserialize` reads it back), and a
/// checkpoint a handler resumes at should carry its phase as a `phase`
/// string field, which the engine routes on. A struct with a `phase`
/// field is the usual shape; an externally-tagged enum is not, because
/// the commits are plain objects.
pub struct TaskDefinition<I, S> {
    /// Registered name, persisted with the record (`kind`).
    pub name: &'static str,
    /// Definition version; a later version migrates live records.
    pub version: u32,
    /// The first checkpoint, from the input.
    pub initial: Arc<dyn Fn(&I) -> S + Send + Sync>,
    /// One handler per phase, in declaration order (the first is the
    /// entry phase). Each commits its next checkpoint or terminal
    /// outcome; what it does between commits is not durable.
    pub phases: Vec<(&'static str, PhaseHandler<I, S>)>,
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
            phases: Vec::new(),
            abort: None,
        }
    }
}

impl<I: Send + Sync + 'static, S: Send + Sync + 'static> TaskDefinition<I, S> {
    /// Adds a phase handler (see [`phase`]); the first added is the
    /// entry phase.
    pub fn phase(mut self, name: &'static str, handler: PhaseHandler<I, S>) -> Self {
        self.phases.push((name, handler));
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
    /// Eligible for scheduling, at its checkpoint; `wake` is why it last
    /// resumed.
    Pending {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<WakeReason>,
    },
    /// Reserved by one in-memory invocation.
    Running {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wake: Option<WakeReason>,
    },
    /// Parked without an invocation until the harness clock reaches
    /// `until`, then resuming at `checkpoint` (upstream `sleeping`).
    Sleeping { checkpoint: Value, until: u64 },
    /// Parked without an invocation until a signal key holds a value no
    /// wake used or `until` passes, then resuming at `checkpoint` with
    /// the wake as its reason (upstream `sleeping` with signals).
    WaitingOnSignals {
        checkpoint: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        signals: Vec<Value>,
    },
    /// Settled, with its outcome.
    Terminal { outcome: TaskOutcome },
}

/// A signal kind (upstream `defineSignal`): JSON values delivered under
/// keys to a task, kept in its memos until a wake uses them. `T` is the
/// value's Rust type, for the callers; the wire is JSON.
pub struct SignalToken<T> {
    /// The signal's name, unique like a task kind; no `/`.
    pub name: &'static str,
    pub version: u32,
    #[doc(hidden)]
    pub _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Copy for SignalToken<T> {}

impl<T> Clone for SignalToken<T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Defines a signal kind (upstream `defineSignal`): values of `T` (any
/// JSON shape) delivered under keys.
pub const fn define_signal<T>(name: &'static str, version: u32) -> SignalToken<T> {
    SignalToken {
        name,
        version,
        _marker: std::marker::PhantomData,
    }
}

impl<T> SignalToken<T> {
    /// The key of one wait for this signal (upstream `token.key`).
    pub fn key(&self, key: impl Into<String>) -> SignalKey<T> {
        SignalKey {
            signal: self.name,
            key: key.into(),
            _marker: std::marker::PhantomData,
        }
    }

    /// The memo a key's state lives in.
    pub(crate) fn memo_name(&self, key: &str) -> String {
        format!("pi.signal.{}/{}", self.name, key)
    }
}

/// The key of one wait (upstream `SignalKey`).
pub struct SignalKey<T> {
    pub signal: &'static str,
    pub key: String,
    #[doc(hidden)]
    pub _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for SignalKey<T> {
    fn clone(&self) -> Self {
        SignalKey {
            signal: self.signal,
            key: self.key.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

/// A key's state: the value delivered under it, or expired because its
/// sleep's deadline came first (upstream `SignalState`).
#[derive(Debug, Clone, PartialEq)]
pub enum SignalState<T> {
    Value(T),
    Expired,
}

/// What a delivery did (upstream `SignalResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalResult {
    /// Durably accepted, not necessarily consumed.
    Delivered,
    /// The key already holds a value.
    Duplicate,
    /// The task ended, is abort-marked, or the key expired.
    Closed,
    /// No such task.
    NotFound,
}

/// Why a wait woke (upstream `WakeReason`): `expired`, or `signal`
/// naming the key that held a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum WakeReason {
    /// The deadline passed.
    Expired,
    /// A signal key held a value no wake used.
    Signal { signal: String, key: String },
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
        let names = self
            .phases
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<&'static str>>();
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
            let handler = definition
                .phases
                .iter()
                .find(|(name, _)| *name == phase)
                .map(|(_, handler)| handler.clone());
            let Some(handler) = handler else {
                return;
            };
            let wake = record_wake(&record);
            let context = TaskContext::new(
                Arc::new(input),
                Arc::new(checkpoint),
                wake,
                record.memos.clone(),
                runtime,
            );
            handler(context).await;
        })
    })
}

/// The wake a record's state holds, as the resumed phase reads it.
pub(crate) fn record_wake(record: &DurableRecordCore) -> Option<WakeReason> {
    record
        .state
        .get("wake")
        .and_then(|wake| serde_json::from_value(wake.clone()).ok())
}

/// The core a wake is read from (the noel-core record, as the erased
/// handlers hold it).
pub(crate) type DurableRecordCore = noel_core::DurableTaskRecord;

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
            let wake = record_wake(&record);
            let context = TaskContext::new(
                Arc::new(input),
                Arc::new(checkpoint),
                wake,
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
