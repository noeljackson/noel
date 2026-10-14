//! Hosts, their connections and the references they hold: the subset of
//! upstream's `chord/src/host.ts`, `connection.ts` and `remote.ts` noel runs.
//!
//! A [`Host`] serves object types (contracts with a factory that picks a
//! variant per principal), accepts links from clients and child hosts,
//! connects to a parent, relays holds for `at` hosts through its routes, and
//! holds remote objects itself ([`Host::get`]). It does not reload: there is
//! one code version, 1, and reload control from a parent is answered
//! `unavailable`.
//!
//! All of a host's state lives in one task (the actor), which takes link
//! frames, method results, state changes and API requests from one queue, in
//! order, as upstream's single-threaded runtime does. Methods run on their own
//! tasks; what they publish reaches the actor before their result.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::contract::{coverage, fingerprint, Description, MemberDescription, Scope};
use crate::delta::{apply_owned, Decoder, Encoder, Op};
use crate::error::{ChordError, ErrorCode};
use crate::link::{Inbound, Link};
use crate::object::{
    AbortSignal, BoxFuture, CallContext, Cell, CellChange, Implementation, Member,
};
use crate::schema::{canonical_json, deep_equal};
use crate::validate;
use crate::wire::{Frame, Kind, RootRef, StateFrame, Token, PROTOCOL_VERSION};

/// The one code version noel runs.
const CODE_VERSION: u64 = 1;
const DEFAULT_WINDOW: usize = 256 * 1024;
/// A waiting subscription keeps at most this many windows of ops; beyond, its
/// current value replaces them.
const PENDING_WINDOWS: usize = 4;

// ─── options ────────────────────────────────────────────────────────────────

/// Maps a principal to the key objects are kept by (default: canonical JSON).
pub type PrincipalKey = Arc<dyn Fn(&Value) -> String + Send + Sync>;
/// On `invalidate_principal`: a new principal keeps the link, `None` closes
/// it.
pub type Restamp = Arc<dyn Fn(&Value) -> Option<Value> + Send + Sync>;
/// Receives errors from background work.
pub type ErrorHandler = Arc<dyn Fn(&ChordError) + Send + Sync>;
/// Hears a host's [`HostEvent`]s.
pub type Observer = Arc<dyn Fn(&HostEvent) + Send + Sync>;
/// A factory: `(input, principal)` to the implementation that principal gets.
pub type Factory = Arc<dyn Fn(&Value, &Value) -> Result<Implementation, ChordError> + Send + Sync>;

pub struct HostOptions {
    /// `server`, `session:S1`, ...; how others address this host.
    pub name: String,
    /// `Client`: never addressable, never carries roots.
    pub kind: Kind,
    pub principal_key: Option<PrincipalKey>,
    /// Errors from background work (protocol violations, failed subscriptions).
    pub on_error: Option<ErrorHandler>,
    /// Links opening and closing, holds made and released.
    pub observer: Option<Observer>,
}

impl HostOptions {
    pub fn host(name: impl Into<String>) -> Self {
        HostOptions {
            name: name.into(),
            kind: Kind::Host,
            principal_key: None,
            on_error: None,
            observer: None,
        }
    }

    pub fn client(name: impl Into<String>) -> Self {
        HostOptions {
            kind: Kind::Client,
            ..HostOptions::host(name)
        }
    }
}

/// Per-link settings.
#[derive(Clone, Debug)]
pub struct LinkOptions {
    /// Time between pings (default 15 s).
    pub ping_interval: Duration,
    /// The link closes when nothing arrived for this long (default 45 s).
    pub ping_timeout: Duration,
    /// Unacknowledged frame bytes beyond which state frames wait (default
    /// 256 KiB); `None` never holds state back.
    pub window: Option<usize>,
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            ping_interval: Duration::from_secs(15),
            ping_timeout: Duration::from_secs(45),
            window: Some(DEFAULT_WINDOW),
        }
    }
}

/// How an accepted link is treated. The acceptor decides; the peer's
/// `hello` must agree.
#[derive(Clone)]
pub struct AcceptOptions {
    /// Every request on the link acts as this principal.
    pub principal: Value,
    /// `Client` (default) or `Host` (a child host; needs `name`).
    pub kind: Kind,
    /// A child host's name: its hello must say it, and it is reachable `at` it.
    pub name: Option<String>,
    pub restamp: Option<Restamp>,
    /// Calls the peer may have in progress; more are refused with
    /// `unavailable/too_many_calls`.
    pub max_calls: Option<usize>,
    pub link: LinkOptions,
}

impl AcceptOptions {
    pub fn client(principal: Value) -> Self {
        AcceptOptions {
            principal,
            kind: Kind::Client,
            name: None,
            restamp: None,
            max_calls: None,
            link: LinkOptions::default(),
        }
    }

    pub fn host(name: impl Into<String>, principal: Value) -> Self {
        AcceptOptions {
            kind: Kind::Host,
            name: Some(name.into()),
            ..AcceptOptions::client(principal)
        }
    }
}

/// Where holds for hosts named `prefix…` go.
pub trait Route: Send + Sync + 'static {
    /// Checked before resolving (or spawning) and again after.
    fn authorize(&self, principal: &Value, host_name: &str) -> BoxFuture<bool>;
    /// A link to the host, spawned if needed. Returning a link already in use
    /// reuses its connection.
    fn resolve(&self, host_name: &str) -> BoxFuture<Result<Link, ChordError>>;
    /// The principal the resolved host acts as when it calls back.
    fn principal(&self, host_name: &str) -> Value {
        json!({ "host": host_name })
    }
}

/// Which principals `invalidate_principal` matches.
#[derive(Clone)]
pub enum PrincipalMatch {
    /// Fields that must be equal; propagates to child hosts.
    Pattern(Map<String, Value>),
    /// This host only.
    Predicate(Arc<dyn Fn(&Value) -> bool + Send + Sync>),
}

impl PrincipalMatch {
    fn matches(&self, principal: &Value) -> bool {
        match self {
            PrincipalMatch::Pattern(pattern) => {
                let Some(object) = principal.as_object() else {
                    return false;
                };
                pattern
                    .iter()
                    .all(|(key, value)| deep_equal(object.get(key), Some(value)))
            }
            PrincipalMatch::Predicate(predicate) => predicate(principal),
        }
    }
}

/// What a host's observer hears.
#[derive(Clone, Debug)]
pub enum HostEvent {
    LinkOpened {
        link: u64,
        kind: Kind,
        principal: Option<Value>,
    },
    LinkClosed {
        link: u64,
        reason: String,
    },
    /// A peer holds a local object.
    Held {
        link: u64,
        id: u64,
        type_name: String,
        input: Value,
        principal: Value,
    },
    /// A peer's hold ended: released, its link closed, or the object forced.
    Released {
        link: u64,
        id: u64,
        type_name: String,
        input: Value,
        principal: Value,
    },
}

/// A provider's shape for a hold.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub variant: Option<String>,
    pub members: Vec<String>,
    /// The provider contract's fingerprint.
    pub contract: String,
    pub code_version: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RefStatus {
    Loading,
    Live(Shape),
    Released,
    Gone(ChordError),
}

/// What a state subscription delivers.
#[derive(Clone, Debug, PartialEq)]
pub enum StateUpdate {
    /// The provider's snapshot.
    Snapshot(Value),
    /// A new value replacing what came before (the provider's `reset`).
    Reset(Value),
    /// Decoded ops and the value after them.
    Ops { ops: Vec<Op>, value: Value },
    /// The provider refused the subscription or a snapshot.
    Error(ChordError),
}

// ─── handles ────────────────────────────────────────────────────────────────

struct Shared {
    name: String,
    kind: Kind,
    next_ref: AtomicU64,
    next_key: AtomicU64,
}

/// A host. Cloning shares it. Create it inside a tokio runtime.
#[derive(Clone)]
pub struct Host {
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
}

impl Host {
    pub fn new(options: HostOptions) -> Host {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            name: options.name.clone(),
            kind: options.kind,
            next_ref: AtomicU64::new(1),
            next_key: AtomicU64::new(1),
        });
        let actor = Actor::new(options, tx.downgrade());
        tokio::spawn(actor.run(rx));
        Host { tx, shared }
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }

    pub fn kind(&self) -> Kind {
        self.shared.kind
    }

    /// Serves `contract` (a remote contract) with `factory`.
    pub fn serve<F>(&self, contract: Description, factory: F)
    where
        F: Fn(&Value, &Value) -> Result<Implementation, ChordError> + Send + Sync + 'static,
    {
        let _ = self.tx.send(Msg::Serve(contract, Arc::new(factory)));
    }

    /// Accepts a child link (a client, or a worker host).
    pub fn accept(&self, link: Link, options: AcceptOptions) {
        let _ = self.tx.send(Msg::Accept(link, options));
    }

    /// Sets (or replaces) the link to this host's parent; remote references
    /// that lost their link hold again over it.
    pub fn connect(&self, link: Link, options: LinkOptions) {
        let _ = self.tx.send(Msg::Connect(link, options));
    }

    /// Routes holds for hosts named `prefix…`.
    pub fn route(&self, prefix: impl Into<String>, route: impl Route) {
        let _ = self.tx.send(Msg::Route(prefix.into(), Arc::new(route)));
    }

    /// A reference on an object of `contract` at host `at`, as this host's
    /// own principal.
    pub fn get(&self, contract: Description, input: Value, at: &str) -> RemoteRef {
        self.reference(contract, input, at, None)
    }

    /// As [`Host::get`], forwarding `principal` (honoured only by a host whose
    /// parent this host is; a server ignores it from its clients).
    pub fn get_as(
        &self,
        contract: Description,
        input: Value,
        at: &str,
        principal: Value,
    ) -> RemoteRef {
        self.reference(contract, input, at, Some(principal))
    }

    fn reference(
        &self,
        contract: Description,
        input: Value,
        at: &str,
        principal: Option<Value>,
    ) -> RemoteRef {
        let id = self.shared.next_ref.fetch_add(1, Ordering::Relaxed);
        let (status, receiver) = watch::channel(RefStatus::Loading);
        let contract = Arc::new(contract);
        let _ = self.tx.send(Msg::Get {
            id,
            contract: contract.clone(),
            at: at.to_owned(),
            input,
            principal,
            status,
        });
        RemoteRef {
            id,
            tx: self.tx.clone(),
            shared: self.shared.clone(),
            status: receiver,
            contract,
        }
    }

    /// Force-disposes every object whose principal matches, re-stamps or
    /// closes matching links (closing their relays), and sends a pattern to
    /// child hosts.
    pub async fn invalidate_principal(&self, matching: PrincipalMatch) {
        let (reply, done) = oneshot::channel();
        if self.tx.send(Msg::Invalidate(matching, Some(reply))).is_ok() {
            let _ = done.await;
        }
    }

    /// Disposes every object (holders hear `gone`), then closes every link
    /// with `host closed`, and waits (at most two seconds) for the links to
    /// write what they were sent.
    pub async fn close(&self) {
        let (reply, done) = oneshot::channel();
        if self.tx.send(Msg::Close(reply)).is_ok() {
            if let Ok(links) = done.await {
                let flushed = futures_util::future::join_all(links.iter().map(Link::flushed));
                let _ = tokio::time::timeout(Duration::from_secs(2), flushed).await;
            }
        }
    }
}

/// A reference on a remote object. Late-bound: calls made while it loads
/// wait until it is live.
#[derive(Clone)]
pub struct RemoteRef {
    id: u64,
    tx: mpsc::UnboundedSender<Msg>,
    shared: Arc<Shared>,
    status: watch::Receiver<RefStatus>,
    contract: Arc<Description>,
}

impl RemoteRef {
    pub fn contract(&self) -> &Description {
        &self.contract
    }

    pub fn status(&self) -> RefStatus {
        self.status.borrow().clone()
    }

    /// The shape once live; the error if the hold is gone or released first.
    pub async fn ready(&self) -> Result<Shape, ChordError> {
        let mut status = self.status.clone();
        let settled = status
            .wait_for(|status| !matches!(status, RefStatus::Loading))
            .await
            .map(|status| status.clone())
            .unwrap_or(RefStatus::Gone(ChordError::unavailable("host closed")));
        match settled {
            RefStatus::Live(shape) => Ok(shape),
            RefStatus::Gone(error) => Err(error),
            RefStatus::Released => Err(ChordError::new(ErrorCode::Released, "reference released")),
            RefStatus::Loading => unreachable!("waited for another status"),
        }
    }

