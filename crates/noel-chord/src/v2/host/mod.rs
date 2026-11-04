//! A Chord v2 host (upstream `src/remote/` and the parts of `src/host/`
//! links see): its links, both sides of each, and the remote references it
//! holds.
//!
//! - The provider side of each (link, origin): the holds the peer sent, by
//!   their ids; the objects they bind, made by the served type's factory;
//!   `bound` snapshots and `state` ops through the window (at most
//!   [`WINDOW`] unacknowledged messages of this host's own, ops pending
//!   meanwhile, replaced by a snapshot past [`MAX_PENDING_OPS`]); calls, at
//!   most [`CALLS`] at once; `failed` (waiting or broken), `loading` and
//!   `retry`.
//! - The consumer side of each link: this host's holds on it, the calls
//!   through them within the peer's call window, every `bound` and `state`
//!   applied to the reference's replica, then acknowledged.
//! - References ([`RemoteRef`]) held over the parent link, held again on the
//!   next parent link after a drop.
//!
//! One task owns everything: link bytes, cell changes, API requests and
//! method completions arrive as messages, and the ops of one drained batch
//! go out as one `state` message per (link, origin). Paths, routes and
//! relaying are in [`relay`]; revocation in `revoke`.

mod relay;
mod revoke;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use super::channel::{BinaryCodec, Channel, ChannelOptions, Received, Tick, CALLS};
use super::check::uuidv7;
use super::contract::Contract;
use super::error::{ChordError, Code, WireError};
use super::link::Link;
use super::message::{Address, BoundEntry, CauseKind, FailedMember, Message, Part, StateEntry};
use super::object::{BoxFuture, CallContext, Cell, Factory, Implementation, MethodFn, Signal};
use super::part::{open_part, origin_part, oversized, Limits, OpenError, PartLimits};
use crate::delta::{apply_owned, Decoder, Encoder, Op};
use crate::object::CellChange;
use crate::schema::canonical_json;
use relay::{Holder, PathKey, PathState, Placed, RelayedHold};
pub use relay::{Resolved, Route};

/// At most this many of this host's own `bound` and `state` messages
/// unacknowledged per (link, origin).
pub const WINDOW: usize = 2;

/// Pending ops of a member past this many are replaced by its value.
pub const MAX_PENDING_OPS: usize = 10_000;

/// How a link was made: `Parent` by [`Host::connect`] (holds arriving on
/// it may name whom they act for), `Child` by [`Host::accept`], `Route` by a
/// route's `resolve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Parent,
    Child,
    Route,
}

#[derive(Debug, Clone)]
pub struct HostOptions {
    /// This host's incarnation; a new uuidv7 when `None`.
    pub incarnation: Option<String>,
    /// This host's address: holds naming it are served here, others are
    /// routed or relayed to the parent.
    pub address: Option<Address>,
    /// Deflate parts this host originates above the threshold (never up a
    /// parent link).
    pub compress: bool,
    /// The heartbeat: a silent link pings after `ping_ms`, closes after
    /// `close_ms`; checked every `tick_ms`.
    pub ping_ms: u64,
    pub close_ms: u64,
    pub tick_ms: u64,
}

impl Default for HostOptions {
    fn default() -> Self {
        HostOptions {
            incarnation: None,
            address: None,
            compress: false,
            ping_ms: super::channel::PING_AFTER_MS,
            close_ms: super::channel::CLOSE_AFTER_MS,
            tick_ms: 1_000,
        }
    }
}

/// What a link is made with.
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// The peer, as this host stamps it: whom holds arriving on it act for.
    pub principal: Value,
    /// Skips the message check.
    pub trusted: bool,
    pub limits: PartLimits,
}

impl LinkOptions {
    pub fn new(principal: Value) -> LinkOptions {
        LinkOptions {
            principal,
            trusted: false,
            limits: PartLimits::default(),
        }
    }
}

/// What a host reports.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// The host runs a new link (before its handshake).
    Linked {
        link: u64,
        role: Role,
        principal: Value,
    },
    /// A link's peer said `hello`.
    Opened {
        link: u64,
        peer: String,
        role: Role,
        principal: Value,
    },
    Closed {
        link: u64,
    },
    /// A hold served here bound its object.
    Held {
        link: u64,
        hold: u64,
        type_name: String,
        params: Value,
        principal: Value,
    },
    /// A hold served here ended.
    Released {
        link: u64,
        hold: u64,
        type_name: String,
    },
    /// The link closed for it.
    ProtocolError {
        link: u64,
        error: String,
    },
}

/// Where a reference stands.
#[derive(Debug, Clone, PartialEq)]
pub enum RefStatus {
    /// Not bound: no link yet, a new attempt, or rebinding.
    Loading,
    Live {
        variant: String,
    },
    /// The provider could not bind it: `Waiting` until something changes
    /// there, `Broken` until [`RemoteRef::retry`], `Transient` until it
    /// retries by itself (`retry_in` ms).
    Failed {
        cause: CauseKind,
        error: WireError,
        retry_in: Option<u64>,
    },
    /// The provider ended it.
    Gone(WireError),
    Released,
}

/// What a state subscription delivers.
#[derive(Debug, Clone, PartialEq)]
pub enum StateUpdate {
    /// The member's value at a binding.
    Snapshot(Value),
    /// Decoded ops and the value after them.
    Ops { ops: Vec<Op>, value: Value },
    /// The member failed at the provider.
    Failed(WireError),
}

type Observer = Arc<dyn Fn(&HostEvent) + Send + Sync>;

/// What revokes a principal: the revocations that end its objects and
/// links (compared as canonical JSON). The same function on every host.
pub type RevokedBy = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;

enum Msg {
    Serve {
        contract: Arc<Contract>,
        factory: Factory,
    },
    Attach {
        link: Link,
        role: Role,
        options: LinkOptions,
    },
    Bytes {
        link: u64,
        bytes: Vec<u8>,
    },
    Ended {
        link: u64,
    },
    Changed {
        sub: u64,
        ops: Arc<Vec<Op>>,
    },
    Replied {
        token: u64,
        result: Result<Value, WireError>,
    },
    Get {
        id: u64,
        contract: Arc<Contract>,
        params: Value,
        status: watch::Sender<RefStatus>,
    },
    Call {
        reference: u64,
        token: u64,
        method: String,
        args: Value,
        reply: oneshot::Sender<Result<Value, WireError>>,
    },
    Cancel {
        token: u64,
    },
    Subscribe {
        reference: u64,
        member: String,
        sink: u64,
        sender: mpsc::UnboundedSender<StateUpdate>,
    },
    Unsubscribe {
        reference: u64,
        sink: u64,
    },
    Retry {
        reference: u64,
    },
    Release {
        reference: u64,
    },
    Route {
        kind: String,
        route: Arc<dyn Route>,
    },
    Resolved {
        key: PathKey,
        result: Result<Resolved, ChordError>,
    },
    RouteLink {
        address: Address,
        resolved: Resolved,
    },
    Observe(Observer),
    RevokedBy(RevokedBy),
    Revoke(Value),
    Tick,
    Close(oneshot::Sender<()>),
}

struct Shared {
    incarnation: String,
    next: AtomicU64,
}

/// A host. Cloning shares it. Create it inside a tokio runtime.
#[derive(Clone)]
pub struct Host {
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
}

