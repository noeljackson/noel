//! What a v2 provider serves: an implementation per object, made by a
//! factory from the hold's params and principal. It names its variant and
//! gives the variant's state members as [`Cell`]s and its methods as async
//! functions. Calls run with the caller's principal and an abort signal.
//!
//! noel serves cells and methods only: upstream's documents, derives,
//! resources, routines and reload have no counterpart (the non-parity
//! register's "Chord v2" section).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::watch;

use super::error::{ChordError, WireError};
use std::pin::Pin;
use std::sync::Mutex;

use crate::delta::{apply_owned, diff, Op};
use crate::validate;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Why a cell refused a value: it does not fit the cell's schema, or ops do
/// not apply to it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CellError(pub String);

/// A change of a cell: its ops and the value after them.
#[derive(Clone, Debug)]
pub(crate) struct CellChange {
    pub ops: Arc<Vec<Op>>,
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

/// A state member's value with its schema; each change is checked against
/// the schema and published as ops to the holds that follow it.
#[derive(Clone)]
pub struct Cell {
    inner: Arc<CellInner>,
}

impl Cell {
    pub fn new(schema: Value, initial: Value) -> Result<Cell, CellError> {
        validate::check(&schema, &initial)
            .map_err(|problem| CellError(format!("initial state: {problem}")))?;
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

    pub fn apply(&self, ops: Vec<Op>) -> Result<(), CellError> {
        self.change(|_| ops).map(|_| ())
    }

    /// Applies the ops `ops` gives for the current value, atomically.
    pub fn change(&self, ops: impl FnOnce(&Value) -> Vec<Op>) -> Result<Arc<Value>, CellError> {
        let mut state = self.inner.state.lock().expect("cell");
        let ops = ops(&state.value);
        if ops.is_empty() {
            return Ok(state.value.clone());
        }
        let next = apply_owned((*state.value).clone(), &ops)
            .map_err(|error| CellError(format!("state: {error}")))?;
        validate::check(&self.inner.schema, &next)
            .map_err(|problem| CellError(format!("state: {problem}")))?;
        state.value = Arc::new(next);
        let change = CellChange { ops: Arc::new(ops) };
        for (_, listener) in &state.listeners {
            listener(&change);
        }
        Ok(state.value.clone())
    }

    /// Replaces the whole value: followers get one `r` op.
    pub fn replace(&self, value: Value) -> Result<(), CellError> {
        self.apply(vec![Op::Replace(value)])
    }

    /// Moves to `value`, publishing the structural difference.
    pub fn update(&self, value: Value) -> Result<(), CellError> {
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

/// A method: its arguments (`null` when it takes none) and the call's
/// context, giving its result (`null` for none) or an error.
pub type MethodFn =
    Arc<dyn Fn(Value, CallContext) -> BoxFuture<Result<Value, WireError>> + Send + Sync>;

/// A call's cancellation: its caller aborted it, its hold was released or
/// its link closed.
#[derive(Clone)]
pub struct Signal {
    sender: Arc<watch::Sender<Option<ChordError>>>,
}

impl Default for Signal {
    fn default() -> Self {
        Signal {
            sender: Arc::new(watch::channel(None).0),
        }
    }
}

impl Signal {
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

/// What a call runs with.
#[derive(Clone)]
pub struct CallContext {
    /// Whom the hold acts for.
    pub principal: Value,
    pub signal: Signal,
}

/// What a factory returns: the variant and its members.
pub struct Implementation {
    pub(crate) variant: String,
    pub(crate) state: BTreeMap<String, Cell>,
    pub(crate) methods: BTreeMap<String, MethodFn>,
    pub(crate) on_dispose: Option<Box<dyn FnOnce() + Send>>,
}

impl Implementation {
    pub fn new(variant: impl Into<String>) -> Implementation {
        Implementation {
            variant: variant.into(),
            state: BTreeMap::new(),
            methods: BTreeMap::new(),
            on_dispose: None,
        }
    }

    /// Exposes `cell` as the state member `name`.
    pub fn state(mut self, name: impl Into<String>, cell: &Cell) -> Implementation {
        self.state.insert(name.into(), cell.clone());
        self
    }

    /// Exposes `method` as `name`.
    pub fn method<F, Fut>(mut self, name: impl Into<String>, method: F) -> Implementation
    where
        F: Fn(Value, CallContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, WireError>> + Send + 'static,
    {
        let method: MethodFn = Arc::new(move |args, context| Box::pin(method(args, context)));
        self.methods.insert(name.into(), method);
        self
    }

    /// Runs `dispose` when the object ends (its last hold released).
    pub fn on_dispose(mut self, dispose: impl FnOnce() + Send + 'static) -> Implementation {
        self.on_dispose = Some(Box::new(dispose));
        self
    }
}

/// Makes an object's implementation from its params and the principal of
/// the hold that made it.
pub type Factory = Arc<dyn Fn(&Value, &Value) -> Result<Implementation, WireError> + Send + Sync>;