    /// Calls `member`. `args` is ignored for methods that take none; the
    /// result is `None` for void methods. Dropping the future before it
    /// settles cancels the call.
    pub async fn call(
        &self,
        member: &str,
        args: Option<Value>,
    ) -> Result<Option<Value>, ChordError> {
        let key = self.shared.next_key.fetch_add(1, Ordering::Relaxed);
        let (reply, result) = oneshot::channel();
        let mut guard = CancelOnDrop {
            tx: self.tx.clone(),
            reference: self.id,
            key,
            armed: true,
        };
        let _ = self.tx.send(Msg::Call {
            reference: self.id,
            key,
            member: member.to_owned(),
            args,
            reply,
        });
        let outcome = result
            .await
            .unwrap_or_else(|_| Err(ChordError::unavailable("host closed")));
        guard.armed = false;
        outcome
    }

    /// Subscribes to the state member `member`: the provider's snapshot, then
    /// ops. Dropping the stream unsubscribes.
    pub fn subscribe(&self, member: &str) -> StateStream {
        let sink = self.shared.next_key.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = self.tx.send(Msg::Subscribe {
            reference: self.id,
            member: member.to_owned(),
            sink,
            sender,
        });
        StateStream {
            receiver,
            tx: self.tx.clone(),
            reference: self.id,
            member: member.to_owned(),
            sink,
        }
    }

    /// Releases the hold (and its subscriptions).
    pub fn release(&self) {
        let _ = self.tx.send(Msg::Release { reference: self.id });
    }
}

struct CancelOnDrop {
    tx: mpsc::UnboundedSender<Msg>,
    reference: u64,
    key: u64,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(Msg::Cancel {
                reference: self.reference,
                key: self.key,
            });
        }
    }
}

/// A state subscription. Dropping it unsubscribes.
pub struct StateStream {
    receiver: mpsc::UnboundedReceiver<StateUpdate>,
    tx: mpsc::UnboundedSender<Msg>,
    reference: u64,
    member: String,
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
            member: std::mem::take(&mut self.member),
            sink: self.sink,
        });
    }
}

// ─── the actor ──────────────────────────────────────────────────────────────

type ConnId = u64;
type RefId = u64;
type RelayId = u64;
type CallReply = oneshot::Sender<Result<Option<Value>, ChordError>>;

enum Msg {
    Serve(Description, Factory),
    Accept(Link, AcceptOptions),
    Connect(Link, LinkOptions),
    Route(String, Arc<dyn Route>),
    Inbound(ConnId, Inbound),
    PingTick(ConnId),
    CallDone {
        conn: ConnId,
        id: u64,
        root: String,
        outcome: Result<Option<Value>, ChordError>,
    },
    Cell {
        conn: ConnId,
        sub: u64,
        change: CellChange,
    },
    RelayStill {
        relay: RelayId,
        reply: oneshot::Sender<bool>,
    },
    RelayResolved {
        relay: RelayId,
        outcome: Result<(Link, Value), ChordError>,
    },
    RefResolved {
        reference: RefId,
        outcome: Result<(Link, Value), ChordError>,
    },
    Get {
        id: RefId,
        contract: Arc<Description>,
        at: String,
        input: Value,
        principal: Option<Value>,
        status: watch::Sender<RefStatus>,
    },
    Call {
        reference: RefId,
        key: u64,
        member: String,
        args: Option<Value>,
        reply: CallReply,
    },
    Cancel {
        reference: RefId,
        key: u64,
    },
    Subscribe {
        reference: RefId,
        member: String,
        sink: u64,
        sender: mpsc::UnboundedSender<StateUpdate>,
    },
    Unsubscribe {
        reference: RefId,
        member: String,
        sink: u64,
    },
    Release {
        reference: RefId,
    },
    Invalidate(PrincipalMatch, Option<oneshot::Sender<()>>),
    Close(oneshot::Sender<Vec<Link>>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// The link to this host's parent.
    Upstream,
    /// A link this host accepted (or resolved through a route).
    Downstream,
}

struct Service {
    description: Arc<Description>,
    fingerprint: String,
    factory: Factory,
}

struct Object {
    type_name: String,
    input: Value,
    principal: Value,
    service: Arc<Service>,
    variant: Option<String>,
    members: Vec<(String, Member)>,
    holders: Vec<(ConnId, u64)>,
    on_dispose: Option<Box<dyn FnOnce() + Send>>,
}

impl Object {
    fn member(&self, name: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, member)| member)
    }
}

struct LocalHold {
    object: String,
    principal: Value,
    feeds: BTreeSet<u64>,
    consumer: Arc<Description>,
}

enum Served {
    Local(LocalHold),
    Relay(RelayId),
}

struct Feed {
    object: String,
    member: String,
    cell: Option<(Cell, u64)>,
    encoder: Encoder,
}

enum Incoming {
    Local(AbortSignal),
    Relay(RelayId),
}

#[derive(Default)]
struct PendingState {
    base: Option<StateFrame>,
    ops: Vec<Value>,
    size: usize,
}

struct OutCall {
    reference: RefId,
    key: u64,
    root: Option<String>,
    reply: CallReply,
}

#[derive(Clone, Copy)]
enum SyncWaiter {
    Relay(RelayId),
    Ref(RefId),
}

struct Conn {
    link: Link,
    role: Role,
    peer_kind: Kind,
    peer_name: Option<String>,
    principal: Value,
    restamp: Option<Restamp>,
    generation: u64,
    parent_name: Option<String>,
    synced: bool,
    sync_waiters: Vec<SyncWaiter>,
    next_id: u64,
    last_seen: Instant,
    ping_n: i64,
    ping_timeout: Duration,
    pinger: Option<JoinHandle<()>>,
    max_calls: Option<usize>,
    window: Option<usize>,
    peer_acks: Option<bool>,
    sent: u64,
    acked: u64,
    unacked: VecDeque<usize>,
    unacked_bytes: usize,
    pending: Vec<(u64, PendingState)>,
    received: u64,
    mismatched: bool,
    // consumer side: what this host asked of the peer
    remotes: HashMap<u64, RefId>,
    calls: HashMap<u64, OutCall>,
    subs: HashMap<u64, (RefId, String)>,
    // provider side: what the peer asked of this host
    served: BTreeMap<u64, Served>,
    incoming: HashMap<u64, Incoming>,
    feeds: HashMap<u64, Feed>,
    relay_subs: HashMap<u64, RelayId>,
    // downstream side of relays: ids this host used on this link
    relayed_refs: HashMap<u64, RelayId>,
    relayed_calls: HashMap<u64, (RelayId, u64)>,
    relayed_subs: HashMap<u64, (RelayId, u64)>,
}

impl Conn {
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn full(&self) -> bool {
        match self.window {
            Some(window) => self.peer_acks == Some(true) && self.unacked_bytes >= window,
            None => false,
        }
    }

    fn label(&self) -> String {
        let name = match self.role {
            Role::Upstream => self.parent_name.clone(),
            Role::Downstream => self.peer_name.clone(),
        };
        name.unwrap_or_else(|| "peer".to_owned())
    }
}

struct RelayCall {
    down_id: Option<u64>,
    member: String,
    args: Option<Value>,
    root: String,
}

struct Relay {
    origin: ConnId,
    origin_id: u64,
    at: String,
    token: Value,
    input: Value,
    principal: Value,
    client: bool,
    generation: u64,
    down: Option<ConnId>,
    down_id: u64,
    closed: bool,
    calls: BTreeMap<u64, RelayCall>,
    subs: BTreeMap<u64, (String, Option<u64>)>,
}

struct RootEntry {
    local: bool,
    running: usize,
    foreign: Option<HashSet<ConnId>>,
    out: Vec<ConnId>,
}

struct Readable {
    sinks: Vec<(u64, mpsc::UnboundedSender<StateUpdate>)>,
    sub: Option<(ConnId, u64)>,
    decoder: Decoder,
    value: Option<Value>,
}

struct QueuedCall {
    key: u64,
    member: String,
    args: Option<Value>,
    reply: CallReply,
}

struct RemoteState {
    contract: Arc<Description>,
    at: String,
    input: Value,
    principal: Option<Value>,
    status: watch::Sender<RefStatus>,
    binding: Option<(ConnId, u64)>,
    queued: Vec<QueuedCall>,
    readables: BTreeMap<String, Readable>,
}

impl RemoteState {
    fn status(&self) -> RefStatus {
        self.status.borrow().clone()
    }

    fn settled(&self) -> bool {
        matches!(self.status(), RefStatus::Released | RefStatus::Gone(_))
    }
}

/// Deferred work: what upstream runs a few promise turns later, after the
/// frame that caused it.
enum Later {
    RelayCallDone {
        origin: ConnId,
        origin_id: u64,
        root: String,
        outcome: Result<Option<Value>, ChordError>,
    },
}

struct Actor {
    name: String,
    kind: Kind,
    principal_key: Option<PrincipalKey>,
    on_error: Option<ErrorHandler>,
    observer: Option<Observer>,
    tx: mpsc::WeakUnboundedSender<Msg>,
    services: HashMap<String, Arc<Service>>,
    objects: HashMap<String, Object>,
    conns: BTreeMap<ConnId, Conn>,
    next_conn: ConnId,
    upstream: Option<ConnId>,
    routes: Vec<(String, Arc<dyn Route>)>,
    relays: HashMap<RelayId, Relay>,
    next_relay: RelayId,
    refs: BTreeMap<RefId, RemoteState>,
    roots: HashMap<String, RootEntry>,
    root_seq: u64,
    incarnation: String,
    acks_due: BTreeSet<ConnId>,
    later: VecDeque<Later>,
    closed: bool,
}

/// Six base-36 digits that keep root ids unique across restarts of a host
/// with the same name, as upstream's `Math.random().toString(36).slice(2, 8)`.
fn incarnation() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or(0),
    );
    let mut seed = hasher.finish();
    let mut text = String::new();
    for _ in 0..6 {
        let digit = (seed % 36) as u32;
        seed /= 36;
        text.push(std::char::from_digit(digit, 36).expect("a base-36 digit"));
    }
    text
}

impl Actor {
    fn new(options: HostOptions, tx: mpsc::WeakUnboundedSender<Msg>) -> Actor {
        Actor {
            name: options.name,
            kind: options.kind,
            principal_key: options.principal_key,
            on_error: options.on_error,
            observer: options.observer,
            tx,
            services: HashMap::new(),
            objects: HashMap::new(),
            conns: BTreeMap::new(),
            next_conn: 1,
            upstream: None,
            routes: Vec::new(),
            relays: HashMap::new(),
            next_relay: 1,
            refs: BTreeMap::new(),
            roots: HashMap::new(),
            root_seq: 0,
            incarnation: incarnation(),
            acks_due: BTreeSet::new(),
            later: VecDeque::new(),
            closed: false,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            self.handle(msg);
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
            // One acknowledgement per burst of processed frames.
            self.flush_acks();
        }
        for conn in self.conns.values() {
            if let Some(pinger) = &conn.pinger {
                pinger.abort();
            }
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Serve(description, factory) => self.serve(description, factory),
            Msg::Accept(link, options) => self.accept(link, options),
            Msg::Connect(link, options) => self.connect(link, options),
            Msg::Route(prefix, route) => self.routes.push((prefix, route)),
            Msg::Inbound(conn, Inbound::Frame(frame)) => self.receive(conn, frame),
            Msg::Inbound(conn, Inbound::Ended(reason)) => self.link_ended(conn, &reason),
            Msg::PingTick(conn) => self.ping_tick(conn),
            Msg::CallDone {
                conn,
                id,
                root,
                outcome,
            } => self.call_done(conn, id, &root, outcome),
            Msg::Cell { conn, sub, change } => self.cell_changed(conn, sub, change),
            Msg::RelayStill { relay, reply } => {
                let _ = reply.send(self.relay_current(relay));
            }
            Msg::RelayResolved { relay, outcome } => self.relay_resolved(relay, outcome),
            Msg::RefResolved { reference, outcome } => self.ref_resolved(reference, outcome),
            Msg::Get {
                id,
                contract,
                at,
                input,
                principal,
                status,
            } => self.get(id, contract, at, input, principal, status),
            Msg::Call {
                reference,
                key,
                member,
                args,
                reply,
            } => self.ref_call(reference, key, member, args, reply),
            Msg::Cancel { reference, key } => self.ref_cancel(reference, key),
            Msg::Subscribe {
                reference,
                member,
                sink,
                sender,
            } => self.ref_subscribe(reference, member, sink, sender),
            Msg::Unsubscribe {
                reference,
                member,
                sink,
            } => self.ref_unsubscribe(reference, &member, sink),
            Msg::Release { reference } => self.ref_release(reference),
            Msg::Invalidate(matching, reply) => {
                self.invalidate_principal(&matching);
                if let Some(reply) = reply {
                    let _ = reply.send(());
                }
            }
            Msg::Close(reply) => {
                let links = self.close();
                let _ = reply.send(links);
            }
        }
        self.run_later();
    }