impl Host {
    pub fn new(options: HostOptions) -> Host {
        let incarnation = options.incarnation.clone().unwrap_or_else(uuidv7);
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            incarnation: incarnation.clone(),
            next: AtomicU64::new(1),
        });
        let actor = Actor {
            tx: tx.clone(),
            options,
            incarnation,
            started: tokio::time::Instant::now(),
            served: HashMap::new(),
            objects: HashMap::new(),
            links: HashMap::new(),
            parent: None,
            paths: HashMap::new(),
            routes: HashMap::new(),
            relays: HashMap::new(),
            next_relay: 0,
            refs: HashMap::new(),
            subs: HashMap::new(),
            running: HashMap::new(),
            outgoing: HashMap::new(),
            dirty: Vec::new(),
            observers: Vec::new(),
            revoked_by: None,
            next: 1,
        };
        tokio::spawn(actor.run(rx));
        Host { tx, shared }
    }

    pub fn incarnation(&self) -> &str {
        &self.shared.incarnation
    }

    fn next(&self) -> u64 {
        self.shared.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Serves `contract`'s objects, made by `factory` from their params and
    /// the principal of the hold that made them.
    pub fn serve<F>(&self, contract: impl Into<Arc<Contract>>, factory: F)
    where
        F: Fn(&Value, &Value) -> Result<Implementation, WireError> + Send + Sync + 'static,
    {
        let _ = self.tx.send(Msg::Serve {
            contract: contract.into(),
            factory: Arc::new(factory),
        });
    }

    /// Runs `link` as a child link: holds on it act for `options.principal`.
    pub fn accept(&self, link: Link, options: LinkOptions) {
        let _ = self.tx.send(Msg::Attach {
            link,
            role: Role::Child,
            options,
        });
    }

    /// Runs `link` as this host's parent link: references are held over it,
    /// and over the next one after it drops.
    pub fn connect(&self, link: Link, options: LinkOptions) {
        let _ = self.tx.send(Msg::Attach {
            link,
            role: Role::Parent,
            options,
        });
    }

    /// Routes holds for addresses of `kind` over the links `route` resolves.
    pub fn route(&self, kind: impl Into<String>, route: impl Route) {
        let _ = self.tx.send(Msg::Route {
            kind: kind.into(),
            route: Arc::new(route),
        });
    }

    /// Runs `link` now as the link of `address` on its kind's route, as if
    /// the route had resolved it (a worker this host adopts); its route
    /// resolves the address again when it drops.
    pub fn route_link(&self, address: Address, link: Link, options: LinkOptions) {
        let _ = self.tx.send(Msg::RouteLink {
            address,
            resolved: Resolved { link, options },
        });
    }

    /// Holds the object of `contract` with `params`: over the route of its
    /// address's kind, or the parent link.
    pub fn get(&self, contract: Arc<Contract>, params: Value) -> RemoteRef {
        let id = self.next();
        let (status, receiver) = watch::channel(RefStatus::Loading);
        let _ = self.tx.send(Msg::Get {
            id,
            contract: contract.clone(),
            params,
            status,
        });
        RemoteRef {
            id,
            host: self.clone(),
            status: receiver,
            contract,
        }
    }

    /// Sets what revokes a principal (upstream `revokedBy`).
    pub fn revoked_by(&self, revoked_by: impl Fn(&Value) -> Vec<Value> + Send + Sync + 'static) {
        let _ = self.tx.send(Msg::RevokedBy(Arc::new(revoked_by)));
    }

    /// Revokes `revocation`: every object whose principal it revokes ends
    /// (`gone forbidden/revoked`), so do unbound holds acting for one; route
    /// links are sent `revoke`; child links of such principals close.
    /// Nothing is replayed to links made later.
    pub fn revoke(&self, revocation: Value) {
        let _ = self.tx.send(Msg::Revoke(revocation));
    }

    /// Calls `observer` with each event from now on.
    pub fn observe(&self, observer: impl Fn(&HostEvent) + Send + Sync + 'static) {
        let _ = self.tx.send(Msg::Observe(Arc::new(observer)));
    }

    /// Closes every link and stops the host.
    pub async fn close(&self) {
        let (done, closed) = oneshot::channel();
        if self.tx.send(Msg::Close(done)).is_ok() {
            let _ = closed.await;
        }
    }
}

/// A remote reference: one hold, held again after a drop.
pub struct RemoteRef {
    id: u64,
    host: Host,
    status: watch::Receiver<RefStatus>,
    contract: Arc<Contract>,
}

impl RemoteRef {
    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    pub fn status(&self) -> RefStatus {
        self.status.borrow().clone()
    }

    /// The status, to watch changes.
    pub fn statuses(&self) -> watch::Receiver<RefStatus> {
        self.status.clone()
    }

    /// The variant once live; the error once failed, gone or released.
    pub async fn ready(&self) -> Result<String, WireError> {
        let mut status = self.status.clone();
        let settled = status
            .wait_for(|status| !matches!(status, RefStatus::Loading))
            .await
            .map(|status| status.clone())
            .unwrap_or_else(|_| RefStatus::Gone(host_closed().into()));
        match settled {
            RefStatus::Live { variant } => Ok(variant),
            RefStatus::Failed { error, .. } | RefStatus::Gone(error) => Err(error),
            RefStatus::Released => Err(released().into()),
            RefStatus::Loading => unreachable!("waited for another status"),
        }
    }

    /// Calls `method` with `args` (ignored for a method without arguments).
    /// Waits while the reference is loading. Dropping the future cancels the
    /// call.
    pub async fn call(&self, method: &str, args: Value) -> Result<Value, WireError> {
        let token = self.host.next();
        let (reply, result) = oneshot::channel();
        let mut guard = CancelOnDrop {
            tx: self.host.tx.clone(),
            token,
            armed: true,
        };
        let _ = self.host.tx.send(Msg::Call {
            reference: self.id,
            token,
            method: method.to_owned(),
            args,
            reply,
        });
        let outcome = result.await.unwrap_or_else(|_| Err(host_closed().into()));
        guard.armed = false;
        outcome
    }

    /// Subscribes to the state member `member`: its value at each binding,
    /// then ops. Dropping the stream unsubscribes.
    pub fn subscribe(&self, member: &str) -> StateStream {
        let sink = self.host.next();
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = self.host.tx.send(Msg::Subscribe {
            reference: self.id,
            member: member.to_owned(),
            sink,
            sender,
        });
        StateStream {
            receiver,
            tx: self.host.tx.clone(),
            reference: self.id,
            sink,
        }
    }

    /// Asks the provider of a failed hold for an attempt now.
    pub fn retry(&self) {
        let _ = self.host.tx.send(Msg::Retry { reference: self.id });
    }

    /// Releases the hold.
    pub fn release(&self) {
        let _ = self.host.tx.send(Msg::Release { reference: self.id });
    }
}

impl Drop for RemoteRef {
    fn drop(&mut self) {
        self.release();
    }
}

struct CancelOnDrop {
    tx: mpsc::UnboundedSender<Msg>,
    token: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(Msg::Cancel { token: self.token });
        }
    }
}

/// A state subscription. Dropping it unsubscribes.
pub struct StateStream {
    receiver: mpsc::UnboundedReceiver<StateUpdate>,
    tx: mpsc::UnboundedSender<Msg>,
    reference: u64,
    sink: u64,
}

impl StateStream {
    /// The next update; `None` once the reference is released or gone.
    pub async fn next(&mut self) -> Option<StateUpdate> {
        self.receiver.recv().await
    }
}

impl Drop for StateStream {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Unsubscribe {
            reference: self.reference,
            sink: self.sink,
        });
    }
}

fn host_closed() -> ChordError {
    ChordError::with_reason(Code::Released, "closed", "the host closed")
}

fn released() -> ChordError {
    ChordError::new(Code::Released, "the reference was released")
}

fn link_closed() -> ChordError {
    ChordError::new(Code::Unavailable, "the link closed")
}

// ─── provider side ─────────────────────────────────────────────────────────

/// An object served here, shared by the holds of the same type, params and
/// principal.
struct Object {
    contract: Arc<Contract>,
    variant: String,
    state: BTreeMap<String, Cell>,
    methods: BTreeMap<String, MethodFn>,
    on_dispose: Option<Box<dyn FnOnce() + Send>>,
    holds: usize,
    /// Whom it was made for.
    principal: Value,
}

/// (type, params, principal), each as canonical JSON.
type ObjectKey = (String, String, String);

/// A member a bound hold follows.
struct Followed {
    index: u64,
    cell: Cell,
    /// The cell's listener id.
    listener: u64,
    /// This subscription's id in the host (stale changes are dropped).
    sub: u64,
    encoder: Encoder,
    /// The current batch's ops.
    changes: Vec<Op>,
    pending: Pending,
}

enum Pending {
    Ops(Vec<Op>),
    /// Past [`MAX_PENDING_OPS`]: the member's value goes instead.
    Overflowed,
}

impl Pending {
    fn push(&mut self, ops: Vec<Op>) {
        if let Pending::Ops(pending) = self {
            if ops.first().is_some_and(|op| matches!(op, Op::Replace(_))) {
                pending.clear();
            }
            pending.extend(ops);
            if pending.len() > MAX_PENDING_OPS {
                *self = Pending::Overflowed;
            }
        }
    }
}

/// What a `hold` asked for.
#[derive(Clone)]
struct Request {
    type_name: String,
    version: u64,
    fingerprint: String,
    params: Part,
    address: Address,
}

enum HoldState {
    /// Live; its `bound` waits for a slot.
    Binding,
    Bound {
        methods: Vec<String>,
        followed: Vec<Followed>,
    },
    /// Holding nothing: `Waiting` until the type is served, `Broken` until a
    /// `retry`.
    Refused { cause: CauseKind },
}

struct ServedHold {
    request: Request,
    principal: Value,
    limits: Limits,
    object: Option<ObjectKey>,
    state: HoldState,
}

struct RunningCall {
    hold: u64,
    token: u64,
    signal: Signal,
}

#[derive(Default)]
struct Provider {
    holds: HashMap<u64, ServedHold>,
    calls: HashMap<u64, RunningCall>,
    /// Holds relayed upstream, and their calls in flight (call to hold).
    relayed: HashMap<u64, RelayedHold>,
    relayed_calls: HashMap<u64, u64>,
    /// The sender of each `bound` and `state` message in flight, in order:
    /// this host (`None`) or the upstream link it was relayed from.
    sent: VecDeque<Option<u64>>,
    /// This origin's number on upstream links, and the links it relays to.
    relay: Option<u64>,
    relay_links: Vec<u64>,
    in_flight: usize,
    /// Holds waiting for a slot to send their first binding, in order.
    binding: Vec<u64>,
    /// Holds whose ops wait in their pending queues, in order.
    waiting: Vec<u64>,
    binding_turn: bool,
    /// Holds the current batch touched, in order.
    touched: Vec<u64>,
}

impl Provider {
    fn free(&self) -> bool {
        self.binding.is_empty() && self.waiting.is_empty() && self.in_flight < WINDOW
    }
}

// ─── consumer side ─────────────────────────────────────────────────────────

/// A call through a consumer side.
struct OutgoingCall {
    reference: u64,
    link: u64,
    hold: u64,
    method: u64,
    args: Option<Part>,
    reply: oneshot::Sender<Result<Value, WireError>>,
    /// Its id once sent.
    id: Option<u64>,
}

