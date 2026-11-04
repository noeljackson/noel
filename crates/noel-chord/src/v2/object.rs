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
pub use crate::object::{BoxFuture, Cell};

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