    fn report(&self, error: ChordError) {
        if let Some(on_error) = &self.on_error {
            on_error(&error);
        }
    }

    fn observe(&self, event: HostEvent) {
        if let Some(observer) = &self.observer {
            observer(&event);
        }
    }

    fn sender(&self) -> Option<mpsc::UnboundedSender<Msg>> {
        self.tx.upgrade()
    }

    // ─── serving ───

    fn serve(&mut self, description: Description, factory: Factory) {
        if description.scope != Scope::Remote {
            self.report(ChordError::invalid(
                "local",
                format!(
                    "{} is local; only remote types are served over links",
                    description.name
                ),
            ));
            return;
        }
        if self.services.contains_key(&description.name) {
            self.report(ChordError::invalid(
                "duplicate",
                format!("{} is already served", description.name),
            ));
            return;
        }
        let fingerprint = description.fingerprint();
        self.services.insert(
            description.name.clone(),
            Arc::new(Service {
                description: Arc::new(description),
                fingerprint,
                factory,
            }),
        );
    }

    // ─── links ───

    fn accept(&mut self, link: Link, options: AcceptOptions) {
        if self.closed {
            link.close("host closed");
            return;
        }
        if options.kind == Kind::Host && options.name.is_none() {
            self.report(ChordError::invalid(
                "accept",
                "accepting a host link needs its name",
            ));
            link.close("protocol error");
            return;
        }
        self.add_connection(
            link,
            Role::Downstream,
            options.kind,
            if options.kind == Kind::Host {
                options.name
            } else {
                None
            },
            options.principal,
            options.restamp,
            options.max_calls,
            options.link,
        );
    }

    fn connect(&mut self, link: Link, options: LinkOptions) {
        if self.closed {
            link.close("host closed");
            return;
        }
        if let Some(previous) = self.upstream {
            self.close_link(previous, "replaced");
        }
        let Some(conn) = self.add_connection(
            link,
            Role::Upstream,
            Kind::Host,
            None,
            Value::Null,
            None,
            None,
            options,
        ) else {
            return;
        };
        self.upstream = Some(conn);
        // Remote references that lost their link hold again over the new one.
        let unbound: Vec<RefId> = self
            .refs
            .iter()
            .filter(|(_, state)| state.binding.is_none() && !state.settled())
            .map(|(id, _)| *id)
            .collect();
        for reference in unbound {
            self.hold(conn, reference);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_connection(
        &mut self,
        link: Link,
        role: Role,
        peer_kind: Kind,
        peer_name: Option<String>,
        principal: Value,
        restamp: Option<Restamp>,
        max_calls: Option<usize>,
        options: LinkOptions,
    ) -> Option<ConnId> {
        let id = self.next_conn;
        self.next_conn += 1;
        let conn = Conn {
            link: link.clone(),
            role,
            peer_kind,
            peer_name,
            principal: principal.clone(),
            restamp,
            generation: 0,
            parent_name: None,
            synced: !(role == Role::Downstream && peer_kind == Kind::Host),
            sync_waiters: Vec::new(),
            next_id: 1,
            last_seen: Instant::now(),
            ping_n: 0,
            ping_timeout: options.ping_timeout,
            pinger: None,
            max_calls,
            window: options.window,
            peer_acks: None,
            sent: 0,
            acked: 0,
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            pending: Vec::new(),
            received: 0,
            mismatched: false,
            remotes: HashMap::new(),
            calls: HashMap::new(),
            subs: HashMap::new(),
            served: BTreeMap::new(),
            incoming: HashMap::new(),
            feeds: HashMap::new(),
            relay_subs: HashMap::new(),
            relayed_refs: HashMap::new(),
            relayed_calls: HashMap::new(),
            relayed_subs: HashMap::new(),
        };
        self.conns.insert(id, conn);
        self.send(
            id,
            Frame::Hello {
                protocol: Some(PROTOCOL_VERSION),
                name: self.name.clone(),
                kind: self.kind,
                code_version: CODE_VERSION,
                acks: Some(true),
            },
        );
        if link.is_closed() {
            // A link that was already closed would route to nowhere forever.
            self.conns.remove(&id);
            return None;
        }
        let tx = self.tx.clone();
        link.attach(move |inbound| {
            if let Some(tx) = tx.upgrade() {
                let _ = tx.send(Msg::Inbound(id, inbound));
            }
        });
        let tx = self.tx.clone();
        let interval = options.ping_interval;
        let pinger = tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(Instant::now() + interval, interval);
            loop {
                ticker.tick().await;
                let Some(tx) = tx.upgrade() else { return };
                if tx.send(Msg::PingTick(id)).is_err() {
                    return;
                }
            }
        });
        if let Some(conn) = self.conns.get_mut(&id) {
            conn.pinger = Some(pinger);
        }
        self.observe(HostEvent::LinkOpened {
            link: id,
            kind: peer_kind,
            principal: (role == Role::Downstream).then_some(principal),
        });
        Some(id)
    }

    /// Sends a frame, counting it for flow control.
    fn send(&mut self, conn: ConnId, frame: Frame) {
        let Some(conn) = self.conns.get_mut(&conn) else {
            return;
        };
        let size = conn.link.send(&frame).unwrap_or(0);
        if conn.peer_acks == Some(false) {
            return;
        }
        conn.sent += 1;
        conn.unacked.push_back(size);
        conn.unacked_bytes += size;
    }