#[derive(Default)]
struct Consumer {
    next_hold: u64,
    next_call: u64,
    /// Hold id to reference.
    holds: HashMap<u64, u64>,
    /// Call id to token, in flight.
    calls: HashMap<u64, u64>,
    /// The peer's call window: 0 before its `hello`.
    limit: u64,
    in_flight: u64,
    waiting: VecDeque<u64>,
}

type QueuedCall = (
    u64,
    String,
    Value,
    oneshot::Sender<Result<Value, WireError>>,
);

struct MemberReplica {
    value: Option<Value>,
    decoder: Decoder,
}

struct RefEntry {
    contract: Arc<Contract>,
    params: Value,
    status: watch::Sender<RefStatus>,
    /// Where it is held.
    path: Option<PathKey>,
    /// (link, hold) while held.
    held: Option<(u64, u64)>,
    variant: Option<String>,
    methods: Vec<String>,
    members: BTreeMap<String, MemberReplica>,
    /// Member to index, for the binding's state entries.
    state_names: Vec<String>,
    subscribers: Vec<(u64, String, mpsc::UnboundedSender<StateUpdate>)>,
    /// Calls made while loading, sent once live: (token, method, args,
    /// reply).
    queued: Vec<QueuedCall>,
}

impl RefEntry {
    fn set_status(&self, status: RefStatus) {
        self.status.send_replace(status);
    }

    fn notify(&mut self, member: &str, update: StateUpdate) {
        self.subscribers.retain(|(_, subscribed, sender)| {
            subscribed != member || sender.send(update.clone()).is_ok()
        });
    }
}

// ─── links ─────────────────────────────────────────────────────────────────

struct HostLink {
    link: Link,
    channel: Channel,
    role: Role,
    principal: Value,
    trusted: bool,
    limits: Limits,
    providers: HashMap<u64, Provider>,
    consumer: Consumer,
}

