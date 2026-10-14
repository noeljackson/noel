//! What a host serves: an object's implementation (its variant, state cells
//! and methods), as a factory returns it, and what a method call gets.
//!
//! This is the subset of upstream's `object.ts` and `cell.ts` noel needs:
//! factories are synchronous and objects are live at once; there are no
//! resources, listeners, tasks, derivations, registries or reloads.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::watch;

use crate::delta::{apply_owned, diff, DeltaError, Op};
use crate::error::ChordError;
use crate::validate;

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A method: arguments (absent for methods that take none) and the call's
/// context, to a result (ignored for void methods).
pub type MethodFn =
    Arc<dyn Fn(Option<Value>, CallContext) -> BoxFuture<Result<Value, ChordError>> + Send + Sync>;

/// Cancellation of one call: aborted when the caller cancels or its link
/// closes.
#[derive(Clone)]
pub struct AbortSignal {
    sender: Arc<watch::Sender<Option<ChordError>>>,
}

impl Default for AbortSignal {
    fn default() -> Self {
        AbortSignal::new()
    }
}

impl AbortSignal {
    pub fn new() -> Self {
        AbortSignal {
            sender: Arc::new(watch::channel(None).0),
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.sender.borrow().is_some()
    }

    pub fn reason(&self) -> Option<ChordError> {
        self.sender.borrow().clone()
    }

    /// Resolves with the reason once aborted.
    pub async fn aborted(&self) -> ChordError {
        let mut receiver = self.sender.subscribe();
        let reason = receiver
            .wait_for(Option::is_some)
            .await
            .expect("the sender lives as long as the signal");
        reason.clone().expect("aborted")
    }

    pub(crate) fn abort(&self, reason: ChordError) {
        self.sender.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }
}

/// What a method gets besides its arguments.
#[derive(Clone)]
pub struct CallContext {
    /// The principal the call acts as, stamped by the link it came over.
    pub principal: Value,
    pub signal: AbortSignal,
}

/// One change of a cell: its ops and the value after them.
#[derive(Clone, Debug)]
pub(crate) struct CellChange {
    pub ops: Arc<Vec<Op>>,
    pub value: Arc<Value>,
}

type Listener = Arc<dyn Fn(&CellChange) + Send + Sync>;

struct CellState {
    value: Arc<Value>,
    listeners: Vec<(u64, Listener)>,
    next: u64,
}

struct CellInner {
    schema: Value,
    state: Mutex<CellState>,
}

/// A state member: a value with a schema, whose changes are published as
/// delta ops to every subscription.
#[derive(Clone)]
pub struct Cell {
    inner: Arc<CellInner>,
}

impl Cell {
    /// A cell with its schema and initial value (which must fit it).
    pub fn new(schema: Value, initial: Value) -> Result<Cell, ChordError> {
        validate::assert_valid("state", "initial state", &schema, &initial)?;
        Ok(Cell {
            inner: Arc::new(CellInner {
                schema,
                state: Mutex::new(CellState {
                    value: Arc::new(initial),
                    listeners: Vec::new(),
                    next: 0,
                }),
            }),
        })
    }

    pub fn schema(&self) -> &Value {
        &self.inner.schema
    }

    pub fn value(&self) -> Arc<Value> {
        self.inner.state.lock().expect("cell").value.clone()
    }

    /// Applies `ops` and publishes them. The new value must fit the schema;
    /// otherwise nothing changes (`invalid/state`).
    pub fn apply(&self, ops: Vec<Op>) -> Result<(), ChordError> {
        self.change(|_| ops).map(|_| ())
    }

    /// Computes ops from the current value and applies them, atomically with
    /// respect to other changes. Returns the new value.
    pub fn change(&self, ops: impl FnOnce(&Value) -> Vec<Op>) -> Result<Arc<Value>, ChordError> {
        let mut state = self.inner.state.lock().expect("cell");
        let ops = ops(&state.value);
        if ops.is_empty() {
            return Ok(state.value.clone());
        }
        let next = apply_owned((*state.value).clone(), &ops)
            .map_err(|error: DeltaError| ChordError::invalid("state", error.to_string()))?;
        validate::assert_valid("state", "state", &self.inner.schema, &next)?;
        state.value = Arc::new(next);
        let change = CellChange {
            ops: Arc::new(ops),
            value: state.value.clone(),
        };
        for (_, listener) in &state.listeners {
            listener(&change);
        }
        Ok(state.value.clone())
    }

    /// Replaces the whole value: subscribers get one `r` op.
    pub fn replace(&self, value: Value) -> Result<(), ChordError> {
        self.apply(vec![Op::Replace(value)])
    }

    /// Moves to `value`, publishing the structural difference.
    pub fn update(&self, value: Value) -> Result<(), ChordError> {
        let ops = diff(&self.value(), &value);
        self.apply(ops)
    }

    /// Registers a listener and returns the value it starts from, atomically
    /// with respect to changes.
    pub(crate) fn listen(&self, listener: Listener) -> (u64, Arc<Value>) {
        let mut state = self.inner.state.lock().expect("cell");
        let id = state.next;
        state.next += 1;
        state.listeners.push((id, listener));
        (id, state.value.clone())
    }

    pub(crate) fn unlisten(&self, id: u64) {
        let mut state = self.inner.state.lock().expect("cell");
        state.listeners.retain(|(listener, _)| *listener != id);
    }
}

/// A member of an implementation.
#[derive(Clone)]
pub enum Member {
    State(Cell),
    Method(MethodFn),
}

/// What a factory returns: the variant the principal gets and its members,
/// in order (the order `shape.members` lists them).
pub struct Implementation {
    pub(crate) variant: Option<String>,
    pub(crate) members: Vec<(String, Member)>,
    pub(crate) on_dispose: Option<Box<dyn FnOnce() + Send>>,
}

impl Implementation {
    pub fn new(variant: impl Into<String>) -> Self {
        Implementation {
            variant: Some(variant.into()),
            members: Vec::new(),
            on_dispose: None,
        }
    }

    /// Exposes `cell` as the state member `name`.
    pub fn state(mut self, name: impl Into<String>, cell: &Cell) -> Self {
        self.members
            .push((name.into(), Member::State(cell.clone())));
        self
    }

    /// Exposes `method` as `name`.
    pub fn method<F, Fut>(mut self, name: impl Into<String>, method: F) -> Self
    where
        F: Fn(Option<Value>, CallContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ChordError>> + Send + 'static,
    {
        let method: MethodFn = Arc::new(move |args, ctx| Box::pin(method(args, ctx)));
        self.members.push((name.into(), Member::Method(method)));
        self
    }

    /// Runs `dispose` when the object ends (its last hold released, or
    /// forced).
    pub fn on_dispose(mut self, dispose: impl FnOnce() + Send + 'static) -> Self {
        self.on_dispose = Some(Box::new(dispose));
        self
    }

    pub(crate) fn member(&self, name: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, member)| member)
    }
}