    /// Sends a state frame, or keeps it waiting. False: the slot is full;
    /// send a reset.
    fn send_state(&mut self, conn_id: ConnId, frame: StateFrame) -> bool {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return true;
        };
        let index = conn.pending.iter().position(|(id, _)| *id == frame.id);
        if index.is_none() && !conn.full() {
            self.send(conn_id, Frame::State(frame));
            return true;
        }
        let index = match index {
            Some(index) => index,
            None => {
                conn.pending.push((frame.id, PendingState::default()));
                conn.pending.len() - 1
            }
        };
        let window = conn.window.unwrap_or(usize::MAX);
        let slot = &mut conn.pending[index].1;
        let Some(ops) = frame.ops else {
            slot.base = Some(frame);
            slot.ops.clear();
            slot.size = 0;
            return true;
        };
        let size = crate::wire::text_size(&serde_json::to_string(&ops).expect("ops are JSON"));
        if slot.size.saturating_add(size) > window.saturating_mul(PENDING_WINDOWS) {
            return false;
        }
        slot.ops.extend(ops);
        slot.size += size;
        true
    }

    fn drop_state(&mut self, conn: ConnId, id: u64) {
        if let Some(conn) = self.conns.get_mut(&conn) {
            conn.pending.retain(|(pending, _)| *pending != id);
        }
    }

    fn on_ack(&mut self, conn_id: ConnId, n: u64) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if conn.peer_acks != Some(true) || n < conn.acked || n > conn.sent {
            let reason = format!("ack {n} outside {}..{}", conn.acked, conn.sent);
            self.violation(conn_id, &reason);
            return;
        }
        for _ in 0..(n - conn.acked) {
            if let Some(size) = conn.unacked.pop_front() {
                conn.unacked_bytes -= size;
            }
        }
        conn.acked = n;
        loop {
            let Some(conn) = self.conns.get_mut(&conn_id) else {
                return;
            };
            if conn.pending.is_empty() || conn.full() {
                return;
            }
            let (id, slot) = conn.pending.remove(0);
            if let Some(base) = slot.base {
                self.send(conn_id, Frame::State(base));
            }
            if !slot.ops.is_empty() {
                self.send(
                    conn_id,
                    Frame::State(StateFrame {
                        id,
                        ops: Some(slot.ops),
                        ..StateFrame::default()
                    }),
                );
            }
        }
    }

    fn flush_acks(&mut self) {
        for conn in std::mem::take(&mut self.acks_due) {
            if let Some(conn) = self.conns.get(&conn) {
                if conn.peer_acks == Some(true) && !conn.link.is_closed() {
                    // Not counted: acknowledgements are outside flow control.
                    conn.link.send(&Frame::Ack { n: conn.received });
                }
            }
        }
    }

    fn ping_tick(&mut self, conn_id: ConnId) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if Instant::now().duration_since(conn.last_seen) > conn.ping_timeout {
            self.close_link(conn_id, "ping timeout");
            return;
        }
        conn.ping_n += 1;
        let n = conn.ping_n;
        self.send(conn_id, Frame::Ping { n });
    }

    /// Closes a link from this side and handles the closure at once.
    fn close_link(&mut self, conn: ConnId, reason: &str) {
        let Some(link) = self.conns.get(&conn).map(|conn| conn.link.clone()) else {
            return;
        };
        if link.is_closed() {
            return;
        }
        link.close(reason);
        self.closed_conn(conn, reason);
    }

    fn link_ended(&mut self, conn: ConnId, reason: &str) {
        if self.conns.contains_key(&conn) {
            self.closed_conn(conn, reason);
        }
    }

    /// A protocol violation: nothing on this link can be trusted any more.
    fn violation(&mut self, conn: ConnId, reason: &str) {
        let label = self.conns.get(&conn).map(Conn::label).unwrap_or_default();
        self.report(ChordError::invalid(
            "protocol",
            format!("link to {label}: {reason}"),
        ));
        self.close_link(conn, &format!("protocol error: {reason}"));
    }

    fn protocol_mismatch(&mut self, conn_id: ConnId, peer: u64, close: bool) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if conn.mismatched {
            return;
        }
        conn.mismatched = true;
        self.report(ChordError::unavailable(format!(
            "the peer speaks link protocol {peer}, this host {PROTOCOL_VERSION}"
        )));
        if close {
            self.close_link(conn_id, &format!("protocol-version {PROTOCOL_VERSION}"));
        }
    }

    // ─── receiving ───

    fn receive(&mut self, conn_id: ConnId, value: Value) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if conn.mismatched || conn.link.is_closed() {
            return;
        }
        conn.last_seen = Instant::now();
        let frame = match Frame::parse(&value) {
            Ok(frame) => frame,
            Err(_) => {
                self.violation(conn_id, "malformed frame");
                return;
            }
        };
        if let Frame::Ack { n } = frame {
            self.on_ack(conn_id, n);
            return;
        }
        conn.received += 1;
        self.dispatch(conn_id, frame);
        if let Some(conn) = self.conns.get(&conn_id) {
            if conn.peer_acks == Some(true) && !conn.link.is_closed() {
                self.acks_due.insert(conn_id);
            }
        }
    }

    fn dispatch(&mut self, conn: ConnId, frame: Frame) {
        match frame {
            Frame::Hello {
                protocol,
                name,
                kind,
                code_version,
                acks,
            } => self.on_hello(conn, protocol, name, kind, code_version, acks),
            Frame::Ping { n } => self.send(conn, Frame::Pong { n }),
            Frame::Pong { .. } | Frame::Bye { .. } | Frame::Ack { .. } => {}
            Frame::Hold {
                id,
                at,
                token,
                input,
                principal,
                client,
            } => self.on_hold(conn, id, at, token.to_value(), input, principal, client),
            Frame::Release { id } => self.drop_served(conn, id, true),
            Frame::Call {
                id,
                hold,
                member,
                args,
                root,
            } => self.on_call(conn, id, hold, member, args, root),
            Frame::Cancel { id } => self.on_cancel(conn, id),
            Frame::Sub { id, hold, member } => self.on_sub(conn, id, hold, member),
            Frame::Unsub { id } => self.on_unsub(conn, id),
            Frame::Shape {
                id,
                variant,
                members,
                contract,
                code_version,
            } => self.on_shape(
                conn,
                id,
                Shape {
                    variant,
                    members,
                    contract,
                    code_version,
                },
            ),
            Frame::Gone { id, error } => self.on_gone(conn, id, ChordError::from_wire(&error)),
            Frame::Result { id, value, error } => self.on_result(
                conn,
                id,
                value,
                error.map(|error| ChordError::from_wire(&error)),
            ),
            Frame::State(state) => self.on_state(conn, state),
            Frame::RootEnd { root_id } => self.foreign_root_ended(&root_id, conn),
            Frame::Invalidate { .. }
            | Frame::Prepare { .. }
            | Frame::Commit { .. }
            | Frame::Abort { .. }
            | Frame::Retire { .. } => {
                // Control frames only from the parent.
                if self.conns.get(&conn).map(|conn| conn.role) != Some(Role::Upstream) {
                    let reason = format!("{} from a child", frame.kind());
                    self.violation(conn, &reason);
                    return;
                }
                match frame {
                    Frame::Invalidate { pattern } => {
                        self.invalidate_principal(&PrincipalMatch::Pattern(pattern))
                    }
                    Frame::Prepare { reload, .. } => {
                        // noel does not reload: it votes no.
                        let error =
                            ChordError::unavailable(format!("{} does not reload", self.name))
                                .with_reason("not_coordinator");
                        self.send(
                            conn,
                            Frame::Vote {
                                reload,
                                error: Some(error.to_wire()),
                                report: None,
                            },
                        );
                    }
                    _ => {}
                }
            }
            Frame::Vote { .. } => {
                let participant = self.conns.get(&conn).is_some_and(|conn| {
                    conn.role == Role::Downstream && conn.peer_kind == Kind::Host
                });
                if !participant {
                    self.violation(conn, "vote from a non-participant");
                }
                // noel never sends `prepare`, so there is no vote to settle.
            }
        }
    }

    fn on_hello(
        &mut self,
        conn_id: ConnId,
        protocol: Option<u64>,
        name: String,
        kind: Kind,
        code_version: u64,
        acks: Option<bool>,
    ) {
        let protocol = protocol.unwrap_or(1);
        if protocol != PROTOCOL_VERSION {
            self.protocol_mismatch(conn_id, protocol, true);
            return;
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if conn.peer_acks.is_none() {
            let acks = acks == Some(true);
            conn.peer_acks = Some(acks);
            if !acks {
                conn.unacked.clear();
                conn.unacked_bytes = 0;
            }
        }
        if conn.role == Role::Upstream {
            if kind != Kind::Host {
                self.violation(conn_id, "parent is not a host");
                return;
            }
            conn.parent_name.get_or_insert(name);
            return;
        }
        if kind != conn.peer_kind
            || conn
                .peer_name
                .as_ref()
                .is_some_and(|expected| *expected != name)
        {
            let reason = format!(
                "hello {} {} does not match the accepted {} {}",
                kind.as_str(),
                name,
                conn.peer_kind.as_str(),
                conn.peer_name.clone().unwrap_or_default()
            );
            self.violation(conn_id, &reason);
            return;
        }
        if conn.peer_kind == Kind::Host {
            self.peer_hello(conn_id, code_version);
        }
    }

    /// A child host announced its code version. noel has one: a child in
    /// step is usable; any other cannot be reconciled.
    fn peer_hello(&mut self, conn_id: ConnId, code_version: u64) {
        if code_version > CODE_VERSION {
            let label = self
                .conns
                .get(&conn_id)
                .map(Conn::label)
                .unwrap_or_default();
            self.report(ChordError::invalid(
                "protocol",
                format!("{label} runs code version {code_version}, ahead of {CODE_VERSION}"),
            ));
            self.close_link(conn_id, "code version ahead of parent");
            return;
        }
        if code_version < CODE_VERSION {
            self.close_link(conn_id, "reconciliation failed");
            return;
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        conn.synced = true;
        let waiters = std::mem::take(&mut conn.sync_waiters);
        for waiter in waiters {
            match waiter {
                SyncWaiter::Relay(relay) => {
                    if self.relay_current(relay) {
                        self.relay_open(relay, conn_id);
                    } else {
                        self.relay_close(relay, revoked(), true, true);
                    }
                }
                SyncWaiter::Ref(reference) => {
                    let unbound = self.refs.get(&reference).is_some_and(|state| {
                        state.binding.is_none() && state.status() == RefStatus::Loading
                    });
                    if unbound {
                        self.hold(conn_id, reference);
                    }
                }
            }
        }
    }

    /// Principal stamping: a downstream link acts as the principal it was
    /// accepted with, whatever the frame says. Only the parent forwards one;
    /// its own requests act as `{host: parentName}`.
    fn principal_of(&self, conn: &Conn, forwarded: Option<Value>) -> Value {
        match conn.role {
            Role::Downstream => conn.principal.clone(),
            Role::Upstream => forwarded.unwrap_or_else(|| {
                json!({ "host": conn.parent_name.clone().unwrap_or_else(|| "parent".to_owned()) })
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_hold(
        &mut self,
        conn_id: ConnId,
        id: u64,
        at: String,
        token: Value,
        input: Value,
        principal: Option<Value>,
        client: Option<bool>,
    ) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        if conn.served.contains_key(&id) {
            self.violation(conn_id, &format!("duplicate hold id {id}"));
            return;
        }
        let principal = self.principal_of(conn, principal);
        let is_client = match conn.role {
            Role::Downstream => conn.peer_kind == Kind::Client,
            Role::Upstream => client == Some(true),
        };
        if at != self.name {
            if conn.role == Role::Upstream {
                let error = ChordError::unavailable(format!("{} does not relay", self.name));
                self.send(
                    conn_id,
                    Frame::Gone {
                        id,
                        error: error.to_wire(),
                    },
                );
                return;
            }
            let relay_id = self.next_relay;
            self.next_relay += 1;
            let generation = conn.generation;
            self.relays.insert(
                relay_id,
                Relay {
                    origin: conn_id,
                    origin_id: id,
                    at,
                    token,
                    input,
                    principal,
                    client: is_client,
                    generation,
                    down: None,
                    down_id: 0,
                    closed: false,
                    calls: BTreeMap::new(),
                    subs: BTreeMap::new(),
                },
            );
            if let Some(conn) = self.conns.get_mut(&conn_id) {
                conn.served.insert(id, Served::Relay(relay_id));
            }
            self.relay(relay_id);
            return;
        }
        match self.acquire(&token, &input, &principal) {
            Ok((address, consumer)) => {
                let object = self.objects.get_mut(&address).expect("acquired");
                object.holders.push((conn_id, id));
                let shape = shape_of(object);
                let event = HostEvent::Held {
                    link: conn_id,
                    id,
                    type_name: object.type_name.clone(),
                    input: object.input.clone(),
                    principal: object.principal.clone(),
                };
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.served.insert(
                        id,
                        Served::Local(LocalHold {
                            object: address,
                            principal,
                            feeds: BTreeSet::new(),
                            consumer,
                        }),
                    );
                }
                self.observe(event);
                self.send(
                    conn_id,
                    Frame::Shape {
                        id,
                        variant: shape.variant,
                        members: shape.members,
                        contract: shape.contract,
                        code_version: shape.code_version,
                    },
                );
            }
            Err(error) => {
                let error = if is_client {
                    client_error(error)
                } else {
                    error
                };
                self.send(
                    conn_id,
                    Frame::Gone {
                        id,
                        error: error.to_wire(),
                    },
                );
            }
        }
    }

    /// Binds the consumer's token to the served type and finds or makes the
    /// object for `(type, input, principal)`.
    fn acquire(
        &mut self,
        token: &Value,
        input: &Value,
        principal: &Value,
    ) -> Result<(String, Arc<Description>), ChordError> {
        let malformed = || ChordError::invalid("token", "malformed contract token");
        let consumer: Description =
            serde_json::from_value(token.clone()).map_err(|_| malformed())?;
        if consumer.scope != Scope::Remote {
            return Err(ChordError::invalid(
                "token",
                format!("{} is local; only remote types cross links", consumer.name),
            ));
        }
        let service = self.services.get(&consumer.name).cloned().ok_or_else(|| {
            ChordError::unavailable(format!("{} is not served by {}", consumer.name, self.name))
                .with_reason("unknown_type")
        })?;
        bind(&service, &consumer, &fingerprint(token))?;
        let description = &service.description;
        let normalized = validate::normalize(
            &description.input,
            input,
            &format!("{} input", description.name),
        )?;
        let identity = validate::canonical_identity(&description.input, &normalized);
        let key = match &self.principal_key {
            Some(key) => key(principal),
            None => canonical_json(principal),
        };
        let address = format!("{}\u{0}{}\u{0}p:{}", description.name, identity, key);
        if !self.objects.contains_key(&address) {
            let implementation = (service.factory)(&normalized, principal)?;
            check_implementation(description, &implementation)?;
            self.objects.insert(
                address.clone(),
                Object {
                    type_name: description.name.clone(),
                    input: normalized,
                    principal: principal.clone(),
                    service: service.clone(),
                    variant: implementation.variant,
                    members: implementation.members,
                    holders: Vec::new(),
                    on_dispose: implementation.on_dispose,
                },
            );
        }
        Ok((address, Arc::new(consumer)))
    }

    /// Forgets a served hold: unsubscribes its feeds and releases it (locally
    /// or downstream).
    fn drop_served(&mut self, conn_id: ConnId, id: u64, release: bool) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        let Some(served) = conn.served.remove(&id) else {
            return;
        };
        match served {
            Served::Relay(relay) => {
                let error = ChordError::new(ErrorCode::Released, "relayed hold released");
                self.relay_close(relay, error, release, false);
            }
            Served::Local(hold) => {
                for feed in hold.feeds {
                    self.on_unsub(conn_id, feed);
                }
                if let Some(object) = self.objects.get(&hold.object) {
                    self.observe(HostEvent::Released {
                        link: conn_id,
                        id,
                        type_name: object.type_name.clone(),
                        input: object.input.clone(),
                        principal: object.principal.clone(),
                    });
                }
                if release {
                    self.release_object(&hold.object, conn_id, id);
                }
            }
        }
    }

    fn release_object(&mut self, address: &str, conn: ConnId, id: u64) {
        let Some(object) = self.objects.get_mut(address) else {
            return;
        };
        object.holders.retain(|holder| *holder != (conn, id));
        if object.holders.is_empty() {
            if let Some(object) = self.objects.remove(address) {
                if let Some(dispose) = object.on_dispose {
                    dispose();
                }
            }
        }
    }

    /// Force-disposes an object: its holders hear `gone`.
    fn dispose_object(&mut self, address: &str, error: &ChordError) {
        let Some(object) = self.objects.remove(address) else {
            return;
        };
        for (conn_id, id) in &object.holders {
            let ours = self
                .conns
                .get(conn_id)
                .and_then(|conn| conn.served.get(id))
                .is_some_and(
                    |served| matches!(served, Served::Local(hold) if hold.object == address),
                );
            if !ours {
                continue;
            }
            self.drop_served(*conn_id, *id, false);
            self.send(
                *conn_id,
                Frame::Gone {
                    id: *id,
                    error: error.to_wire(),
                },
            );
        }
        if let Some(dispose) = object.on_dispose {
            dispose();
        }
    }

    fn on_call(
        &mut self,
        conn_id: ConnId,
        id: u64,
        hold: u64,
        member: String,
        args: Option<Value>,
        root: Option<RootRef>,
    ) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        if conn.incoming.contains_key(&id) {
            self.violation(conn_id, &format!("duplicate call id {id}"));
            return;
        }
        if let Some(max) = conn.max_calls {
            if conn.incoming.len() >= max {
                let error = ChordError::unavailable(format!(
                    "Too many calls in progress on this connection ({max}); wait for earlier ones to finish"
                ))
                .with_reason("too_many_calls");
                self.send(
                    conn_id,
                    Frame::Result {
                        id,
                        value: None,
                        error: Some(error.to_wire()),
                    },
                );
                return;
            }
        }
        let served = match conn.served.get(&hold) {
            None => {
                let error = ChordError::gone("unknown hold");
                self.send(
                    conn_id,
                    Frame::Result {
                        id,
                        value: None,
                        error: Some(error.to_wire()),
                    },
                );
                return;
            }
            Some(Served::Relay(relay)) => Err(*relay),
            Some(Served::Local(local)) => Ok((
                local.object.clone(),
                local.principal.clone(),
                local.consumer.clone(),
            )),
        };
        // Roots are honoured from hosts only; a request without one starts one here.
        let carried = if conn.peer_kind == Kind::Host {
            root
        } else {
            None
        };
        let root_id = match carried {
            Some(root) => {
                if root.code_version != CODE_VERSION {
                    let error = ChordError::new(
                        ErrorCode::Superseded,
                        format!(
                            "code version {} is not retained by {}",
                            root.code_version, self.name
                        ),
                    );
                    self.send(
                        conn_id,
                        Frame::Result {
                            id,
                            value: None,
                            error: Some(error.to_wire()),
                        },
                    );
                    return;
                }
                self.root_join_foreign(&root.root_id, conn_id);
                root.root_id
            }
            None => self.root_start(),
        };
        match served {
            Ok((address, principal, consumer)) => {
                let plan = self.plan_call(&address, &consumer, &member, args);
                let (method, input, returns, result_schema) = match plan {
                    Ok(plan) => plan,
                    Err(error) => {
                        self.root_settle(&root_id);
                        self.send(
                            conn_id,
                            Frame::Result {
                                id,
                                value: None,
                                error: Some(error.to_wire()),
                            },
                        );
                        return;
                    }
                };
                let signal = AbortSignal::new();
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.incoming.insert(id, Incoming::Local(signal.clone()));
                }
                let ctx = CallContext { principal, signal };
                let what = format!("{address}.{member} result");
                let future = method(input, ctx);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let outcome = match std::panic::AssertUnwindSafe(future).catch_unwind().await {
                        Ok(Ok(value)) => {
                            if !returns {
                                Ok(None)
                            } else {
                                match &result_schema {
                                    Some(schema) => {
                                        validate::assert_valid("result", &what, schema, &value)
                                            .map(|()| Some(value))
                                    }
                                    None => Ok(Some(value)),
                                }
                            }
                        }
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(ChordError::unavailable(format!(
                            "{what}: the method panicked"
                        ))),
                    };
                    if let Some(tx) = tx.upgrade() {
                        let _ = tx.send(Msg::CallDone {
                            conn: conn_id,
                            id,
                            root: root_id,
                            outcome,
                        });
                    }
                });
            }
            Err(relay) => {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.incoming.insert(id, Incoming::Relay(relay));
                }
                self.relay_call(relay, id, member, args, root_id);
            }
        }
    }

    /// Checks a call against the variant, the consumer's and the provider's
    /// contracts, and validates its arguments.
    #[allow(clippy::type_complexity)]
    fn plan_call(
        &self,
        address: &str,
        consumer: &Description,
        member: &str,
        args: Option<Value>,
    ) -> Result<(crate::object::MethodFn, Option<Value>, bool, Option<Value>), ChordError> {
        let object = self
            .objects
            .get(address)
            .ok_or_else(|| ChordError::gone(format!("{address} is disposed")))?;
        let variant = object.variant.clone();
        let variant_name = variant.clone().unwrap_or_default();
        let wanted = consumer
            .variants
            .get(&variant_name)
            .and_then(|members| members.get(member));
        let method = match (object.member(member), wanted) {
            (Some(Member::Method(method)), Some(MemberDescription::Method { .. })) => {
                method.clone()
            }
            _ => {
                return Err(ChordError::new(
                    ErrorCode::NotInVariant,
                    format!("{address}: {member} is not a method of variant {variant_name}"),
                ))
            }
        };
        let provided = object
            .service
            .description
            .variants
            .get(&variant_name)
            .and_then(|members| members.get(member));
        let (args_schema, result_schema, takes_args, returns) = match provided {
            Some(MemberDescription::Method { args, result }) => (
                args.clone(),
                result.clone(),
                args.is_some(),
                result.is_some(),
            ),
            _ => (None, None, true, true),
        };
        let input = if takes_args {
            // The consumer sends only what its own schema knows, and the
            // provider sees only what its schema knows.
            let own = match wanted {
                Some(MemberDescription::Method {
                    args: Some(own), ..
                }) => Some(own),
                _ => None,
            };
            let sent = args.unwrap_or(Value::Null);
            let sent = match own {
                Some(own) => validate::clean(own, sent),
                None => sent,
            };
            let input = match &args_schema {
                Some(schema) => validate::clean(schema, sent),
                None => sent,
            };
            if let Some(schema) = &args_schema {
                validate::assert_valid(
                    "args",
                    &format!("{address}.{member} args"),
                    schema,
                    &input,
                )?;
            }
            Some(input)
        } else {
            None
        };
        Ok((method, input, returns, result_schema))
    }

    fn call_done(
        &mut self,
        conn_id: ConnId,
        id: u64,
        root: &str,
        outcome: Result<Option<Value>, ChordError>,
    ) {
        self.root_settle(root);
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        conn.incoming.remove(&id);
        let frame = match outcome {
            Ok(value) => Frame::Result {
                id,
                value,
                error: None,
            },
            Err(error) => Frame::Result {
                id,
                value: None,
                error: Some(error.to_wire()),
            },
        };
        self.send(conn_id, frame);
    }

    fn on_cancel(&mut self, conn_id: ConnId, id: u64) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        match conn.incoming.get(&id) {
            Some(Incoming::Local(signal)) => {
                signal.abort(ChordError::aborted("cancelled by caller"))
            }
            Some(Incoming::Relay(relay)) => {
                let relay = *relay;
                self.relay_call_cancel(relay, id, ChordError::aborted("call cancelled"), true);
            }
            None => {}
        }
    }

    fn on_sub(&mut self, conn_id: ConnId, id: u64, hold: u64, member: String) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        if conn.feeds.contains_key(&id) || conn.relay_subs.contains_key(&id) {
            self.violation(conn_id, &format!("duplicate sub id {id}"));
            return;
        }
        let (address, consumer) = match conn.served.get(&hold) {
            None => {
                let error = ChordError::gone("unknown hold");
                self.send_state(
                    conn_id,
                    StateFrame {
                        id,
                        error: Some(error.to_wire()),
                        ..StateFrame::default()
                    },
                );
                return;
            }
            Some(Served::Relay(relay)) => {
                let relay = *relay;
                self.relay_subscribe(relay, id, member);
                return;
            }
            Some(Served::Local(local)) => (local.object.clone(), local.consumer.clone()),
        };
        let Some(object) = self.objects.get(&address) else {
            return;
        };
        let declared = consumer
            .variants
            .iter()
            .find_map(|(_, members)| members.get(&member))
            .cloned();
        let implemented = object.member(&member).cloned();
        let readable = match &declared {
            Some(MemberDescription::State { .. }) => true,
            None => matches!(implemented, Some(Member::State(_))),
            Some(_) => false,
        };
        if !readable {
            let error = ChordError::new(ErrorCode::NotInVariant, format!("{member} is not state"));
            self.send_state(
                conn_id,
                StateFrame {
                    id,
                    error: Some(error.to_wire()),
                    ..StateFrame::default()
                },
            );
            return;
        }
        // Only a member of the consumer's view of the current variant has a source.
        let variant = object.variant.clone().unwrap_or_default();
        let in_variant = consumer
            .variants
            .get(&variant)
            .is_some_and(|members| members.contains_key(&member));
        let cell = match implemented {
            Some(Member::State(cell)) if in_variant => Some(cell),
            _ => None,
        };
        let schema = object
            .service
            .description
            .variants
            .get(&variant)
            .and_then(|members| members.get(&member))
            .and_then(|description| match description {
                MemberDescription::State { schema } => Some(schema.clone()),
                _ => None,
            });
        let what = format!("{address}.{member}");
        let mut feed = Feed {
            object: address,
            member: member.clone(),
            cell: None,
            encoder: Encoder::new(),
        };
        let mut snapshot = None;
        if let Some(cell) = cell {
            let tx = self.tx.clone();
            let listener = cell.listen(Arc::new(move |change: &CellChange| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Msg::Cell {
                        conn: conn_id,
                        sub: id,
                        change: change.clone(),
                    });
                }
            }));
            snapshot = Some(listener.1);
            feed.cell = Some((cell, listener.0));
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        conn.feeds.insert(id, feed);
        if let Some(Served::Local(local)) = conn.served.get_mut(&hold) {
            local.feeds.insert(id);
        }
        if let Some(value) = snapshot {
            // Snapshots are validated once per (re)subscription.
            let frame = match schema
                .as_ref()
                .map(|schema| validate::assert_valid("state", &what, schema, &value))
            {
                Some(Err(error)) => StateFrame {
                    id,
                    error: Some(error.to_wire()),
                    ..StateFrame::default()
                },
                _ => StateFrame {
                    id,
                    snap: Some((*value).clone()),
                    ..StateFrame::default()
                },
            };
            self.send_state(conn_id, frame);
        }
    }

    /// A cell a feed follows changed.
    fn cell_changed(&mut self, conn_id: ConnId, sub: u64, change: CellChange) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        let Some(feed) = conn.feeds.get_mut(&sub) else {
            return;
        };
        let encoded = feed.encoder.encode(&change.ops);
        if self.send_state(
            conn_id,
            StateFrame {
                id: sub,
                ops: Some(encoded),
                ..StateFrame::default()
            },
        ) {
            return;
        }
        // Too many ops wait for a slow peer: the current value replaces them.
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        let Some(feed) = conn.feeds.get_mut(&sub) else {
            return;
        };
        feed.encoder = Encoder::new();
        let schema = self.objects.get(&feed.object).and_then(|object| {
            object
                .service
                .description
                .variants
                .get(&object.variant.clone().unwrap_or_default())
                .and_then(|members| members.get(&feed.member))
                .and_then(|description| match description {
                    MemberDescription::State { schema } => Some(schema.clone()),
                    _ => None,
                })
        });
        let frame = match schema
            .map(|schema| validate::assert_valid("state", &feed.member, &schema, &change.value))
        {
            Some(Err(error)) => StateFrame {
                id: sub,
                error: Some(error.to_wire()),
                ..StateFrame::default()
            },
            _ => StateFrame {
                id: sub,
                reset: Some((*change.value).clone()),
                ..StateFrame::default()
            },
        };
        self.send_state(conn_id, frame);
    }

    fn on_unsub(&mut self, conn_id: ConnId, id: u64) {
        self.drop_state(conn_id, id);
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if let Some(feed) = conn.feeds.remove(&id) {
            if let Some((cell, listener)) = feed.cell {
                cell.unlisten(listener);
            }
        }
        if let Some(relay) = conn.relay_subs.get(&id).copied() {
            self.relay_unsubscribe(relay, id, true);
        }
    }

    // ─── replies to what this host asked ───

    fn on_shape(&mut self, conn_id: ConnId, id: u64, shape: Shape) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        if let Some(relay) = conn.relayed_refs.get(&id).copied() {
            let Some(state) = self.relays.get(&relay) else {
                return;
            };
            if !state.closed {
                let (origin, origin_id) = (state.origin, state.origin_id);
                self.send(
                    origin,
                    Frame::Shape {
                        id: origin_id,
                        variant: shape.variant,
                        members: shape.members,
                        contract: shape.contract,
                        code_version: shape.code_version,
                    },
                );
            }
            return;
        }
        let Some(reference) = conn.remotes.get(&id).copied() else {
            return;
        };
        self.ref_shaped(reference, shape);
    }

    fn on_gone(&mut self, conn_id: ConnId, id: u64, error: ChordError) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if let Some(relay) = conn.relayed_refs.get(&id).copied() {
            self.relay_close(relay, error, false, true);
            return;
        }
        let Some(reference) = conn.remotes.remove(&id) else {
            return;
        };
        self.ref_gone(reference, error);
    }

    fn on_result(
        &mut self,
        conn_id: ConnId,
        id: u64,
        value: Option<Value>,
        error: Option<ChordError>,
    ) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if let Some((relay, origin_id)) = conn.relayed_calls.get(&id).copied() {
            let outcome = match error {
                Some(error) => Err(error),
                None => Ok(value),
            };
            self.relay_call_finish(relay, origin_id, outcome);
            return;
        }
        let Some(call) = conn.calls.remove(&id) else {
            return;
        };
        if let Some(root) = &call.root {
            self.root_settle(root);
        }
        let _ = call.reply.send(match error {
            Some(error) => Err(error),
            None => Ok(value),
        });
    }

    fn on_state(&mut self, conn_id: ConnId, frame: StateFrame) {
        let Some(conn) = self.conns.get(&conn_id) else {
            return;
        };
        if let Some((relay, origin_sub)) = conn.relayed_subs.get(&frame.id).copied() {
            let Some(origin) = self.relays.get(&relay).map(|relay| relay.origin) else {
                return;
            };
            if !self.send_state(
                origin,
                StateFrame {
                    id: origin_sub,
                    ..frame
                },
            ) {
                self.relay_resync(relay, origin_sub);
            }
            return;
        }
        let Some((reference, member)) = conn.subs.get(&frame.id).cloned() else {
            return;
        };
        self.readable_receive(reference, &member, frame);
    }

    // ─── lifetime ───

    fn closed_conn(&mut self, conn_id: ConnId, reason: &str) {
        if let Some(told) = reason.strip_prefix("protocol-version ") {
            if let Ok(version) = told.parse::<u64>() {
                if version != PROTOCOL_VERSION {
                    self.protocol_mismatch(conn_id, version, false);
                }
            }
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        if let Some(pinger) = conn.pinger.take() {
            pinger.abort();
        }
        conn.pending.clear();
        let error = ChordError::unavailable(format!("link to {} closed: {reason}", conn.label()));
        // As origin: everything the peer held here, relayed or not, is released.
        let served: Vec<u64> = conn.served.keys().copied().collect();
        for id in served {
            self.drop_served(conn_id, id, true);
        }
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        for (_, incoming) in conn.incoming.drain() {
            if let Incoming::Local(signal) = incoming {
                signal.abort(error.clone());
            }
        }
        let calls: Vec<OutCall> = conn.calls.drain().map(|(_, call)| call).collect();
        conn.subs.clear();
        let waiters = std::mem::take(&mut conn.sync_waiters);
        let relayed: Vec<RelayId> = conn.relayed_refs.values().copied().collect();
        let remotes: Vec<RefId> = conn
            .remotes
            .drain()
            .map(|(_, reference)| reference)
            .collect();
        let role = conn.role;
        for call in calls {
            if let Some(root) = &call.root {
                self.root_settle(root);
            }
            let _ = call.reply.send(Err(error.clone()));
        }
        for waiter in waiters {
            match waiter {
                SyncWaiter::Relay(relay) => self.relay_close(relay, error.clone(), true, true),
                SyncWaiter::Ref(reference) => self.ref_gone(reference, error.clone()),
            }
        }
        // As downstream of relays: their origins lose the hold.
        for relay in relayed {
            self.relay_close(relay, error.clone(), false, true);
        }
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            conn.relayed_refs.clear();
            conn.relayed_calls.clear();
            conn.relayed_subs.clear();
        }
        for reference in remotes {
            // An upstream link may be replaced; a downstream host that went away is gone.
            match role {
                Role::Upstream => self.ref_detached(reference),
                Role::Downstream => self.ref_gone(reference, error.clone()),
            }
        }
        // Foreign roots that came over this link will never announce their end over it.
        let foreign: Vec<String> = self
            .roots
            .iter()
            .filter(|(_, entry)| {
                entry
                    .foreign
                    .as_ref()
                    .is_some_and(|links| links.contains(&conn_id))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for root in foreign {
            self.foreign_root_ended(&root, conn_id);
        }
        for entry in self.roots.values_mut() {
            entry.out.retain(|out| *out != conn_id);
        }
        if self.upstream == Some(conn_id) {
            self.upstream = None;
        }
        self.conns.remove(&conn_id);
        self.acks_due.remove(&conn_id);
        self.observe(HostEvent::LinkClosed {
            link: conn_id,
            reason: reason.to_owned(),
        });
    }

    fn invalidate_principal(&mut self, matching: &PrincipalMatch) {
        let reason = ChordError::gone("principal invalidated");
        // Links acting as a matching principal are re-stamped or closed.
        let links: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, conn)| conn.role == Role::Downstream && matching.matches(&conn.principal))
            .map(|(id, _)| *id)
            .collect();
        for conn in links {
            let next = self
                .conns
                .get(&conn)
                .and_then(|conn| {
                    conn.restamp
                        .as_ref()
                        .map(|restamp| restamp(&conn.principal))
                })
                .flatten();
            self.revoke(conn, next);
        }
        // Patterns propagate to child hosts.
        if let PrincipalMatch::Pattern(pattern) = matching {
            let children: Vec<ConnId> = self
                .conns
                .iter()
                .filter(|(_, conn)| conn.role == Role::Downstream && conn.peer_kind == Kind::Host)
                .map(|(id, _)| *id)
                .collect();
            for child in children {
                self.send(
                    child,
                    Frame::Invalidate {
                        pattern: pattern.clone(),
                    },
                );
            }
        }
        let mut matching_objects: Vec<String> = self
            .objects
            .iter()
            .filter(|(_, object)| matching.matches(&object.principal))
            .map(|(address, _)| address.clone())
            .collect();
        matching_objects.sort();
        for address in matching_objects {
            self.dispose_object(&address, &reason);
        }
    }

    /// Revokes a link's stamped principal: relays made under it close at
    /// once, also those still resolving a route.
    fn revoke(&mut self, conn_id: ConnId, next: Option<Value>) {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        conn.generation += 1;
        let relays: Vec<RelayId> = conn
            .served
            .values()
            .filter_map(|served| match served {
                Served::Relay(relay) => Some(*relay),
                Served::Local(_) => None,
            })
            .collect();
        for relay in relays {
            self.relay_close(relay, ChordError::gone("principal invalidated"), true, true);
        }
        match next {
            None => self.close_link(conn_id, "principal invalidated"),
            Some(principal) => {
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.principal = principal;
                }
            }
        }
    }

    /// Returns the links it closed.
    fn close(&mut self) -> Vec<Link> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let reason = ChordError::gone(format!("host {} closed", self.name));
        let mut addresses: Vec<String> = self.objects.keys().cloned().collect();
        addresses.sort();
        for address in addresses {
            self.dispose_object(&address, &reason);
        }
        self.run_later();
        let conns: Vec<ConnId> = self.conns.keys().copied().collect();
        let links = conns
            .iter()
            .filter_map(|conn| self.conns.get(conn).map(|conn| conn.link.clone()))
            .collect();
        for conn in conns {
            self.close_link(conn, "host closed");
        }
        links
    }

    // ─── roots ───

    fn root_start(&mut self) -> String {
        self.root_seq += 1;
        let id = format!("{}#{}:{}", self.name, self.incarnation, self.root_seq);
        self.roots.insert(
            id.clone(),
            RootEntry {
                local: true,
                running: 1,
                foreign: None,
                out: Vec::new(),
            },
        );
        id
    }

    fn root_join_foreign(&mut self, id: &str, from: ConnId) {
        let entry = self.roots.entry(id.to_owned()).or_insert(RootEntry {
            local: false,
            running: 0,
            foreign: None,
            out: Vec::new(),
        });
        if !entry.local {
            entry.foreign.get_or_insert_with(HashSet::new).insert(from);
        }
        entry.running += 1;
    }

    fn root_sent(&mut self, id: &str, conn: ConnId) {
        if let Some(entry) = self.roots.get_mut(id) {
            if !entry.out.contains(&conn) {
                entry.out.push(conn);
            }
        }
    }

    fn root_settle(&mut self, id: &str) {
        let Some(entry) = self.roots.get_mut(id) else {
            return;
        };
        entry.running = entry.running.saturating_sub(1);
        if entry.running == 0 && entry.foreign.is_none() {
            self.end_root(id);
        }
    }

    fn foreign_root_ended(&mut self, id: &str, from: ConnId) {
        let Some(entry) = self.roots.get_mut(id) else {
            return;
        };
        let Some(links) = entry.foreign.as_mut() else {
            return;
        };
        if !links.remove(&from) || !links.is_empty() {
            return;
        }
        if entry.running > 0 {
            // Still running here: the lease ends when it settles.
            entry.foreign = None;
        } else {
            self.end_root(id);
        }
    }

    fn end_root(&mut self, id: &str) {
        let Some(entry) = self.roots.remove(id) else {
            return;
        };
        for conn in entry.out {
            self.send(
                conn,
                Frame::RootEnd {
                    root_id: id.to_owned(),
                },
            );
        }
    }

    // ─── relays ───

    fn route_for(&self, host_name: &str) -> Option<Arc<dyn Route>> {
        self.routes
            .iter()
            .filter(|(prefix, _)| host_name.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, route)| route.clone())
    }

    fn child_named(&self, name: &str) -> Option<ConnId> {
        self.conns
            .iter()
            .find(|(_, conn)| {
                !conn.link.is_closed()
                    && conn.role == Role::Downstream
                    && conn.peer_kind == Kind::Host
                    && conn.peer_name.as_deref() == Some(name)
            })
            .map(|(id, _)| *id)
    }

    /// Still authorized as when the hold arrived.
    fn relay_current(&self, relay: RelayId) -> bool {
        let Some(state) = self.relays.get(&relay) else {
            return false;
        };
        !state.closed
            && self.conns.get(&state.origin).is_some_and(|origin| {
                !origin.link.is_closed() && origin.generation == state.generation
            })
    }

    /// Relays a hold to the host it names: authorize, resolve, authorize
    /// again, with the origin's authorization checked after every step.
    fn relay(&mut self, relay: RelayId) {
        let Some(state) = self.relays.get(&relay) else {
            return;
        };
        let at = state.at.clone();
        let principal = state.principal.clone();
        let Some(route) = self.route_for(&at) else {
            let error = if self.child_named(&at).is_none() {
                ChordError::unavailable(format!("no route to {at}"))
            } else {
                ChordError::forbidden(format!("no route authorizes {at}"))
            };
            self.relay_close(relay, error, true, true);
            return;
        };
        let Some(tx) = self.sender() else { return };
        tokio::spawn(async move {
            let still = |tx: &mpsc::UnboundedSender<Msg>| {
                let (reply, answer) = oneshot::channel();
                let _ = tx.send(Msg::RelayStill { relay, reply });
                async move { answer.await.unwrap_or(false) }
            };
            let outcome = async {
                let refuse = || ChordError::forbidden(format!("{at} refused for this principal"));
                if !route.authorize(&principal, &at).await {
                    return Err(refuse());
                }
                if !still(&tx).await {
                    return Err(revoked());
                }
                let link = route.resolve(&at).await?;
                if !still(&tx).await {
                    return Err(revoked());
                }
                if !route.authorize(&principal, &at).await {
                    return Err(refuse());
                }
                if !still(&tx).await {
                    return Err(revoked());
                }
                Ok((link, route.principal(&at)))
            }
            .await;
            let _ = tx.send(Msg::RelayResolved { relay, outcome });
        });
    }

    /// The connection to a resolved host: the one already on `link`, or a new
    /// downstream host connection.
    fn connection_for(
        &mut self,
        link: Link,
        host_name: &str,
        principal: Value,
    ) -> Result<ConnId, ChordError> {
        if let Some((id, _)) = self.conns.iter().find(|(_, conn)| conn.link.same(&link)) {
            return Ok(*id);
        }
        self.add_connection(
            link,
            Role::Downstream,
            Kind::Host,
            Some(host_name.to_owned()),
            principal,
            None,
            None,
            LinkOptions::default(),
        )
        .ok_or_else(|| ChordError::unavailable(format!("link to {host_name} closed")))
    }

    fn relay_resolved(&mut self, relay: RelayId, outcome: Result<(Link, Value), ChordError>) {
        let Some(at) = self.relays.get(&relay).map(|state| state.at.clone()) else {
            return;
        };
        let conn = outcome.and_then(|(link, principal)| self.connection_for(link, &at, principal));
        match conn {
            Err(error) => self.relay_close(relay, error, true, true),
            Ok(conn) => {
                if !self.relay_current(relay) {
                    self.relay_close(relay, revoked(), true, true);
                    return;
                }
                let Some(down) = self.conns.get_mut(&conn) else {
                    return;
                };
                if down.synced {
                    self.relay_open(relay, conn);
                } else {
                    down.sync_waiters.push(SyncWaiter::Relay(relay));
                }
            }
        }
    }

    /// Forwards the hold (and what queued meanwhile) to `down`.
    fn relay_open(&mut self, relay_id: RelayId, down: ConnId) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        if relay.closed {
            return;
        }
        let Some(conn) = self.conns.get_mut(&down) else {
            return;
        };
        let down_id = conn.next_id();
        conn.relayed_refs.insert(down_id, relay_id);
        let relay = self.relays.get_mut(&relay_id).expect("checked");
        relay.down = Some(down);
        relay.down_id = down_id;
        let frame = Frame::Hold {
            id: down_id,
            at: relay.at.clone(),
            token: Token::Value(relay.token.clone()),
            input: relay.input.clone(),
            principal: Some(relay.principal.clone()),
            client: relay.client.then_some(true),
        };
        let queued_subs: Vec<u64> = relay
            .subs
            .iter()
            .filter(|(_, (_, down_id))| down_id.is_none())
            .map(|(origin_sub, _)| *origin_sub)
            .collect();
        let queued_calls: Vec<u64> = relay
            .calls
            .iter()
            .filter(|(_, call)| call.down_id.is_none())
            .map(|(origin_id, _)| *origin_id)
            .collect();
        self.send(down, frame);
        for origin_sub in queued_subs {
            self.relay_forward_sub(relay_id, origin_sub);
        }
        for origin_id in queued_calls {
            self.relay_forward_call(relay_id, origin_id);
        }
    }

    fn relay_subscribe(&mut self, relay_id: RelayId, origin_sub: u64, member: String) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            return;
        };
        relay.subs.insert(origin_sub, (member, None));
        let (origin, open) = (relay.origin, relay.down.is_some());
        if let Some(conn) = self.conns.get_mut(&origin) {
            conn.relay_subs.insert(origin_sub, relay_id);
        }
        if open {
            self.relay_forward_sub(relay_id, origin_sub);
        }
    }

    fn relay_forward_sub(&mut self, relay_id: RelayId, origin_sub: u64) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        let (Some(down), hold) = (relay.down, relay.down_id) else {
            return;
        };
        let Some((member, _)) = relay.subs.get(&origin_sub).cloned() else {
            return;
        };
        let Some(conn) = self.conns.get_mut(&down) else {
            return;
        };
        let down_id = conn.next_id();
        conn.relayed_subs.insert(down_id, (relay_id, origin_sub));
        if let Some(entry) = self
            .relays
            .get_mut(&relay_id)
            .and_then(|relay| relay.subs.get_mut(&origin_sub))
        {
            entry.1 = Some(down_id);
        }
        self.send(
            down,
            Frame::Sub {
                id: down_id,
                hold,
                member,
            },
        );
    }

    /// The origin could not keep up and dropped what waited: subscribe
    /// downstream anew, so the provider's snapshot replaces the lost ops.
    fn relay_resync(&mut self, relay_id: RelayId, origin_sub: u64) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        let origin = relay.origin;
        let entry = relay
            .subs
            .get(&origin_sub)
            .and_then(|(_, down_id)| *down_id);
        let down = relay.down;
        self.drop_state(origin, origin_sub);
        let (Some(old), Some(down)) = (entry, down) else {
            return;
        };
        if let Some(conn) = self.conns.get_mut(&down) {
            conn.relayed_subs.remove(&old);
        }
        self.send(down, Frame::Unsub { id: old });
        self.relay_forward_sub(relay_id, origin_sub);
    }

    fn relay_unsubscribe(&mut self, relay_id: RelayId, origin_sub: u64, forward: bool) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            return;
        };
        let Some((_, down_id)) = relay.subs.remove(&origin_sub) else {
            return;
        };
        let (origin, down) = (relay.origin, relay.down);
        if let Some(conn) = self.conns.get_mut(&origin) {
            conn.relay_subs.remove(&origin_sub);
        }
        self.drop_state(origin, origin_sub);
        if let (Some(down_id), Some(down)) = (down_id, down) {
            if let Some(conn) = self.conns.get_mut(&down) {
                conn.relayed_subs.remove(&down_id);
            }
            if forward {
                self.send(down, Frame::Unsub { id: down_id });
            }
        }
    }

    fn relay_call(
        &mut self,
        relay_id: RelayId,
        origin_id: u64,
        member: String,
        args: Option<Value>,
        root: String,
    ) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            self.later.push_back(Later::RelayCallDone {
                origin: 0,
                origin_id,
                root,
                outcome: Err(ChordError::gone("relayed hold closed")),
            });
            return;
        };
        let origin = relay.origin;
        if relay.closed {
            self.later.push_back(Later::RelayCallDone {
                origin,
                origin_id,
                root,
                outcome: Err(ChordError::gone("relayed hold closed")),
            });
            return;
        }
        relay.calls.insert(
            origin_id,
            RelayCall {
                down_id: None,
                member,
                args,
                root,
            },
        );
        if relay.down.is_some() {
            self.relay_forward_call(relay_id, origin_id);
        }
    }

    fn relay_forward_call(&mut self, relay_id: RelayId, origin_id: u64) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        let (Some(down), hold) = (relay.down, relay.down_id) else {
            return;
        };
        let Some(call) = relay.calls.get(&origin_id) else {
            return;
        };
        let (member, args, root) = (call.member.clone(), call.args.clone(), call.root.clone());
        let Some(conn) = self.conns.get_mut(&down) else {
            return;
        };
        let down_id = conn.next_id();
        conn.relayed_calls.insert(down_id, (relay_id, origin_id));
        if let Some(call) = self
            .relays
            .get_mut(&relay_id)
            .and_then(|relay| relay.calls.get_mut(&origin_id))
        {
            call.down_id = Some(down_id);
        }
        self.root_sent(&root, down);
        self.send(
            down,
            Frame::Call {
                id: down_id,
                hold,
                member,
                args,
                root: Some(RootRef {
                    root_id: root,
                    code_version: CODE_VERSION,
                }),
            },
        );
    }

    /// A relayed call settled: by its result, a cancel, or the relay closing.
    fn relay_call_finish(
        &mut self,
        relay_id: RelayId,
        origin_id: u64,
        outcome: Result<Option<Value>, ChordError>,
    ) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            return;
        };
        let origin = relay.origin;
        let Some(call) = relay.calls.remove(&origin_id) else {
            return;
        };
        let down = relay.down;
        if let (Some(down_id), Some(down)) = (call.down_id, down) {
            if let Some(conn) = self.conns.get_mut(&down) {
                conn.relayed_calls.remove(&down_id);
            }
        }
        self.later.push_back(Later::RelayCallDone {
            origin,
            origin_id,
            root: call.root,
            outcome,
        });
    }

    /// Cancels a relayed call downstream (when the downstream hold still
    /// exists) and settles it with `error`.
    fn relay_call_cancel(
        &mut self,
        relay_id: RelayId,
        origin_id: u64,
        error: ChordError,
        forward: bool,
    ) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        let Some(call) = relay.calls.get(&origin_id) else {
            return;
        };
        if let (true, Some(down_id), Some(down)) = (forward, call.down_id, relay.down) {
            self.send(down, Frame::Cancel { id: down_id });
        }
        self.relay_call_finish(relay_id, origin_id, Err(error));
    }

    /// Closes a relay once. `release_down`: the downstream hold still exists
    /// (the origin released, disconnected or was revoked): cancel its calls
    /// and release it. `notify`: tell the origin the hold is gone.
    fn relay_close(
        &mut self,
        relay_id: RelayId,
        error: ChordError,
        release_down: bool,
        notify: bool,
    ) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            return;
        };
        if relay.closed {
            return;
        }
        relay.closed = true;
        let calls: Vec<u64> = relay.calls.keys().copied().collect();
        let subs: Vec<u64> = relay.subs.keys().copied().collect();
        let (origin, origin_id, down, down_id) =
            (relay.origin, relay.origin_id, relay.down, relay.down_id);
        for call in calls {
            self.relay_call_cancel(relay_id, call, error.clone(), release_down);
        }
        for sub in subs {
            self.relay_unsubscribe(relay_id, sub, release_down);
        }
        if let Some(conn) = self.conns.get_mut(&origin) {
            if matches!(conn.served.get(&origin_id), Some(Served::Relay(id)) if *id == relay_id) {
                conn.served.remove(&origin_id);
            }
        }
        if let Some(down) = down {
            if let Some(conn) = self.conns.get_mut(&down) {
                conn.relayed_refs.remove(&down_id);
            }
            if release_down {
                self.send(down, Frame::Release { id: down_id });
            }
        }
        if notify {
            self.send(
                origin,
                Frame::Gone {
                    id: origin_id,
                    error: error.to_wire(),
                },
            );
        }
        self.relays.remove(&relay_id);
    }

    fn run_later(&mut self) {
        while let Some(later) = self.later.pop_front() {
            match later {
                Later::RelayCallDone {
                    origin,
                    origin_id,
                    root,
                    outcome,
                } => {
                    // The root ends (its `root_end` goes out), then the origin hears the result.
                    self.root_settle(&root);
                    let Some(conn) = self.conns.get_mut(&origin) else {
                        continue;
                    };
                    conn.incoming.remove(&origin_id);
                    let frame = match outcome {
                        Ok(value) => Frame::Result {
                            id: origin_id,
                            value,
                            error: None,
                        },
                        Err(error) => Frame::Result {
                            id: origin_id,
                            value: None,
                            error: Some(error.to_wire()),
                        },
                    };
                    self.send(origin, frame);
                }
            }
        }
    }

    // ─── remote references (consumer side) ───

    fn get(
        &mut self,
        id: RefId,
        contract: Arc<Description>,
        at: String,
        input: Value,
        principal: Option<Value>,
        status: watch::Sender<RefStatus>,
    ) {
        self.refs.insert(
            id,
            RemoteState {
                contract: contract.clone(),
                at: at.clone(),
                input,
                principal,
                status,
                binding: None,
                queued: Vec::new(),
                readables: BTreeMap::new(),
            },
        );
        if contract.scope != Scope::Remote {
            let error = ChordError::invalid(
                "local",
                format!("{} is local and cannot be reached at {at}", contract.name),
            );
            self.ref_gone(id, error);
            return;
        }
        if let Some(upstream) = self.upstream {
            self.hold(upstream, id);
            return;
        }
        if let Some(child) = self.child_named(&at) {
            let conn = self.conns.get_mut(&child).expect("found");
            if conn.synced {
                self.hold(child, id);
            } else {
                conn.sync_waiters.push(SyncWaiter::Ref(id));
            }
            return;
        }
        let Some(route) = self.route_for(&at) else {
            self.ref_gone(id, ChordError::unavailable(format!("no route to {at}")));
            return;
        };
        let principal = self
            .refs
            .get(&id)
            .and_then(|state| state.principal.clone())
            .unwrap_or_else(|| json!({ "host": true }));
        let Some(tx) = self.sender() else { return };
        tokio::spawn(async move {
            let outcome = async {
                let refuse = || ChordError::forbidden(format!("{at} refused for this principal"));
                if !route.authorize(&principal, &at).await {
                    return Err(refuse());
                }
                let link = route.resolve(&at).await?;
                if !route.authorize(&principal, &at).await {
                    return Err(refuse());
                }
                Ok((link, route.principal(&at)))
            }
            .await;
            let _ = tx.send(Msg::RefResolved {
                reference: id,
                outcome,
            });
        });
    }

    fn ref_resolved(&mut self, reference: RefId, outcome: Result<(Link, Value), ChordError>) {
        let Some(at) = self.refs.get(&reference).map(|state| state.at.clone()) else {
            return;
        };
        match outcome.and_then(|(link, principal)| self.connection_for(link, &at, principal)) {
            Err(error) => self.ref_gone(reference, error),
            Ok(conn) => {
                let Some(down) = self.conns.get_mut(&conn) else {
                    return;
                };
                if down.synced {
                    let unbound = self.refs.get(&reference).is_some_and(|state| {
                        state.binding.is_none() && state.status() == RefStatus::Loading
                    });
                    if unbound {
                        self.hold(conn, reference);
                    }
                } else {
                    down.sync_waiters.push(SyncWaiter::Ref(reference));
                }
            }
        }
    }

    /// Holds `reference` over `conn`.
    fn hold(&mut self, conn_id: ConnId, reference: RefId) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        let id = conn.next_id();
        conn.remotes.insert(id, reference);
        state.binding = Some((conn_id, id));
        let frame = Frame::Hold {
            id,
            at: state.at.clone(),
            token: Token::Description(state.contract.clone()),
            input: state.input.clone(),
            principal: state.principal.clone(),
            client: None,
        };
        self.send(conn_id, frame);
    }

    fn ref_shaped(&mut self, reference: RefId, shape: Shape) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if state.settled() {
            return;
        }
        state.status.send_replace(RefStatus::Live(shape));
        let members: Vec<String> = state.readables.keys().cloned().collect();
        for member in members {
            self.readable_bind(reference, &member);
        }
        let queued = self
            .refs
            .get_mut(&reference)
            .map(|state| std::mem::take(&mut state.queued))
            .unwrap_or_default();
        for call in queued {
            self.ref_call(reference, call.key, call.member, call.args, call.reply);
        }
    }

    fn ref_gone(&mut self, reference: RefId, error: ChordError) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if state.settled() {
            return;
        }
        state.status.send_replace(RefStatus::Gone(error.clone()));
        state.binding = None;
        for call in std::mem::take(&mut state.queued) {
            let _ = call.reply.send(Err(error.clone()));
        }
        self.readables_stop(reference);
        self.refs.remove(&reference);
    }

    /// The upstream link went down but may come back: wait, then hold again.
    fn ref_detached(&mut self, reference: RefId) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if state.settled() {
            return;
        }
        state.binding = None;
        for readable in state.readables.values_mut() {
            readable.sub = None;
        }
        if matches!(state.status(), RefStatus::Live(_)) {
            state.status.send_replace(RefStatus::Loading);
        }
    }

    fn ref_release(&mut self, reference: RefId) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if state.settled() {
            return;
        }
        state.status.send_replace(RefStatus::Released);
        let released = ChordError::new(ErrorCode::Released, "reference released");
        for call in std::mem::take(&mut state.queued) {
            let _ = call.reply.send(Err(released.clone()));
        }
        self.readables_stop(reference);
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if let Some((conn, id)) = state.binding.take() {
            if let Some(conn_state) = self.conns.get_mut(&conn) {
                conn_state.remotes.remove(&id);
            }
            self.send(conn, Frame::Release { id });
        }
        self.refs.remove(&reference);
    }

    fn ref_call(
        &mut self,
        reference: RefId,
        key: u64,
        member: String,
        args: Option<Value>,
        reply: CallReply,
    ) {
        let Some(state) = self.refs.get_mut(&reference) else {
            let _ = reply.send(Err(ChordError::new(
                ErrorCode::Released,
                "reference released",
            )));
            return;
        };
        let shape = match state.status() {
            RefStatus::Released => {
                let _ = reply.send(Err(ChordError::new(
                    ErrorCode::Released,
                    "reference released",
                )));
                return;
            }
            RefStatus::Gone(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            RefStatus::Loading => {
                state.queued.push(QueuedCall {
                    key,
                    member,
                    args,
                    reply,
                });
                return;
            }
            RefStatus::Live(shape) => shape,
        };
        let Some((conn, hold)) = state.binding else {
            state.queued.push(QueuedCall {
                key,
                member,
                args,
                reply,
            });
            return;
        };
        let variant = shape.variant.clone().unwrap_or_default();
        let description = state
            .contract
            .variants
            .get(&variant)
            .and_then(|members| members.get(&member));
        let Some(MemberDescription::Method { args: schema, .. }) = description else {
            let _ = reply.send(Err(ChordError::new(
                ErrorCode::NotInVariant,
                format!(
                    "{}: {member} is not a method of variant {variant}",
                    state.contract.name
                ),
            )));
            return;
        };
        // Send only what the consumer's own schema knows.
        let sent = schema
            .as_ref()
            .map(|schema| validate::clean(schema, args.unwrap_or(Value::Null)));
        // Clients never carry roots: the server starts them.
        let root = (self.kind == Kind::Host).then(|| self.root_start());
        if let Some(root) = &root {
            self.root_sent(root, conn);
        }
        let Some(conn_state) = self.conns.get_mut(&conn) else {
            return;
        };
        let id = conn_state.next_id();
        conn_state.calls.insert(
            id,
            OutCall {
                reference,
                key,
                root: root.clone(),
                reply,
            },
        );
        self.send(
            conn,
            Frame::Call {
                id,
                hold,
                member,
                args: sent,
                root: root.map(|root_id| RootRef {
                    root_id,
                    code_version: CODE_VERSION,
                }),
            },
        );
    }

    fn ref_cancel(&mut self, reference: RefId, key: u64) {
        if let Some(state) = self.refs.get_mut(&reference) {
            if let Some(index) = state.queued.iter().position(|call| call.key == key) {
                let call = state.queued.remove(index);
                let _ = call.reply.send(Err(ChordError::aborted("call aborted")));
                return;
            }
        }
        let found = self.conns.iter().find_map(|(conn_id, conn)| {
            conn.calls
                .iter()
                .find(|(_, call)| call.reference == reference && call.key == key)
                .map(|(id, _)| (*conn_id, *id))
        });
        let Some((conn_id, id)) = found else { return };
        let Some(call) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|conn| conn.calls.remove(&id))
        else {
            return;
        };
        self.send(conn_id, Frame::Cancel { id });
        if let Some(root) = &call.root {
            self.root_settle(root);
        }
        let _ = call.reply.send(Err(ChordError::aborted("call aborted")));
    }

    fn ref_subscribe(
        &mut self,
        reference: RefId,
        member: String,
        sink: u64,
        sender: mpsc::UnboundedSender<StateUpdate>,
    ) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        let readable = state
            .readables
            .entry(member.clone())
            .or_insert_with(|| Readable {
                sinks: Vec::new(),
                sub: None,
                decoder: Decoder::new(),
                value: None,
            });
        if let Some(value) = &readable.value {
            let _ = sender.send(StateUpdate::Snapshot(value.clone()));
        }
        readable.sinks.push((sink, sender));
        self.readable_bind(reference, &member);
    }

    fn ref_unsubscribe(&mut self, reference: RefId, member: &str, sink: u64) {
        let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(member))
        else {
            return;
        };
        readable.sinks.retain(|(id, _)| *id != sink);
        self.readable_bind(reference, member);
    }

    /// Subscribes upstream while someone listens and the reference is bound;
    /// drops a subscription on an old link.
    fn readable_bind(&mut self, reference: RefId, member: &str) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        let binding = state.binding;
        let Some(readable) = state.readables.get_mut(member) else {
            return;
        };
        let listening = !readable.sinks.is_empty();
        if !listening {
            readable.value = None;
        }
        let binding = if listening { binding } else { None };
        if let (Some((conn, _)), Some((sub_conn, _))) = (binding, readable.sub) {
            if conn == sub_conn {
                return;
            }
        }
        if let Some((sub_conn, sub_id)) = readable.sub.take() {
            if let Some(conn) = self.conns.get_mut(&sub_conn) {
                if conn.subs.remove(&sub_id).is_some() {
                    self.send(sub_conn, Frame::Unsub { id: sub_id });
                }
            }
        }
        let Some((conn_id, hold)) = binding else {
            return;
        };
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return;
        };
        let id = conn.next_id();
        conn.subs.insert(id, (reference, member.to_owned()));
        if let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(member))
        {
            readable.decoder = Decoder::new();
            readable.sub = Some((conn_id, id));
        }
        self.send(
            conn_id,
            Frame::Sub {
                id,
                hold,
                member: member.to_owned(),
            },
        );
    }

    fn readables_stop(&mut self, reference: RefId) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        let members: Vec<String> = state.readables.keys().cloned().collect();
        for readable in state.readables.values_mut() {
            readable.sinks.clear();
        }
        for member in members {
            self.readable_bind(reference, &member);
        }
    }

    fn readable_receive(&mut self, reference: RefId, member: &str, frame: StateFrame) {
        let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(member))
        else {
            return;
        };
        let update = if let Some(error) = &frame.error {
            let error = ChordError::from_wire(error);
            self.report(error.clone());
            StateUpdate::Error(error)
        } else if let Some(wire) = frame.ops {
            let ops = match readable.decoder.decode(&wire) {
                Ok(ops) => ops,
                Err(error) => {
                    let error = ChordError::invalid("state", error.to_string());
                    self.report(error);
                    return;
                }
            };
            let base = readable.value.take().unwrap_or(Value::Null);
            match apply_owned(base, &ops) {
                Ok(value) => {
                    readable.value = Some(value.clone());
                    StateUpdate::Ops { ops, value }
                }
                Err(error) => {
                    let error = ChordError::invalid("state", error.to_string());
                    self.report(error);
                    return;
                }
            }
        } else if let Some(value) = frame.snap.or(frame.reset.clone()) {
            readable.decoder = Decoder::new();
            readable.value = Some(value.clone());
            if frame.reset.is_some() {
                StateUpdate::Reset(value)
            } else {
                StateUpdate::Snapshot(value)
            }
        } else {
            return;
        };
        let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(member))
        else {
            return;
        };
        readable
            .sinks
            .retain(|(_, sink)| sink.send(update.clone()).is_ok());
    }
}