struct Actor {
    tx: mpsc::UnboundedSender<Msg>,
    options: HostOptions,
    incarnation: String,
    started: tokio::time::Instant,
    served: HashMap<String, (Arc<Contract>, Factory)>,
    objects: HashMap<ObjectKey, Object>,
    links: HashMap<u64, HostLink>,
    parent: Option<u64>,
    paths: HashMap<PathKey, PathState>,
    routes: HashMap<String, Arc<dyn Route>>,
    /// This host's relay origins: to (downstream link, origin).
    relays: HashMap<u64, (u64, u64)>,
    next_relay: u64,
    refs: HashMap<u64, RefEntry>,
    /// Subscription id to (link, origin, hold).
    subs: HashMap<u64, (u64, u64, u64)>,
    /// Call token to (link, origin, call id) for calls served here.
    running: HashMap<u64, (u64, u64, u64)>,
    /// Call token to the call, for calls this host made.
    outgoing: HashMap<u64, OutgoingCall>,
    /// (link, origin) sides the current batch touched.
    dirty: Vec<(u64, u64)>,
    observers: Vec<Observer>,
    revoked_by: Option<RevokedBy>,
    next: u64,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(
            self.options.tick_ms.max(1),
        ));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                _ = ticks.tick() => Some(Msg::Tick),
            };
            let Some(msg) = msg else { return };
            if let Msg::Close(done) = msg {
                self.shutdown();
                let _ = done.send(());
                return;
            }
            self.handle(msg);
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Close(done) = msg {
                    self.flush();
                    self.shutdown();
                    let _ = done.send(());
                    return;
                }
                self.handle(msg);
            }
            self.flush();
            self.pump();
        }
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn next_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn emit(&self, event: HostEvent) {
        for observer in &self.observers {
            observer(&event);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Serve { contract, factory } => {
                let name = contract.declaration.name.clone();
                self.served.insert(name.clone(), (contract, factory));
                self.wake(&name);
            }
            Msg::Attach {
                link,
                role,
                options,
            } => self.attach(link, role, options),
            Msg::Bytes { link, bytes } => self.receive(link, &bytes),
            Msg::Ended { link } => self.link_closed(link),
            Msg::Changed { sub, ops } => self.changed(sub, ops),
            Msg::Replied { token, result } => self.replied(token, result),
            Msg::Get {
                id,
                contract,
                params,
                status,
            } => {
                self.refs.insert(
                    id,
                    RefEntry {
                        contract,
                        params,
                        status,
                        path: None,
                        held: None,
                        variant: None,
                        methods: Vec::new(),
                        members: BTreeMap::new(),
                        state_names: Vec::new(),
                        subscribers: Vec::new(),
                        queued: Vec::new(),
                    },
                );
                let entry = &self.refs[&id];
                let address = entry.contract.address(&entry.params);
                match self.place_reference(&address) {
                    Ok(key) => {
                        self.refs.get_mut(&id).expect("the reference").path = Some(key.clone());
                        self.path_attach(&key, Holder::Ref(id));
                    }
                    Err(error) => {
                        let entry = self.refs.remove(&id).expect("the reference");
                        entry.set_status(RefStatus::Gone(error.into()));
                    }
                }
            }
            Msg::Route { kind, route } => {
                self.routes.insert(kind, route);
            }
            Msg::Resolved { key, result } => self.resolved(key, result),
            Msg::RouteLink { address, resolved } => self.route_link(address, resolved),
            Msg::Call {
                reference,
                token,
                method,
                args,
                reply,
            } => self.call(reference, token, &method, args, reply),
            Msg::Cancel { token } => self.cancel(token),
            Msg::Subscribe {
                reference,
                member,
                sink,
                sender,
            } => {
                let Some(entry) = self.refs.get_mut(&reference) else {
                    return;
                };
                if let Some(value) = entry.members.get(&member).and_then(|m| m.value.clone()) {
                    let _ = sender.send(StateUpdate::Snapshot(value));
                }
                entry.subscribers.push((sink, member, sender));
            }
            Msg::Unsubscribe { reference, sink } => {
                if let Some(entry) = self.refs.get_mut(&reference) {
                    entry.subscribers.retain(|(id, _, _)| *id != sink);
                }
            }
            Msg::Retry { reference } => {
                let Some(entry) = self.refs.get(&reference) else {
                    return;
                };
                let failed = matches!(*entry.status.borrow(), RefStatus::Failed { .. });
                if let (Some((link, hold)), true) = (entry.held, failed) {
                    self.send(
                        link,
                        &Message::Retry {
                            origin: None,
                            hold,
                            hops: super::check::RETRY_HOPS,
                        },
                    );
                }
            }
            Msg::Release { reference } => self.release(reference),
            Msg::Observe(observer) => self.observers.push(observer),
            Msg::RevokedBy(revoked_by) => self.revoked_by = Some(revoked_by),
            Msg::Revoke(revocation) => self.revoke(&revocation),
            Msg::Tick => {
                let now = self.now();
                let (ping, close) = (self.options.ping_ms, self.options.close_ms);
                let mut closing = Vec::new();
                for (id, link) in &mut self.links {
                    if link.channel.tick(now, ping, close) == Tick::Close {
                        closing.push(*id);
                    }
                }
                for id in closing {
                    self.close_link(id);
                }
            }
            Msg::Close(_) => unreachable!("handled by run"),
        }
    }

    /// Sends what the channels have to send.
    fn pump(&mut self) {
        for link in self.links.values_mut() {
            for bytes in link.channel.outgoing() {
                link.link.send(bytes);
            }
        }
    }

    fn send(&mut self, link: u64, message: &Message) {
        if let Some(link) = self.links.get_mut(&link) {
            link.channel.send(message);
        }
    }

    // ─── links ─────────────────────────────────────────────────────────────

    fn attach(&mut self, link: Link, role: Role, options: LinkOptions) {
        let id = link.id();
        if self.links.contains_key(&id) {
            return;
        }
        let Some(mut inbound) = link.take_inbound() else {
            return;
        };
        let channel = Channel::new(
            BinaryCodec,
            ChannelOptions {
                incarnation: self.incarnation.clone(),
                calls: CALLS,
                trusted: options.trusted,
            },
            self.now(),
        );
        self.links.insert(
            id,
            HostLink {
                link: link.clone(),
                channel,
                role,
                principal: options.principal,
                trusted: options.trusted,
                limits: Limits::of_link(options.trusted, options.limits),
                providers: HashMap::new(),
                consumer: Consumer::default(),
            },
        );
        self.emit(HostEvent::Linked {
            link: id,
            role,
            principal: self.links[&id].principal.clone(),
        });
        if role == Role::Parent {
            if let Some(previous) = self.parent.replace(id) {
                self.close_link(previous);
            }
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    bytes = inbound.recv() => match bytes {
                        Some(bytes) => {
                            if tx.send(Msg::Bytes { link: id, bytes }).is_err() {
                                return;
                            }
                        }
                        None => break,
                    },
                    _ = link.closed() => break,
                }
            }
            let _ = tx.send(Msg::Ended { link: id });
        });
        if role == Role::Parent {
            self.path_connected(&PathKey::Parent, id);
        }
    }

    fn receive(&mut self, id: u64, bytes: &[u8]) {
        let now = self.now();
        let Some(link) = self.links.get_mut(&id) else {
            return;
        };
        match link.channel.receive(bytes, now) {
            Ok(Received::Nothing) => {}
            Ok(Received::Opened) => {
                let peer = link.channel.peer().to_owned();
                link.consumer.limit = link.channel.peer_calls();
                let (role, principal) = (link.role, link.principal.clone());
                self.emit(HostEvent::Opened {
                    link: id,
                    peer,
                    role,
                    principal,
                });
                self.drain_calls(id);
            }
            Ok(Received::Message(message)) => {
                if let Err(error) = self.message(id, *message) {
                    self.protocol_error(id, error);
                }
            }
            Err(error) => self.protocol_error(id, error.0),
        }
    }

    fn protocol_error(&mut self, link: u64, error: String) {
        self.emit(HostEvent::ProtocolError { link, error });
        self.close_link(link);
    }

    fn close_link(&mut self, id: u64) {
        if let Some(link) = self.links.get(&id) {
            link.link.close();
        }
        self.link_closed(id);
    }

    /// A link closed, from either end: its provider sides release the peer's
    /// holds and abort its calls; its references go loading until the next
    /// parent link, and their calls fail.
    fn link_closed(&mut self, id: u64) {
        let Some(mut link) = self.links.remove(&id) else {
            return;
        };
        link.channel.closed();
        link.link.close();
        if self.parent == Some(id) {
            self.parent = None;
        }
        for (origin, mut provider) in link.providers.drain() {
            for call in provider.calls.values() {
                call.signal.abort(link_closed());
                self.running.remove(&call.token);
            }
            for (hold_id, hold) in std::mem::take(&mut provider.holds) {
                self.drop_hold(id, origin, hold_id, hold);
            }
            self.relay_closed(id, origin, &mut provider);
        }
        // Upstream of relayed holds: nothing more goes to it.
        for link in self.links.values_mut() {
            for provider in link.providers.values_mut() {
                provider.relay_links.retain(|up| *up != id);
            }
        }
        let tokens: Vec<u64> = self
            .outgoing
            .iter()
            .filter(|(_, call)| call.link == id)
            .map(|(token, _)| *token)
            .collect();
        for token in tokens {
            if let Some(call) = self.outgoing.remove(&token) {
                let _ = call.reply.send(Err(link_closed().into()));
            }
        }
        for reference in link.consumer.holds.values() {
            if let Some(entry) = self.refs.get_mut(reference) {
                entry.held = None;
                entry.variant = None;
                entry.set_status(RefStatus::Loading);
            }
        }
        self.path_disconnected(id);
        self.emit(HostEvent::Closed { link: id });
    }

    fn shutdown(&mut self) {
        let links: Vec<u64> = self.links.keys().copied().collect();
        self.pump();
        for id in links {
            self.close_link(id);
        }
        for (_, entry) in self.refs.drain() {
            entry.set_status(RefStatus::Gone(host_closed().into()));
        }
        for (_, call) in self.outgoing.drain() {
            let _ = call.reply.send(Err(host_closed().into()));
        }
    }

    fn message(&mut self, link: u64, message: Message) -> Result<(), String> {
        let role = self.links[&link].role;
        if role == Role::Child && message.origin().is_some() {
            return Err(format!("{}: an origin on a child link", message.kind()));
        }
        if let Message::Call { origin, call, .. } = &message {
            let in_use = self
                .provider(link, origin.unwrap_or(0))
                .is_some_and(|p| p.calls.contains_key(call) || p.relayed_calls.contains_key(call));
            if in_use {
                return Err(format!("call {call}: the id is in use"));
            }
        }
        if matches!(
            message,
            Message::Release { .. }
                | Message::Call { .. }
                | Message::Abort { .. }
                | Message::Retry { .. }
        ) && self.relayed_message(link, &message)
        {
            return Ok(());
        }
        match message {
            Message::Hold { .. } => self.hold(link, message),
            Message::Release { origin, hold } => {
                let origin = origin.unwrap_or(0);
                if let Some(served) = self
                    .provider(link, origin)
                    .and_then(|provider| provider.holds.remove(&hold))
                {
                    self.forget_calls(link, origin, hold, released());
                    self.drop_hold(link, origin, hold, served);
                }
                Ok(())
            }
            Message::Call {
                origin,
                call,
                hold,
                method,
                args,
                ..
            } => self.served_call(link, origin.unwrap_or(0), call, hold, method, args),
            Message::Abort { origin, call } => {
                if let Some(running) = self
                    .provider(link, origin.unwrap_or(0))
                    .and_then(|provider| provider.calls.remove(&call))
                {
                    self.running.remove(&running.token);
                    running.signal.abort(ChordError::new(
                        Code::Aborted,
                        format!("call {call}: aborted by its caller"),
                    ));
                }
                Ok(())
            }
            Message::Retry { origin, hold, .. } => {
                self.retry_hold(link, origin.unwrap_or(0), hold);
                Ok(())
            }
            Message::Ack { origin } => {
                let origin = origin.unwrap_or(0);
                let Some(provider) = self.provider(link, origin) else {
                    return Ok(());
                };
                match provider.sent.pop_front() {
                    // An ack crossing nothing in flight is ignored.
                    None => {}
                    Some(None) => {
                        provider.in_flight -= 1;
                        self.slot_freed(link, origin);
                    }
                    Some(Some(up)) => {
                        let relay = provider.relay;
                        self.send(up, &Message::Ack { origin: relay });
                    }
                }
                Ok(())
            }
            Message::Closed { origin } => {
                if let Some(mut provider) = self
                    .links
                    .get_mut(&link)
                    .and_then(|link| link.providers.remove(&origin))
                {
                    for call in provider.calls.values() {
                        call.signal.abort(link_closed());
                        self.running.remove(&call.token);
                    }
                    for (hold_id, hold) in std::mem::take(&mut provider.holds) {
                        self.drop_hold(link, origin, hold_id, hold);
                    }
                    self.relay_closed(link, origin, &mut provider);
                }
                Ok(())
            }
            Message::Bound {
                origin: None,
                holds,
            } => self.bound(link, holds),
            Message::State {
                origin: None,
                holds,
            } => self.state(link, holds),
            Message::Failed {
                origin: None,
                hold,
                cause,
                error,
                retry_in,
            } => {
                let error = WireError::from_json(&error).map_err(|e| e.to_string())?;
                self.consumer_failed(link, hold, cause, error, retry_in);
                Ok(())
            }
            Message::Loading { origin: None, hold } => {
                if let Some(reference) = self.consumer_ref(link, hold) {
                    self.fail_waiting_calls(
                        reference,
                        ChordError::new(Code::Unavailable, format!("hold {hold}: rebinding")),
                    );
                    if let Some(entry) = self.refs.get_mut(&reference) {
                        entry.variant = None;
                        entry.set_status(RefStatus::Loading);
                    }
                }
                Ok(())
            }
            Message::Result {
                origin: None,
                call,
                value,
                ..
            } => self.reply(link, call, Ok(value)),
            Message::Error {
                origin: None,
                call,
                error,
            } => {
                let error = WireError::from_json(&error).map_err(|e| e.to_string())?;
                self.reply(link, call, Err(error))
            }
            Message::Gone {
                origin: None,
                hold,
                error,
            } => {
                let error = WireError::from_json(&error).map_err(|e| e.to_string())?;
                if let Some(reference) = self.consumer_ref(link, hold) {
                    self.forget_reference(reference, error.clone());
                    if let Some(entry) = self.refs.get_mut(&reference) {
                        entry.set_status(RefStatus::Gone(error));
                        // Its subscriptions end.
                        entry.subscribers.clear();
                        if let Some(path) = entry.path.take() {
                            self.path_detach(&path, Holder::Ref(reference));
                        }
                    }
                }
                Ok(())
            }
            // With an origin: for a hold this host relays.
            Message::Bound { .. }
            | Message::State { .. }
            | Message::Failed { .. }
            | Message::Loading { .. }
            | Message::Result { .. }
            | Message::Error { .. }
            | Message::Gone { .. } => {
                self.forward(link, message);
                Ok(())
            }
            // Only the parent revokes.
            Message::Revoke { revocation } => {
                if role == Role::Parent {
                    self.revoke(&revocation);
                }
                Ok(())
            }
            Message::Hello { .. } | Message::Ping | Message::Pong => Ok(()),
        }
    }

    fn provider(&mut self, link: u64, origin: u64) -> Option<&mut Provider> {
        self.links.get_mut(&link)?.providers.get_mut(&origin)
    }

    fn send_origin(&mut self, link: u64, origin: u64, message: Message) {
        let message = with_origin(message, origin);
        self.send(link, &message);
    }

    // ─── provider: holds ───────────────────────────────────────────────────

    fn hold(&mut self, link: u64, message: Message) -> Result<(), String> {
        let Message::Hold {
            origin,
            hold,
            type_name,
            version,
            fingerprint,
            params,
            address,
            principal,
            incarnation,
            chain,
            limit,
        } = message
        else {
            unreachable!("a hold");
        };
        let origin = origin.unwrap_or(0);
        let host_link = &self.links[&link];
        let provider = host_link.providers.get(&origin);
        if provider.is_some_and(|p| p.holds.contains_key(&hold) || p.relayed.contains_key(&hold)) {
            return Err(format!("hold {hold}: the id is in use"));
        }
        if matches!(params, Part::Deflated(_)) {
            return Err(format!("hold {hold}: compressed params"));
        }
        let parent = host_link.role == Role::Parent;
        let peer = host_link.channel.peer().to_owned();
        let (principal, incarnation) = match (parent, principal) {
            (true, Some(principal)) => (principal, incarnation.unwrap_or(peer)),
            _ => (host_link.principal.clone(), peer),
        };
        let chain = chain.filter(|_| host_link.trusted);
        let mut limits = host_link.limits;
        if let (true, Some(limit)) = (parent, limit) {
            limits.decoded = limits.decoded.min(limit);
        }
        let wire_limit = host_link.limits.part;
        let request = Request {
            type_name,
            version,
            fingerprint,
            params,
            address,
        };
        // Placed by its address, its params within the link's part limit
        // first; one placed elsewhere is relayed.
        let placed = match oversized(&request.params, wire_limit) {
            Some(error) => Err(error),
            None => self.place_hold(link, &request.type_name, &request.address, &principal),
        };
        let refusal = match placed {
            Ok(Placed::Path(path)) => {
                let relayed = RelayedHold {
                    request,
                    principal,
                    incarnation,
                    chain,
                    limit: (limits.decoded != u64::MAX).then_some(limits.decoded),
                    path,
                    upstream: None,
                };
                self.links
                    .get_mut(&link)
                    .expect("the link")
                    .providers
                    .entry(origin)
                    .or_default();
                self.relay_hold(link, origin, hold, relayed);
                return Ok(());
            }
            Ok(Placed::Here) => None,
            Err(error) => Some(error),
        };
        let served = ServedHold {
            request,
            principal,
            limits,
            object: None,
            state: HoldState::Binding,
        };
        self.links
            .get_mut(&link)
            .expect("the link")
            .providers
            .entry(origin)
            .or_default()
            .holds
            .insert(hold, served);
        if let Some(error) = refusal {
            self.refuse(link, origin, hold, CauseKind::Broken, error.into());
            return Ok(());
        }
        self.start_hold(link, origin, hold)
    }

    /// Takes the hold's object, or refuses it.
    fn start_hold(&mut self, link: u64, origin: u64, hold: u64) -> Result<(), String> {
        let wire_limit = self.links[&link].limits.part;
        let Some(served) = self.provider(link, origin).and_then(|p| p.holds.get(&hold)) else {
            return Ok(());
        };
        let request = served.request.clone();
        let principal = served.principal.clone();
        let limits = served.limits;
        let refuse = |actor: &mut Actor, cause: CauseKind, error: WireError| {
            actor.refuse(link, origin, hold, cause, error);
            Ok(())
        };
        if let Some(error) = oversized(&request.params, wire_limit) {
            return refuse(self, CauseKind::Broken, error.into());
        }
        let Some((contract, factory)) = self.served.get(&request.type_name).cloned() else {
            let error = ChordError::new(
                Code::Unavailable,
                format!("{} is not served here", request.type_name),
            );
            return refuse(self, CauseKind::Waiting, error.into());
        };
        if contract.declaration.version != request.version
            || contract.fingerprint != request.fingerprint
        {
            let error = ChordError::new(
                Code::ContractMismatch,
                format!(
                    "{} v{} is not served here",
                    contract.declaration.name, request.version
                ),
            );
            return refuse(self, CauseKind::Waiting, error.into());
        }
        let params: Value = match open_part(&request.params, limits) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(params) => params,
                Err(error) => return Err(format!("hold {hold}: malformed params ({error})")),
            },
            Err(OpenError::TooLarge(error)) => {
                return refuse(self, CauseKind::Broken, error.into());
            }
            Err(OpenError::Corrupt(error)) => return Err(error),
        };
        if let Err(error) = contract.check_params(&params) {
            return refuse(
                self,
                CauseKind::Broken,
                ChordError::invalid("params", error).into(),
            );
        }
        // The provider's own check: the params put the object on this host.
        let address = contract.address(&params);
        if !self.is_here(&address) {
            let error = ChordError::invalid(
                "host_address",
                format!(
                    "{}: placed on {}, this host is {}",
                    request.type_name,
                    canonical_json(&json!({"kind": address.kind, "params": address.params})),
                    self.options
                        .address
                        .as_ref()
                        .map_or("unaddressed".to_owned(), |own| {
                            canonical_json(&json!({"kind": own.kind, "params": own.params}))
                        }),
                ),
            );
            return refuse(self, CauseKind::Waiting, error.into());
        }
        let key: ObjectKey = (
            request.type_name.clone(),
            canonical_json(&params),
            canonical_json(&principal),
        );
        if !self.objects.contains_key(&key) {
            let implementation = match factory(&params, &principal) {
                Ok(implementation) => implementation,
                Err(error) => return refuse(self, CauseKind::Broken, error),
            };
            if !contract
                .declaration
                .variants
                .contains_key(&implementation.variant)
            {
                let error = ChordError::new(
                    Code::Unavailable,
                    format!(
                        "{}: no variant {:?}",
                        request.type_name, implementation.variant
                    ),
                );
                return refuse(self, CauseKind::Broken, error.into());
            }
            self.objects.insert(
                key.clone(),
                Object {
                    contract,
                    variant: implementation.variant,
                    state: implementation.state,
                    methods: implementation.methods,
                    on_dispose: implementation.on_dispose,
                    holds: 0,
                    principal: principal.clone(),
                },
            );
        }
        self.objects.get_mut(&key).expect("the object").holds += 1;
        self.emit(HostEvent::Held {
            link,
            hold,
            type_name: request.type_name.clone(),
            params: params.clone(),
            principal: principal.clone(),
        });
        let provider = self.provider(link, origin).expect("the provider");
        let served = provider.holds.get_mut(&hold).expect("the hold");
        served.object = Some(key);
        served.state = HoldState::Binding;
        if provider.free() {
            let entry = self.bound_entry(link, origin, hold);
            self.send_own(
                link,
                origin,
                Message::Bound {
                    origin: None,
                    holds: vec![entry],
                },
            );
        } else {
            provider.binding.push(hold);
        }
        Ok(())
    }

    fn refuse(&mut self, link: u64, origin: u64, hold: u64, cause: CauseKind, error: WireError) {
        if let Some(served) = self
            .provider(link, origin)
            .and_then(|p| p.holds.get_mut(&hold))
        {
            served.state = HoldState::Refused { cause };
        }
        self.send_origin(
            link,
            origin,
            Message::Failed {
                origin: None,
                hold,
                cause,
                error: error.to_json(),
                retry_in: None,
            },
        );
    }

    /// A newly served type wakes the holds waiting for it.
    fn wake(&mut self, type_name: &str) {
        let mut waking = Vec::new();
        for (link_id, link) in &self.links {
            for (origin, provider) in &link.providers {
                for (hold_id, hold) in &provider.holds {
                    if hold.request.type_name == type_name
                        && matches!(
                            hold.state,
                            HoldState::Refused {
                                cause: CauseKind::Waiting
                            }
                        )
                    {
                        waking.push((*link_id, *origin, *hold_id));
                    }
                }
            }
        }
        for (link, origin, hold) in waking {
            self.restart(link, origin, hold);
        }
    }

    /// Starts a refused hold again, announced with `loading`.
    fn restart(&mut self, link: u64, origin: u64, hold: u64) {
        self.send_origin(link, origin, Message::Loading { origin: None, hold });
        if let Err(error) = self.start_hold(link, origin, hold) {
            self.protocol_error(link, error);
        }
    }

    fn retry_hold(&mut self, link: u64, origin: u64, hold: u64) {
        let refused = self
            .provider(link, origin)
            .and_then(|p| p.holds.get(&hold))
            .is_some_and(|hold| matches!(hold.state, HoldState::Refused { .. }));
        // A live object here never fails, so only refused holds retry.
        if refused {
            self.restart(link, origin, hold);
        }
    }

    /// The binding: every member's value and its subscription, taken in one
    /// step, so each member's next ops continue from it.
    fn bound_entry(&mut self, link: u64, origin: u64, hold: u64) -> BoundEntry {
        let compress = self.options.compress && self.links[&link].role != Role::Parent;
        let key = self.links[&link].providers[&origin].holds[&hold]
            .object
            .clone()
            .expect("a live hold");
        let object = &self.objects[&key];
        let contract = object.contract.clone();
        let variant = object.variant.clone();
        let (state_names, method_names) = contract
            .variant_names(&variant)
            .expect("the object's variant");
        let variant_index = contract
            .variant_order()
            .iter()
            .position(|name| *name == variant)
            .expect("the variant") as u64;
        let cells: Vec<(u64, Option<Cell>)> = state_names
            .iter()
            .enumerate()
            .map(|(index, name)| (index as u64, object.state.get(*name).cloned()))
            .collect();
        let methods = method_names.iter().map(|name| (*name).to_owned()).collect();
        let mut members = Vec::new();
        let mut failed = Vec::new();
        let mut followed = Vec::new();
        for (index, cell) in cells {
            let Some(cell) = cell else {
                let error: WireError =
                    ChordError::new(Code::Unavailable, "the member has no value").into();
                failed.push(FailedMember {
                    member: index,
                    error: error.to_json(),
                });
                continue;
            };
            let sub = self.next_id();
            let tx = self.tx.clone();
            let (listener, value) = cell.listen(Arc::new(move |change: &CellChange| {
                let _ = tx.send(Msg::Changed {
                    sub,
                    ops: change.ops.clone(),
                });
            }));
            self.subs.insert(sub, (link, origin, hold));
            members.push((index, origin_part(value.to_string(), compress)));
            followed.push(Followed {
                index,
                cell,
                listener,
                sub,
                encoder: Encoder::new(),
                changes: Vec::new(),
                pending: Pending::Ops(Vec::new()),
            });
        }
        let served = self
            .provider(link, origin)
            .and_then(|p| p.holds.get_mut(&hold))
            .expect("the hold");
        served.state = HoldState::Bound { methods, followed };
        BoundEntry {
            hold,
            variant: variant_index,
            members,
            failed,
        }
    }

    /// Sends one of this host's own `bound` or `state` messages, taking a
    /// slot of the window.
    fn send_own(&mut self, link: u64, origin: u64, message: Message) {
        if let Some(provider) = self.provider(link, origin) {
            provider.in_flight += 1;
            provider.sent.push_back(None);
        }
        self.send_origin(link, origin, message);
    }

    /// A slot may be free: the waiting state goes out as one message and the
    /// waiting bindings as one `bound`, taking turns when both wait.
    fn slot_freed(&mut self, link: u64, origin: u64) {
        loop {
            let Some(provider) = self.provider(link, origin) else {
                return;
            };
            if provider.in_flight >= WINDOW {
                return;
            }
            if !provider.binding.is_empty()
                && (provider.waiting.is_empty() || provider.binding_turn)
            {
                let holds = std::mem::take(&mut provider.binding);
                provider.binding_turn = false;
                let entries = holds
                    .into_iter()
                    .map(|hold| self.bound_entry(link, origin, hold))
                    .collect();
                self.send_own(
                    link,
                    origin,
                    Message::Bound {
                        origin: None,
                        holds: entries,
                    },
                );
            } else if !provider.waiting.is_empty() {
                let holds = std::mem::take(&mut provider.waiting);
                provider.binding_turn = !provider.binding.is_empty();
                let entries = holds
                    .into_iter()
                    .filter_map(|hold| self.pending_entry(link, origin, hold))
                    .collect();
                self.send_own(
                    link,
                    origin,
                    Message::State {
                        origin: None,
                        holds: entries,
                    },
                );
            } else {
                return;
            }
        }
    }

    /// A hold's pending ops as one entry, or its members' values after an
    /// overflow (each subscribed afresh, so no change is counted twice).
    fn pending_entry(&mut self, link: u64, origin: u64, hold: u64) -> Option<StateEntry> {
        let compress = self.options.compress && self.links[&link].role != Role::Parent;
        let mut resubscribe = Vec::new();
        let served = self.provider(link, origin)?.holds.get_mut(&hold)?;
        let HoldState::Bound { followed, .. } = &mut served.state else {
            return None;
        };
        let mut members = Vec::new();
        for member in followed.iter_mut() {
            match std::mem::replace(&mut member.pending, Pending::Ops(Vec::new())) {
                Pending::Ops(ops) if ops.is_empty() => {}
                Pending::Ops(ops) => {
                    let wire = member.encoder.encode(&ops);
                    members.push((
                        member.index,
                        origin_part(Value::Array(wire).to_string(), compress),
                    ));
                }
                Pending::Overflowed => resubscribe.push(member.index),
            }
        }
        for index in resubscribe {
            let sub = self.next_id();
            let tx = self.tx.clone();
            let served = self.provider(link, origin)?.holds.get_mut(&hold)?;
            let HoldState::Bound { followed, .. } = &mut served.state else {
                return None;
            };
            let member = followed.iter_mut().find(|m| m.index == index)?;
            member.cell.unlisten(member.listener);
            let previous = member.sub;
            let (listener, value) = member.cell.listen(Arc::new(move |change: &CellChange| {
                let _ = tx.send(Msg::Changed {
                    sub,
                    ops: change.ops.clone(),
                });
            }));
            member.listener = listener;
            member.sub = sub;
            let wire = member.encoder.encode(&[Op::Replace((*value).clone())]);
            members.push((index, origin_part(Value::Array(wire).to_string(), compress)));
            self.subs.remove(&previous);
            self.subs.insert(sub, (link, origin, hold));
        }
        members.sort_by_key(|(index, _)| *index);
        Some(StateEntry {
            hold,
            members,
            failed: Vec::new(),
            retired: Vec::new(),
        })
    }

    /// A cell changed: its ops join the hold's batch.
    fn changed(&mut self, sub: u64, ops: Arc<Vec<Op>>) {
        let Some(&(link, origin, hold)) = self.subs.get(&sub) else {
            return;
        };
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        let Some(served) = provider.holds.get_mut(&hold) else {
            return;
        };
        let HoldState::Bound { followed, .. } = &mut served.state else {
            return;
        };
        let Some(member) = followed.iter_mut().find(|member| member.sub == sub) else {
            return;
        };
        member.changes.extend(ops.iter().cloned());
        if !provider.touched.contains(&hold) {
            provider.touched.push(hold);
        }
        if !self.dirty.contains(&(link, origin)) {
            self.dirty.push((link, origin));
        }
    }

    /// The end of a batch: one `state` message per side while a slot is free
    /// and nothing waits; otherwise the ops join the pending queues.
    fn flush(&mut self) {
        let compress = self.options.compress;
        for (link, origin) in std::mem::take(&mut self.dirty) {
            let compress = compress
                && self
                    .links
                    .get(&link)
                    .is_some_and(|l| l.role != Role::Parent);
            let Some(provider) = self.provider(link, origin) else {
                continue;
            };
            let touched = std::mem::take(&mut provider.touched);
            let free = provider.free();
            let mut entries = Vec::new();
            for hold in touched {
                let Some(served) = provider.holds.get_mut(&hold) else {
                    continue;
                };
                let HoldState::Bound { followed, .. } = &mut served.state else {
                    continue;
                };
                if free {
                    let mut members = Vec::new();
                    for member in followed.iter_mut() {
                        if member.changes.is_empty() {
                            continue;
                        }
                        let ops = std::mem::take(&mut member.changes);
                        let wire = member.encoder.encode(&ops);
                        members.push((
                            member.index,
                            origin_part(Value::Array(wire).to_string(), compress),
                        ));
                    }
                    if !members.is_empty() {
                        entries.push(StateEntry {
                            hold,
                            members,
                            failed: Vec::new(),
                            retired: Vec::new(),
                        });
                    }
                } else {
                    let mut queued = false;
                    for member in followed.iter_mut() {
                        if member.changes.is_empty() {
                            continue;
                        }
                        let ops = std::mem::take(&mut member.changes);
                        member.pending.push(ops);
                        queued = true;
                    }
                    if queued && !provider.waiting.contains(&hold) {
                        provider.waiting.push(hold);
                    }
                }
            }
            if !entries.is_empty() {
                self.send_own(
                    link,
                    origin,
                    Message::State {
                        origin: None,
                        holds: entries,
                    },
                );
            }
        }
    }

    /// A hold ends: no longer subscribed, its object loses it.
    fn drop_hold(&mut self, link: u64, origin: u64, hold: u64, served: ServedHold) {
        if let HoldState::Bound { followed, .. } = served.state {
            for member in followed {
                member.cell.unlisten(member.listener);
                self.subs.remove(&member.sub);
            }
        }
        if let Some(provider) = self.provider(link, origin) {
            provider.binding.retain(|id| *id != hold);
            provider.waiting.retain(|id| *id != hold);
            provider.touched.retain(|id| *id != hold);
        }
        if let Some(key) = served.object {
            self.emit(HostEvent::Released {
                link,
                hold,
                type_name: served.request.type_name.clone(),
            });
            if let Some(object) = self.objects.get_mut(&key) {
                object.holds -= 1;
                if object.holds == 0 {
                    let object = self.objects.remove(&key).expect("the object");
                    if let Some(dispose) = object.on_dispose {
                        dispose();
                    }
                }
            }
        }
    }

    /// Aborts the calls running on a hold that ended.
    fn forget_calls(&mut self, link: u64, origin: u64, hold: u64, reason: ChordError) {
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        let ids: Vec<u64> = provider
            .calls
            .iter()
            .filter(|(_, call)| call.hold == hold)
            .map(|(id, _)| *id)
            .collect();
        let mut tokens = Vec::new();
        for id in ids {
            if let Some(call) = provider.calls.remove(&id) {
                call.signal.abort(reason.clone());
                tokens.push(call.token);
            }
        }
        for token in tokens {
            self.running.remove(&token);
        }
    }

    // ─── provider: calls ───────────────────────────────────────────────────

    fn served_call(
        &mut self,
        link: u64,
        origin: u64,
        call: u64,
        hold: u64,
        method: u64,
        args: Option<Part>,
    ) -> Result<(), String> {
        let role = self.links[&link].role;
        let Some(provider) = self.provider(link, origin) else {
            return Ok(());
        };
        if provider.calls.contains_key(&call) {
            return Err(format!("call {call}: the id is in use"));
        }
        let Some(served) = provider.holds.get(&hold) else {
            return Ok(());
        };
        if provider.calls.len() as u64 >= CALLS {
            return Err(format!("call {call}: more than {CALLS} calls"));
        }
        if matches!(args, Some(Part::Deflated(_))) && role == Role::Child {
            return Err(format!("call {call}: compressed arguments from a child"));
        }
        let HoldState::Bound { methods, .. } = &served.state else {
            let error = ChordError::new(Code::NotReady, format!("hold {hold}: not bound yet"));
            self.refuse_call(link, origin, call, error.into());
            return Ok(());
        };
        let Some(name) = methods.get(method as usize).cloned() else {
            return Err(format!("hold {hold}: no method {method}"));
        };
        let limits = served.limits;
        let principal = served.principal.clone();
        let key = served.object.clone().expect("a bound hold");
        let args = match args {
            None => Value::Null,
            Some(part) => match open_part(&part, limits) {
                Ok(bytes) => serde_json::from_slice(&bytes)
                    .map_err(|error| format!("call {call}: malformed arguments ({error})"))?,
                Err(OpenError::TooLarge(error)) => {
                    self.refuse_call(link, origin, call, error.into());
                    return Ok(());
                }
                Err(OpenError::Corrupt(error)) => return Err(error),
            },
        };
        let object = &self.objects[&key];
        if let Some(declaration) = object.contract.method(&name) {
            if let Err(error) = crate::validate::check(&declaration.args, &args) {
                let error = ChordError::invalid("args", format!("{name}: {error}"));
                self.refuse_call(link, origin, call, error.into());
                return Ok(());
            }
        }
        let Some(function) = object.methods.get(&name).cloned() else {
            let error = ChordError::invalid("member", format!("{name}: not implemented"));
            self.refuse_call(link, origin, call, error.into());
            return Ok(());
        };
        let token = self.next_id();
        let signal = Signal::default();
        let provider = self.provider(link, origin).expect("the provider");
        provider.calls.insert(
            call,
            RunningCall {
                hold,
                token,
                signal: signal.clone(),
            },
        );
        self.running.insert(token, (link, origin, call));
        let future = function(args, CallContext { principal, signal });
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = future.await;
            let _ = tx.send(Msg::Replied { token, result });
        });
        Ok(())
    }

    fn refuse_call(&mut self, link: u64, origin: u64, call: u64, error: WireError) {
        self.send_origin(
            link,
            origin,
            Message::Error {
                origin: None,
                call,
                error: error.to_json(),
            },
        );
    }

    /// A served call settled: its reply goes, unless it was aborted,
    /// released or its link closed meanwhile.
    fn replied(&mut self, token: u64, result: Result<Value, WireError>) {
        let Some((link, origin, call)) = self.running.remove(&token) else {
            return;
        };
        let compress = self.options.compress
            && self
                .links
                .get(&link)
                .is_some_and(|l| l.role != Role::Parent);
        let Some(provider) = self.provider(link, origin) else {
            return;
        };
        if provider.calls.remove(&call).is_none() {
            return;
        }
        let reply = match result {
            Ok(Value::Null) => Message::Result {
                origin: None,
                call,
                value: None,
                segments: None,
            },
            Ok(value) => Message::Result {
                origin: None,
                call,
                value: Some(origin_part(value.to_string(), compress)),
                segments: None,
            },
            Err(error) => Message::Error {
                origin: None,
                call,
                error: error.to_json(),
            },
        };
        self.send_origin(link, origin, reply);
    }

    // ─── consumer ──────────────────────────────────────────────────────────

    /// Holds reference `reference` on `link`.
    fn hold_on(&mut self, reference: u64, link: u64) {
        let Some(entry) = self.refs.get(&reference) else {
            return;
        };
        let contract = entry.contract.clone();
        let params = entry.params.clone();
        let Some(host_link) = self.links.get_mut(&link) else {
            return;
        };
        let hold = host_link.consumer.next_hold;
        host_link.consumer.next_hold += 1;
        host_link.consumer.holds.insert(hold, reference);
        let message = Message::Hold {
            origin: None,
            hold,
            type_name: contract.declaration.name.clone(),
            version: contract.declaration.version,
            fingerprint: contract.fingerprint.clone(),
            params: Part::Text(params.to_string()),
            address: contract.address(&params),
            principal: None,
            incarnation: None,
            chain: None,
            limit: None,
        };
        host_link.channel.send(&message);
        let entry = self.refs.get_mut(&reference).expect("the reference");
        entry.held = Some((link, hold));
    }

    fn consumer_ref(&self, link: u64, hold: u64) -> Option<u64> {
        self.links.get(&link)?.consumer.holds.get(&hold).copied()
    }

    /// One `bound` message: each entry applied to its reference, which goes
    /// live; then acknowledged.
    fn bound(&mut self, link: u64, holds: Vec<BoundEntry>) -> Result<(), String> {
        let limits = self.links[&link].limits;
        let mut live = Vec::new();
        for entry in holds {
            let Some(reference) = self.consumer_ref(link, entry.hold) else {
                continue;
            };
            let Some(ref_entry) = self.refs.get_mut(&reference) else {
                continue;
            };
            let contract = ref_entry.contract.clone();
            let order = contract.variant_order();
            let Some(variant) = order.get(entry.variant as usize) else {
                return Err(format!("hold {}: no variant {}", entry.hold, entry.variant));
            };
            let (state, methods) = contract.variant_names(variant).expect("the variant");
            let mut members = BTreeMap::new();
            for name in &state {
                members.insert(
                    (*name).to_owned(),
                    MemberReplica {
                        value: None,
                        decoder: Decoder::new(),
                    },
                );
            }
            let mut snapshots = Vec::new();
            for (index, part) in &entry.members {
                let Some(name) = state.get(*index as usize) else {
                    return Err(format!("hold {}: no member {index}", entry.hold));
                };
                let value: Value = match open_part(part, limits) {
                    Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                        format!("hold {}: a malformed value ({error})", entry.hold)
                    })?,
                    Err(OpenError::TooLarge(error)) => {
                        snapshots.push(((*name).to_owned(), StateUpdate::Failed(error.into())));
                        continue;
                    }
                    Err(OpenError::Corrupt(error)) => return Err(error),
                };
                members.get_mut(*name).expect("a member").value = Some(value.clone());
                snapshots.push(((*name).to_owned(), StateUpdate::Snapshot(value)));
            }
            for failed in &entry.failed {
                if let Some(name) = state.get(failed.member as usize) {
                    let error = WireError::from_json(&failed.error)?;
                    snapshots.push(((*name).to_owned(), StateUpdate::Failed(error)));
                }
            }
            ref_entry.variant = Some((*variant).to_owned());
            ref_entry.methods = methods.iter().map(|name| (*name).to_owned()).collect();
            ref_entry.state_names = state.iter().map(|name| (*name).to_owned()).collect();
            ref_entry.members = members;
            for (member, update) in snapshots {
                ref_entry.notify(&member, update);
            }
            ref_entry.set_status(RefStatus::Live {
                variant: (*variant).to_owned(),
            });
            live.push(reference);
        }
        self.send(link, &Message::Ack { origin: None });
        for reference in live {
            self.send_queued_calls(reference);
        }
        Ok(())
    }

    /// One `state` message: each entry's ops applied to its replica; then
    /// acknowledged.
    fn state(&mut self, link: u64, holds: Vec<StateEntry>) -> Result<(), String> {
        let limits = self.links[&link].limits;
        for entry in holds {
            let Some(reference) = self.consumer_ref(link, entry.hold) else {
                continue;
            };
            let Some(ref_entry) = self.refs.get_mut(&reference) else {
                continue;
            };
            let mut updates = Vec::new();
            for (index, part) in &entry.members {
                let Some(name) = ref_entry.state_names.get(*index as usize).cloned() else {
                    return Err(format!("hold {}: no member {index}", entry.hold));
                };
                let wire: Value = match open_part(part, limits) {
                    Ok(bytes) => serde_json::from_slice(&bytes)
                        .map_err(|error| format!("hold {}: malformed ops ({error})", entry.hold))?,
                    Err(OpenError::TooLarge(error)) => {
                        updates.push((name, StateUpdate::Failed(error.into())));
                        continue;
                    }
                    Err(OpenError::Corrupt(error)) => return Err(error),
                };
                let Value::Array(wire) = wire else {
                    return Err(format!("hold {}: ops are no list", entry.hold));
                };
                let replica = ref_entry.members.get_mut(&name).expect("a member");
                let ops = replica
                    .decoder
                    .decode(&wire)
                    .map_err(|error| format!("hold {}: {error}", entry.hold))?;
                let base = replica.value.take().unwrap_or(Value::Null);
                let value = apply_owned(base, &ops)
                    .map_err(|error| format!("hold {}: {error}", entry.hold))?;
                replica.value = Some(value.clone());
                updates.push((name, StateUpdate::Ops { ops, value }));
            }
            for failed in &entry.failed {
                if let Some(name) = ref_entry.state_names.get(failed.member as usize).cloned() {
                    let error = WireError::from_json(&failed.error)?;
                    updates.push((name, StateUpdate::Failed(error)));
                }
            }
            for (member, update) in updates {
                ref_entry.notify(&member, update);
            }
        }
        self.send(link, &Message::Ack { origin: None });
        Ok(())
    }

    fn consumer_failed(
        &mut self,
        link: u64,
        hold: u64,
        cause: CauseKind,
        error: WireError,
        retry_in: Option<u64>,
    ) {
        let Some(reference) = self.consumer_ref(link, hold) else {
            return;
        };
        self.fail_waiting_calls(reference, error.clone());
        if let Some(entry) = self.refs.get_mut(&reference) {
            entry.variant = None;
            entry.set_status(RefStatus::Failed {
                cause,
                error,
                retry_in,
            });
        }
    }

    /// Calls of `reference` not sent yet fail with `error`.
    fn fail_waiting_calls(&mut self, reference: u64, error: impl Into<WireError>) {
        let error = error.into();
        let tokens: Vec<u64> = self
            .outgoing
            .iter()
            .filter(|(_, call)| call.reference == reference && call.id.is_none())
            .map(|(token, _)| *token)
            .collect();
        for token in tokens {
            if let Some(call) = self.outgoing.remove(&token) {
                if let Some(link) = self.links.get_mut(&call.link) {
                    link.consumer.waiting.retain(|waiting| *waiting != token);
                }
                let _ = call.reply.send(Err(error.clone()));
            }
        }
        if let Some(entry) = self.refs.get_mut(&reference) {
            for (_, _, _, reply) in std::mem::take(&mut entry.queued) {
                let _ = reply.send(Err(error.clone()));
            }
        }
    }

    /// Drops a reference's hold: its calls fail with `error`.
    fn forget_reference(&mut self, reference: u64, error: WireError) {
        let Some(entry) = self.refs.get_mut(&reference) else {
            return;
        };
        let held = entry.held.take();
        entry.variant = None;
        let tokens: Vec<u64> = self
            .outgoing
            .iter()
            .filter(|(_, call)| call.reference == reference)
            .map(|(token, _)| *token)
            .collect();
        for token in tokens {
            let call = self.outgoing.remove(&token).expect("the call");
            if let Some(link) = self.links.get_mut(&call.link) {
                link.consumer.waiting.retain(|waiting| *waiting != token);
                if let Some(id) = call.id {
                    link.consumer.calls.remove(&id);
                    link.consumer.in_flight -= 1;
                }
            }
            let _ = call.reply.send(Err(error.clone()));
        }
        if let Some((link, hold)) = held {
            if let Some(link) = self.links.get_mut(&link) {
                link.consumer.holds.remove(&hold);
            }
            self.drain_calls(link);
        }
    }

    fn release(&mut self, reference: u64) {
        let Some(entry) = self.refs.get(&reference) else {
            return;
        };
        if let Some((link, hold)) = entry.held {
            self.send(link, &Message::Release { origin: None, hold });
        }
        self.forget_reference(reference, released().into());
        if let Some(entry) = self.refs.remove(&reference) {
            if let Some(path) = &entry.path {
                self.path_detach(path, Holder::Ref(reference));
            }
            entry.set_status(RefStatus::Released);
        }
    }

    fn call(
        &mut self,
        reference: u64,
        token: u64,
        method: &str,
        args: Value,
        reply: oneshot::Sender<Result<Value, WireError>>,
    ) {
        let Some(entry) = self.refs.get_mut(&reference) else {
            let _ = reply.send(Err(released().into()));
            return;
        };
        let status = entry.status.borrow().clone();
        match status {
            RefStatus::Live { .. } => {}
            RefStatus::Loading => {
                // Sent once live, by name then.
                entry.queued.push((token, method.to_owned(), args, reply));
                return;
            }
            RefStatus::Failed { error, .. } | RefStatus::Gone(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            RefStatus::Released => {
                let _ = reply.send(Err(released().into()));
                return;
            }
        }
        self.submit(reference, token, method, args, reply);
    }

    /// Sends a call on a live reference within the link's call window.
    fn submit(
        &mut self,
        reference: u64,
        token: u64,
        method: &str,
        args: Value,
        reply: oneshot::Sender<Result<Value, WireError>>,
    ) {
        let Some(entry) = self.refs.get(&reference) else {
            let _ = reply.send(Err(released().into()));
            return;
        };
        let Some((link, hold)) = entry.held else {
            let _ = reply.send(Err(link_closed().into()));
            return;
        };
        let Some(index) = entry.methods.iter().position(|name| name == method) else {
            let variant = entry.variant.clone().unwrap_or_default();
            let error =
                ChordError::invalid("member", format!("{method}: not a method of {variant}"));
            let _ = reply.send(Err(error.into()));
            return;
        };
        let takes_none = entry
            .contract
            .method(method)
            .is_some_and(|declaration| declaration.args == json!({"type": "null"}));
        let Some(host_link) = self.links.get(&link) else {
            let _ = reply.send(Err(link_closed().into()));
            return;
        };
        let compress = self.options.compress && host_link.role != Role::Parent;
        let args = (!takes_none).then(|| origin_part(args.to_string(), compress));
        self.outgoing.insert(
            token,
            OutgoingCall {
                reference,
                link,
                hold,
                method: index as u64,
                args,
                reply,
                id: None,
            },
        );
        self.links
            .get_mut(&link)
            .expect("the link")
            .consumer
            .waiting
            .push_back(token);
        self.drain_calls(link);
    }

    /// A reference went live: the calls made while it loaded go.
    fn send_queued_calls(&mut self, reference: u64) {
        let Some(entry) = self.refs.get_mut(&reference) else {
            return;
        };
        for (token, method, args, reply) in std::mem::take(&mut entry.queued) {
            self.submit(reference, token, &method, args, reply);
        }
    }

    /// Sends waiting calls in order while the peer's window has room.
    fn drain_calls(&mut self, link: u64) {
        loop {
            let Some(host_link) = self.links.get_mut(&link) else {
                return;
            };
            if !host_link.channel.is_open() {
                return;
            }
            let consumer = &mut host_link.consumer;
            if consumer.in_flight >= consumer.limit {
                return;
            }
            let Some(token) = consumer.waiting.pop_front() else {
                return;
            };
            let Some(call) = self.outgoing.get_mut(&token) else {
                continue;
            };
            let id = consumer.next_call;
            consumer.next_call += 1;
            consumer.in_flight += 1;
            consumer.calls.insert(id, token);
            call.id = Some(id);
            let message = Message::Call {
                origin: None,
                call: id,
                hold: call.hold,
                method: call.method,
                args: call.args.clone(),
                segments: None,
            };
            host_link.channel.send(&message);
        }
    }

    /// A call's caller dropped it: a waiting one leaves, one in flight sends
    /// `abort` and frees its slot.
    fn cancel(&mut self, token: u64) {
        for entry in self.refs.values_mut() {
            entry.queued.retain(|(queued, ..)| *queued != token);
        }
        let Some(call) = self.outgoing.remove(&token) else {
            return;
        };
        let Some(link) = self.links.get_mut(&call.link) else {
            return;
        };
        match call.id {
            None => link.consumer.waiting.retain(|waiting| *waiting != token),
            Some(id) => {
                link.consumer.calls.remove(&id);
                link.consumer.in_flight -= 1;
                link.channel.send(&Message::Abort {
                    origin: None,
                    call: id,
                });
                let link = call.link;
                self.drain_calls(link);
            }
        }
    }

    /// A reply settles its call; one for a call given up is ignored.
    fn reply(
        &mut self,
        link: u64,
        call: u64,
        reply: Result<Option<Part>, WireError>,
    ) -> Result<(), String> {
        let limits = self.links[&link].limits;
        let consumer = &mut self.links.get_mut(&link).expect("the link").consumer;
        let Some(token) = consumer.calls.remove(&call) else {
            return Ok(());
        };
        consumer.in_flight -= 1;
        let outgoing = self.outgoing.remove(&token);
        self.drain_calls(link);
        let Some(outgoing) = outgoing else {
            return Ok(());
        };
        let result = match reply {
            Err(error) => Err(error),
            Ok(None) => Ok(Value::Null),
            Ok(Some(part)) => match open_part(&part, limits) {
                Ok(bytes) => match serde_json::from_slice(&bytes) {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        let _ = outgoing.reply.send(Err(ChordError::new(
                            Code::Unavailable,
                            format!("call {call}: a malformed reply"),
                        )
                        .into()));
                        return Err(format!("call {call}: a malformed reply ({error})"));
                    }
                },
                Err(OpenError::TooLarge(error)) => Err(error.into()),
                Err(OpenError::Corrupt(error)) => {
                    let _ = outgoing.reply.send(Err(ChordError::new(
                        Code::Unavailable,
                        format!("call {call}: a malformed reply"),
                    )
                    .into()));
                    return Err(error);
                }
            },
        };
        let _ = outgoing.reply.send(result);
        Ok(())
    }
}

/// `message` with `origin` (0: the peer's own work, none).
fn with_origin(mut message: Message, origin: u64) -> Message {
    if origin == 0 {
        return message;
    }
    match &mut message {
        Message::Hold { origin: o, .. }
        | Message::Release { origin: o, .. }
        | Message::Call { origin: o, .. }
        | Message::Abort { origin: o, .. }
        | Message::Retry { origin: o, .. }
        | Message::Bound { origin: o, .. }
        | Message::State { origin: o, .. }
        | Message::Ack { origin: o }
        | Message::Failed { origin: o, .. }
        | Message::Loading { origin: o, .. }
        | Message::Result { origin: o, .. }
        | Message::Error { origin: o, .. }
        | Message::Gone { origin: o, .. } => *o = Some(origin),
        Message::Closed { .. }
        | Message::Hello { .. }
        | Message::Revoke { .. }
        | Message::Ping
        | Message::Pong => {}
    }
    message
}