fn revoked() -> ChordError {
    ChordError::gone("principal invalidated while resolving")
}

/// Clients holding an outdated contract are told to update their code.
fn client_error(error: ChordError) -> ChordError {
    if error.code == ErrorCode::ContractMismatch {
        return ChordError::new(ErrorCode::UpgradeRequired, error.to_string());
    }
    error
}

/// How a consumer token binds to the provider: equal fingerprints, or the
/// same version with a description that covers the consumer's. noel has no
/// `accepts` adapters.
fn bind(
    service: &Service,
    consumer: &Description,
    consumer_fingerprint: &str,
) -> Result<(), ChordError> {
    let provider = &service.description;
    if service.fingerprint == consumer_fingerprint {
        return Ok(());
    }
    if provider.version == consumer.version {
        return match coverage(provider, consumer) {
            None => Ok(()),
            Some(reason) => Err(ChordError::new(
                ErrorCode::ContractMismatch,
                format!(
                    "{}@{} does not cover the consumer's description ({reason}); forgotten version bump?",
                    provider.name, provider.version
                ),
            )),
        };
    }
    Err(ChordError::new(
        ErrorCode::ContractMismatch,
        format!(
            "{}: provider version {}, consumer version {}, no accepts adapter",
            provider.name, provider.version, consumer.version
        ),
    ))
}

/// The implementation must be a declared variant with exactly its members.
fn check_implementation(
    description: &Description,
    implementation: &Implementation,
) -> Result<(), ChordError> {
    let fail = |message: String| {
        ChordError::invalid("implementation", format!("{}: {message}", description.name))
    };
    let variant = implementation.variant.clone().unwrap_or_default();
    let Some(members) = description.variants.get(&variant) else {
        return Err(fail(format!("variant {variant} is not declared")));
    };
    for (name, member) in &implementation.members {
        match (members.get(name), member) {
            (Some(MemberDescription::State { .. }), Member::State(_))
            | (Some(MemberDescription::Method { .. }), Member::Method(_)) => {}
            (None, _) => return Err(fail(format!("{name} is not in variant {variant}"))),
            _ => {
                return Err(fail(format!(
                    "{name} is not the kind the contract declares"
                )))
            }
        }
    }
    for (name, _) in members.iter() {
        if implementation.member(name).is_none() {
            return Err(fail(format!("{name} is missing from variant {variant}")));
        }
    }
    Ok(())
}

fn shape_of(object: &Object) -> Shape {
    Shape {
        variant: object.variant.clone(),
        members: object
            .members
            .iter()
            .map(|(name, _)| name.clone())
            .collect(),
        contract: object.service.fingerprint.clone(),
        code_version: CODE_VERSION as i64,
    }
}
