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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
use crate::wire::{Frame, Kind, RootRef, StateAs, StateFrame, Token, PROTOCOL_VERSION};

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
    /// The principal of the host's own code (upstream `createHost({ self })`):
    /// its own holds act as it, and a hold asking for it forwards no
    /// principal.
    pub self_principal: Option<Value>,
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
            self_principal: None,
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

/// A provider's shape for a hold: the variant, its members, the provider
/// contract's fingerprint, and the state members whose snapshots follow
/// (whose arrival hydrates the reference).
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub variant: Option<String>,
    pub members: Vec<String>,
    /// The provider contract's fingerprint.
    pub contract: String,
    /// The state members whose snapshots follow this shape.
    pub state: Vec<String>,
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

    /// Releases the hold (and its subscriptions): one view of a shared
    /// binding; the actor ends the binding when the last view leaves.
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
        member: String,
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
    /// Whether the consumer wants this hold's state (a hold implies it
    /// until it says `state: false`).
    wants_state: bool,
    /// The consumer already has values (a re-hold after a link loss): its
    /// unit is not urgent.
    again: bool,
    /// A unit (shape plus snapshots) has been sent for this hold.
    unit_sent: bool,
    /// Per state member: the feed into the object's cell.
    feeds: BTreeMap<String, Feed>,
    consumer: Arc<Description>,
}

/// The cloneable summary of a served hold, for code that only reads it.
#[derive(Clone)]
struct HoldSummary {
    object: String,
    wants_state: bool,
    members: Vec<String>,
}

enum Served {
    Local(LocalHold),
    Relay(RelayId),
}

impl Served {
    fn summary(&self) -> Option<HoldSummary> {
        match self {
            Served::Local(local) => Some(HoldSummary {
                object: local.object.clone(),
                wants_state: local.wants_state,
                members: local.feeds.keys().cloned().collect(),
            }),
            Served::Relay(_) => None,
        }
    }
}

struct Feed {
    cell: Option<(Cell, u64)>,
    encoder: Encoder,
}
enum Incoming {
    Local(AbortSignal),
    Relay(RelayId),
}

/// What [`Actor::send_state`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SendState {
    /// The frame went out.
    Sent,
    /// It waits behind the window.
    Waiting,
    /// The slot turned lazy: its flush sends a reset, and a relay asks its
    /// downstream member again.
    Lazy,
}

#[derive(Default)]
struct PendingState {
    base: Option<StateFrame>,
    ops: Vec<Value>,
    size: usize,
    /// A reset at flush replaces this slot's ops: they are already covered
    /// (upstream's lazy slots).
    lazy: bool,
}

impl Conn {}

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
    pending: Vec<((u64, String), PendingState)>,
    /// Bytes of state ops waiting across every stream on this link
    /// (upstream `#pendingBytes`), capped at `pending_cap`.
    pending_bytes: usize,
    /// The cap: `window * 4` when a window is set (upstream `#pendingCap`).
    pending_cap: usize,
    received: u64,
    mismatched: bool,
    /// Holds over a client link carry its id as their origin (also when
    /// relayed), so hosts can tell clients apart: `<incarnation>:<seq>`.
    origin_tag: Option<String>,
    /// consumer side: what this host asked of the peer
    remotes: HashMap<u64, RefId>,
    calls: HashMap<u64, OutCall>,
    // provider side: what the peer asked of this host
    served: BTreeMap<u64, Served>,
    incoming: HashMap<u64, Incoming>,
    /// Downstream ends of relays: hold ids this host used on this link.
    relayed_refs: HashMap<u64, RelayId>,
    relayed_calls: HashMap<u64, (RelayId, u64)>,
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
    /// The client connection the hold came from, when it did: an opaque id
    /// forwarded downstream.
    origin_label: Option<String>,
    /// Whether the origin wants this hold's state.
    wants_state: bool,
    /// The origin already has values (a re-hold): its unit is not urgent.
    again: bool,
    generation: u64,
    down: Option<ConnId>,
    down_id: u64,
    closed: bool,
    calls: BTreeMap<u64, RelayCall>,
}

struct RootEntry {
    local: bool,
    running: usize,
    foreign: Option<HashSet<ConnId>>,
    out: Vec<ConnId>,
}

struct Readable {
    sinks: Vec<(u64, mpsc::UnboundedSender<StateUpdate>)>,
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
    /// The binding key this state serves (contract, input identity,
    /// principal): references of one binding share it.
    key: String,
    contract: Arc<Description>,
    at: String,
    input: Value,
    principal: Option<Value>,
    status: watch::Sender<RefStatus>,
    binding: Option<(ConnId, u64)>,
    queued: Vec<QueuedCall>,
    readables: BTreeMap<String, Readable>,
    /// Whether the peer currently believes this hold wants state: the hold
    /// frame says so (`sub`/`unsub` would move it; noel's holds always
    /// want state).
    peer_wants_state: bool,
    /// The hold is a re-hold after a link loss (the values are not new).
    again: bool,
    /// The state members the shape promised, whose snapshots hydrate the
    /// reference before it is live.
    pending_snapshots: BTreeSet<String>,
    /// The shape of the current binding, kept while its snapshots arrive.
    shape: Option<Shape>,
    /// The views of this binding: references sharing one hold. The wire
    /// release goes out when the last view ends.
    views_handle: Arc<AtomicUsize>,
    /// Status channels of the views sharing this binding: every change is
    /// mirrored into each (each view's `ready` waits on its own).
    mirrors: Vec<watch::Sender<RefStatus>>,
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
    origin_seq: u32,
    self_principal: Option<Value>,
    /// The binding key of each reference: references sharing a key share
    /// one hold on the wire.
    binding_keys: HashMap<RefId, String>,
    /// References that share a binding: their id to the binding's id.
    ref_alias: HashMap<RefId, RefId>,
    /// The binding's canonical handle parts, shared with its views.
    binding_handles: HashMap<RefId, Arc<AtomicUsize>>,
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
            origin_seq: 0,
            self_principal: options.self_principal.clone(),
            binding_keys: HashMap::new(),
            ref_alias: HashMap::new(),
            binding_handles: HashMap::new(),
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
            Msg::Cell {
                conn,
                sub,
                member,
                change,
            } => self.cell_changed(conn, sub, member, change),
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
            // A shared binding's views address it by their own id: every
            // reference message resolves to the binding first.
            Msg::Call {
                reference,
                key,
                member,
                args,
                reply,
            } => self.ref_call(self.binding_of(reference), key, member, args, reply),
            Msg::Cancel { reference, key } => self.ref_cancel(self.binding_of(reference), key),
            Msg::Subscribe {
                reference,
                member,
                sink,
                sender,
            } => self.ref_subscribe(self.binding_of(reference), member, sink, sender),
            Msg::Unsubscribe {
                reference,
                member,
                sink,
            } => self.ref_unsubscribe(self.binding_of(reference), &member, sink),
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
        let origin_tag = if role == Role::Downstream && peer_kind == Kind::Client {
            let incarnation = self.incarnation.clone();
            Some(format!("{}:{}", incarnation, self.next_origin_seq()))
        } else {
            // A relayed client hold keeps its origin; our own holds under
            // an upstream link carry none.
            None
        };
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
            pending_bytes: 0,
            pending_cap: options
                .window
                .map(|window| window.saturating_mul(PENDING_WINDOWS))
                .unwrap_or(usize::MAX),
            received: 0,
            mismatched: false,
            origin_tag,
            remotes: HashMap::new(),
            calls: HashMap::new(),
            served: BTreeMap::new(),
            incoming: HashMap::new(),
            relayed_refs: HashMap::new(),
            relayed_calls: HashMap::new(),
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

    /// Sends a state frame, or keeps it waiting behind a full window. A
    /// slot whose ops outgrow the link's shared pending budget turns lazy:
    /// its flush sends the member's current value as a reset instead
    /// (upstream `sendState` and `#makeRoom`). `Lazy` tells a relay to ask
    /// downstream for a fresh member (upstream's `overflow.resync`).
    fn send_state(&mut self, conn_id: ConnId, frame: StateFrame) -> SendState {
        let Some(conn) = self.conns.get_mut(&conn_id) else {
            return SendState::Sent;
        };
        let key = (frame.hold, frame.m.clone());
        let index = conn.pending.iter().position(|(slot, _)| *slot == key);
        if index.is_none() && !conn.full() {
            self.send(conn_id, Frame::State(frame));
            return SendState::Sent;
        }
        let index = match index {
            Some(index) => index,
            None => {
                conn.pending.push((key, PendingState::default()));
                conn.pending.len() - 1
            }
        };
        let slot = &mut conn.pending[index].1;
        let Some(ops) = frame.ops else {
            conn.pending_bytes = conn.pending_bytes.saturating_sub(slot.size);
            slot.base = Some(frame);
            slot.ops.clear();
            slot.size = 0;
            slot.lazy = false;
            return SendState::Waiting;
        };
        if slot.lazy {
            return SendState::Lazy;
        }
        let size = crate::wire::text_size(&serde_json::to_string(&ops).expect("ops are JSON"));
        let over = conn.pending_bytes.saturating_add(size) > conn.pending_cap;
        if over {
            // Drop waiting streams, oldest first, then this one: each
            // becomes a lazy reset at flush.
            let mut order: Vec<usize> = (0..conn.pending.len()).collect();
            order.retain(|&i| i != index);
            order.push(index);
            for i in order {
                if conn.pending_bytes.saturating_add(size) <= conn.pending_cap {
                    break;
                }
                let slot = &mut conn.pending[i].1;
                if slot.size == 0 && i != index {
                    continue;
                }
                conn.pending_bytes = conn.pending_bytes.saturating_sub(slot.size);
                slot.base = None;
                slot.ops.clear();
                slot.size = 0;
                slot.lazy = true;
            }
        }
        let slot = &mut conn.pending[index].1;
        if slot.lazy {
            return SendState::Lazy;
        }
        slot.ops.extend(ops);
        slot.size += size;
        conn.pending_bytes += size;
        SendState::Waiting
    }

    /// Drops the pending state of one hold (all its members), or of one
    /// member when `m` names it.
    fn drop_state(&mut self, conn: ConnId, hold: u64, m: Option<&str>) {
        if let Some(conn) = self.conns.get_mut(&conn) {
            let mut bytes = 0;
            conn.pending.retain(|(slot, state)| {
                let drop = slot.0 != hold || m.is_some_and(|m| slot.1 != m);
                if drop {
                    bytes += state.size;
                }
                !drop
            });
            conn.pending_bytes = conn.pending_bytes.saturating_sub(bytes);
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
            let ((hold, m), slot) = conn.pending.remove(0);
            conn.pending_bytes = conn.pending_bytes.saturating_sub(slot.size);
            if let Some(base) = slot.base {
                self.send(conn_id, Frame::State(base));
            }
            if !slot.ops.is_empty() {
                self.send(
                    conn_id,
                    Frame::State(StateFrame {
                        hold,
                        m,
                        as_kind: StateAs::Ops,
                        ops: Some(slot.ops),
                        ..StateFrame::default()
                    }),
                );
            } else if slot.lazy {
                // A lazy slot flushes as a reset of the member's current
                // value, which the provider builds from its object.
                self.flush_lazy_reset(conn_id, hold, m);
            }
        }
    }

    /// A lazy slot's flush: the member's current value as a reset.
    fn flush_lazy_reset(&mut self, conn_id: ConnId, hold: u64, m: String) {
        let object = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(|served| match served {
                Served::Local(local) => Some(local.object.clone()),
                Served::Relay(_) => None,
            });
        let Some(address) = object else { return };
        let Some(object) = self.objects.get(&address) else {
            return;
        };
        let Some(Member::State(cell)) = object.member(&m) else {
            return;
        };
        let value = cell.value();
        let schema = self.schema_of(&address, &m);
        let what = format!("{address}.{m}");
        let frame = match schema
            .as_ref()
            .map(|schema| validate::assert_valid("state", &what, schema, &value))
        {
            Some(Err(error)) => StateFrame {
                hold,
                m,
                as_kind: StateAs::Error,
                error: Some(error.to_wire()),
                ..StateFrame::default()
            },
            _ => StateFrame {
                hold,
                m,
                as_kind: StateAs::Reset,
                value: Some((*value).clone()),
                ..StateFrame::default()
            },
        };
        self.send(conn_id, Frame::State(frame));
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
                origin,
                state,
                again,
            } => self.on_hold(
                conn,
                id,
                at,
                token.to_value(),
                input,
                principal,
                origin,
                state != Some(false),
                again == Some(true),
            ),
            Frame::Release { id } => self.drop_served(conn, id, true),
            Frame::Call {
                id,
                hold,
                member,
                args,
                root,
            } => self.on_call(conn, id, hold, member, args, root),
            Frame::Cancel { id } => self.on_cancel(conn, id),
            Frame::Sub { hold } => self.on_want_state(conn, hold, true),
            Frame::Unsub { hold } => self.on_want_state(conn, hold, false),
            Frame::Resync { hold, m } => self.on_resync(conn, hold, m),
            Frame::Shape {
                id,
                variant,
                members,
                contract,
                state,
            } => self.on_shape(
                conn,
                id,
                Shape {
                    variant,
                    members,
                    contract,
                    state,
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
        origin: Option<String>,
        wants_state: bool,
        again: bool,
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
            Role::Upstream => origin.is_some(),
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
            // A hold from a client carries that connection's origin; a
            // forwarded one keeps what it came with.
            let origin_label = match conn.role {
                Role::Downstream if conn.peer_kind == Kind::Client => conn.origin_tag.clone(),
                _ => origin.clone(),
            };
            self.relays.insert(
                relay_id,
                Relay {
                    origin: conn_id,
                    origin_id: id,
                    at,
                    token,
                    input,
                    principal,
                    origin_label,
                    wants_state,
                    again,
                    generation,
                    down: None,
                    down_id: 0,
                    closed: false,
                    calls: BTreeMap::new(),
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
                // The provider's own placement decides, never where the
                // frame says it is going.
                let description = self
                    .objects
                    .get(&address)
                    .map(|object| object.service.description.clone())
                    .expect("acquired");
                if let Some(placement) = &description.host {
                    if placement.resolve(&description.name, &input) != Ok(at.clone()) {
                        let error = ChordError::invalid(
                            "placement",
                            format!("{} is not placed on {at}", description.name),
                        );
                        self.send(
                            conn_id,
                            Frame::Gone {
                                id,
                                error: error.to_wire(),
                            },
                        );
                        return;
                    }
                }
                let object = self.objects.get_mut(&address).expect("acquired");
                object.holders.push((conn_id, id));
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
                            wants_state,
                            again,
                            unit_sent: false,
                            feeds: BTreeMap::new(),
                            consumer,
                        }),
                    );
                }
                self.observe(event);
                self.send_unit(conn_id, id);
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
                for name in hold.feeds.keys().cloned().collect::<Vec<_>>() {
                    self.detach_feed(conn_id, id, &hold.object, &name);
                }
                self.drop_state(conn_id, id, None);
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

    /// The state members of the current shape the consumer's contract
    /// knows, in the shape's order.
    fn state_members(consumer: &Description, object: &Object) -> Vec<String> {
        let variant = object.variant.clone().unwrap_or_default();
        object
            .members
            .iter()
            .map(|(name, _)| name)
            .filter(|name| {
                consumer
                    .variants
                    .get(&variant)
                    .is_some_and(|members| members.contains_key(name))
                    && matches!(
                        consumer.variants.get(&variant).and_then(|m| m.get(name)),
                        Some(MemberDescription::State { .. })
                    )
            })
            .cloned()
            .collect()
    }

    /// The shape changed, state was asked for, or the hold is new: members
    /// that left lose their feed, and a unit goes out with the shape and
    /// the members the consumer lacks.
    fn send_unit(&mut self, conn_id: ConnId, hold: u64) {
        let summary = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(Served::summary);
        let Some(summary) = summary else { return };
        let Some(object) = self.objects.get(&summary.object) else {
            return;
        };
        let consumer = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(|served| match served {
                Served::Local(local) => Some(local.consumer.clone()),
                _ => None,
            });
        let Some(consumer) = consumer else { return };
        let members = Self::state_members(&consumer, object)
            .into_iter()
            .filter(|_| summary.wants_state)
            .collect::<Vec<_>>();
        for name in summary.members.clone() {
            if members.contains(&name) {
                continue;
            }
            self.detach_feed(conn_id, hold, &summary.object, &name);
        }
        let listed: Vec<String> = members
            .iter()
            .filter(|name| !summary.members.contains(name))
            .cloned()
            .collect();
        let again = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .is_some_and(|served| matches!(served, Served::Local(l) if l.again));
        let mut snaps = Vec::new();
        for name in &listed {
            if let Some(frame) = self.attach(conn_id, hold, &summary.object, name) {
                snaps.push(frame);
            }
        }
        let _ = again;
        if let Some(Served::Local(local)) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|c| c.served.get_mut(&hold))
        {
            local.unit_sent = true;
        }
        let shape = self.objects.get(&summary.object).map(shape_of);
        let Some(shape) = shape else { return };
        self.send(
            conn_id,
            Frame::Shape {
                id: hold,
                variant: shape.variant.clone(),
                members: shape.members.clone(),
                contract: shape.contract.clone(),
                state: snaps.iter().map(|frame| frame.m.clone()).collect(),
            },
        );
        for frame in snaps {
            self.send_state(conn_id, frame);
        }
    }

    /// Subscribes member `name` for the consumer: its snapshot (as a frame
    /// of kind `as`), and the feed into the object's cell. `None` if the
    /// member is not readable state.
    fn attach(
        &mut self,
        conn_id: ConnId,
        hold_id: u64,
        address: &str,
        name: &str,
    ) -> Option<StateFrame> {
        let object = self.objects.get(address)?;
        let cell = match object.member(name) {
            Some(Member::State(cell)) => cell.clone(),
            _ => return None,
        };
        let (listener, snapshot) = cell.listen(Arc::new({
            let tx = self.tx.clone();
            let member = name.to_owned();
            move |change: &CellChange| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Msg::Cell {
                        conn: conn_id,
                        sub: hold_id,
                        member: member.clone(),
                        change: change.clone(),
                    });
                }
            }
        }));
        // Remember the listener with the feed.
        if let Some(Served::Local(local)) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|conn| conn.served.get_mut(&hold_id))
        {
            local.feeds.insert(
                name.to_owned(),
                Feed {
                    cell: Some((cell, listener)),
                    encoder: Encoder::new(),
                },
            );
        }
        let schema = self.schema_of(address, name);
        let what = format!("{address}.{name}");
        let frame = match schema
            .as_ref()
            .map(|schema| validate::assert_valid("state", &what, schema, &snapshot))
        {
            Some(Err(error)) => StateFrame {
                hold: hold_id,
                m: name.to_owned(),
                as_kind: StateAs::Error,
                error: Some(error.to_wire()),
                ..StateFrame::default()
            },
            _ => StateFrame {
                hold: hold_id,
                m: name.to_owned(),
                as_kind: StateAs::Snap,
                value: Some((*snapshot).clone()),
                ..StateFrame::default()
            },
        };
        Some(frame)
    }

    /// Stops the feed of one member.
    /// Stops the feed of one member of one hold: the hold's own, never
    /// another hold of the same object.
    fn detach_feed(&mut self, conn_id: ConnId, hold_id: u64, address: &str, name: &str) {
        let Some(Served::Local(local)) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|conn| conn.served.get_mut(&hold_id))
        else {
            return;
        };
        if local.object != address {
            return;
        }
        if let Some(feed) = local.feeds.remove(name) {
            if let Some((cell, listener)) = feed.cell {
                cell.unlisten(listener);
            }
        }
    }

    fn schema_of(&self, address: &str, name: &str) -> Option<Value> {
        let object = self.objects.get(address)?;
        let variant = object.variant.clone().unwrap_or_default();
        object
            .service
            .description
            .variants
            .get(&variant)
            .and_then(|members| members.get(name))
            .and_then(|description| match description {
                MemberDescription::State { schema } => Some(schema.clone()),
                _ => None,
            })
    }

    /// A cell under one of this connection's holds changed.
    fn cell_changed(&mut self, conn_id: ConnId, hold: u64, member: String, change: CellChange) {
        let has_feed = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .is_some_and(
                |served| matches!(&served, Served::Local(l) if l.feeds.contains_key(&member)),
            );
        if !has_feed {
            return;
        }
        let encoded = {
            let Some(Served::Local(local)) = self
                .conns
                .get_mut(&conn_id)
                .and_then(|conn| conn.served.get_mut(&hold))
            else {
                return;
            };
            let Some(feed) = local.feeds.get_mut(&member) else {
                return;
            };
            feed.encoder.encode(&change.ops)
        };
        if self.send_state(
            conn_id,
            StateFrame {
                hold,
                m: member.clone(),
                as_kind: StateAs::Ops,
                ops: Some(encoded),
                ..StateFrame::default()
            },
        ) != SendState::Lazy
        {
            return;
        }
        // The slot turned lazy: the flush sends the current value as a
        // reset; nothing more goes out for it now. The feed's encoder
        // starts over, so ops after the reset encode from it.
        if let Some(Served::Local(local)) = self
            .conns
            .get_mut(&conn_id)
            .and_then(|conn| conn.served.get_mut(&hold))
        {
            if let Some(feed) = local.feeds.get_mut(&member) {
                feed.encoder = Encoder::new();
            }
        }
    }

    /// The consumer starts (`sub`) or stops (`unsub`) the state of a hold.
    fn on_want_state(&mut self, conn_id: ConnId, hold: u64, wanted: bool) {
        let relay = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(|served| match served {
                Served::Relay(relay) => Some(*relay),
                _ => None,
            });
        if let Some(relay) = relay {
            self.relay_want_state(relay, wanted);
            return;
        }
        let object = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(|served| match served {
                Served::Local(local) => Some(local.object.clone()),
                _ => None,
            });
        let Some(object) = object else { return };
        let already = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .is_some_and(|served| matches!(&served, Served::Local(l) if l.wants_state == wanted));
        if already {
            return;
        }
        if let Some(conn) = self.conns.get_mut(&conn_id) {
            if let Some(Served::Local(l)) = conn.served.get_mut(&hold) {
                l.wants_state = wanted;
            }
        }
        if wanted {
            self.send_unit(conn_id, hold);
        } else {
            let members = self
                .conns
                .get(&conn_id)
                .and_then(|conn| conn.served.get(&hold))
                .and_then(|served| match served {
                    Served::Local(local) => Some(local.feeds.keys().cloned().collect::<Vec<_>>()),
                    _ => None,
                })
                .unwrap_or_default();
            for name in members {
                self.detach_feed(conn_id, hold, &object, &name);
            }
            self.drop_state(conn_id, hold, None);
        }
    }

    /// The consumer (a relay that dropped ops) asks for member `m` again as
    /// a reset.
    fn on_resync(&mut self, conn_id: ConnId, hold: u64, m: String) {
        let relay = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.served.get(&hold))
            .and_then(|served| match served {
                Served::Relay(relay) => Some(*relay),
                _ => None,
            });
        if let Some(relay) = relay {
            self.relay_resync_member(relay, m);
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
                        state: shape.state,
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
        // A relayed hold's state: the same member frame goes to the origin,
        // under the origin's hold id.
        let relayed = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.relayed_refs.get(&frame.hold).copied())
            .and_then(|relay| {
                self.relays
                    .get(&relay)
                    .map(|state| (relay, state.origin, state.origin_id))
            });
        if let Some((relay, origin, origin_id)) = relayed {
            let m = frame.m.clone();
            let forwarded = StateFrame {
                hold: origin_id,
                ..frame
            };
            if self.send_state(origin, forwarded) == SendState::Lazy {
                self.relay_resync_member(relay, m);
            }
            return;
        }
        let Some(reference) = self
            .conns
            .get(&conn_id)
            .and_then(|conn| conn.remotes.get(&frame.hold).copied())
        else {
            return;
        };
        self.readable_receive(reference, frame);
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

    /// The next client-origin sequence number (upstream `#originSeq`).
    fn next_origin_seq(&mut self) -> u32 {
        self.origin_seq += 1;
        self.origin_seq
    }

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
        let wants_state = relay.wants_state;
        let again = relay.again;
        let origin_label = relay.origin_label.clone();
        let frame = Frame::Hold {
            id: down_id,
            at: relay.at.clone(),
            token: Token::Value(relay.token.clone()),
            input: relay.input.clone(),
            principal: Some(relay.principal.clone()),
            origin: origin_label,
            state: (!wants_state).then_some(false),
            again: again.then_some(true),
        };
        let queued_calls: Vec<u64> = relay
            .calls
            .iter()
            .filter(|(_, call)| call.down_id.is_none())
            .map(|(origin_id, _)| *origin_id)
            .collect();
        self.send(down, frame);
        for origin_id in queued_calls {
            self.relay_forward_call(relay_id, origin_id);
        }
    }

    /// The origin starts or stops the state of a relayed hold.
    fn relay_want_state(&mut self, relay_id: RelayId, wanted: bool) {
        let Some(relay) = self.relays.get_mut(&relay_id) else {
            return;
        };
        if relay.wants_state == wanted {
            return;
        }
        relay.wants_state = wanted;
        let Some(down) = relay.down else { return };
        let Some(conn) = self.conns.get_mut(&down) else {
            return;
        };
        conn.link.send(&if wanted {
            Frame::Sub {
                hold: relay.down_id,
            }
        } else {
            Frame::Unsub {
                hold: relay.down_id,
            }
        });
    }

    /// The origin could not keep up and dropped what waited: member `m`
    /// goes downstream again as a reset.
    fn relay_resync_member(&mut self, relay_id: RelayId, m: String) {
        let Some(relay) = self.relays.get(&relay_id) else {
            return;
        };
        let Some(down) = relay.down else { return };
        let down_id = relay.down_id;
        let origin = relay.origin;
        let origin_id = relay.origin_id;
        self.drop_state(origin, origin_id, Some(&m));
        self.send(down, Frame::Resync { hold: down_id, m });
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
        let (origin, origin_id, down, down_id) =
            (relay.origin, relay.origin_id, relay.down, relay.down_id);
        for call in calls {
            self.relay_call_cancel(relay_id, call, error.clone(), release_down);
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
        // The consumer normalizes with its own contract before anything is
        // sent: an input it cannot express never reaches the wire (upstream
        // `acquire` throws).
        let input =
            match validate::normalize(&contract.input, &input, &format!("{} input", contract.name))
            {
                Ok(normalized) => normalized,
                Err(error) => {
                    self.ref_gone_nosend(id, error);
                    return;
                }
            };
        // The consumer's own placement decides where the hold goes (upstream
        // `#elsewhere`): never where a caller says it is going. A token
        // without a placement (an old peer's) keeps the caller's `at`.
        let at = match &contract.host {
            Some(placement) => match placement.resolve(&contract.name, &input) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let error = ChordError::invalid("placement", error.to_string());
                    self.ref_gone_nosend(id, error);
                    return;
                }
            },
            None => at,
        };
        // A hold the host's own principal asks for forwards no principal
        // (upstream `isSelf`): it acts as itself, so references of the host
        // and of a caller asking for the host's principal share one binding.
        let principal = principal.filter(|principal| {
            !self
                .self_principal
                .as_ref()
                .is_some_and(|self_principal| self_principal == principal)
        });
        // A binding for the same (contract, input, principal) is shared: a
        // new reference is another view of it (upstream's bindings), and
        // only the last view's release reaches the wire.
        let key = format!(
            "{}\u{0}{}\u{0}p:{}",
            contract.fingerprint(),
            validate::canonical_identity(&contract.input, &input),
            canonical_json(&principal.clone().unwrap_or_else(|| json!({"self": true})))
        );
        // A binding for this key already holds: the new reference is a view
        // of it. Its status mirrors the binding's, and its release ends a
        // view (the binding's handle counts them).
        if let Some((binding, handle)) = self
            .refs
            .iter()
            .find(|(_binding, state)| {
                state.key == key
                    && !matches!(state.status(), RefStatus::Gone(_))
                    && !matches!(state.status(), RefStatus::Released)
            })
            .map(|(binding, state)| (*binding, state.views_handle.clone()))
        {
            handle.fetch_add(1, Ordering::SeqCst);
            self.ref_alias.insert(id, binding);
            self.binding_keys.insert(id, key);
            if let Some(state) = self.refs.get_mut(&binding) {
                let current = state.status();
                status.send_replace(current);
                state.mirrors.push(status);
                if let Some((conn, hold)) = state.binding {
                    if let Some(conn_state) = self.conns.get_mut(&conn) {
                        conn_state.remotes.insert(hold, binding);
                    }
                }
            }
            return;
        }
        let views_handle = Arc::new(AtomicUsize::new(1));
        self.binding_handles.insert(id, views_handle.clone());
        self.binding_keys.insert(id, key.clone());
        self.refs.insert(
            id,
            RemoteState {
                key: key.clone(),
                contract: contract.clone(),
                at: at.clone(),
                input,
                principal,
                status,
                binding: None,
                queued: Vec::new(),
                readables: BTreeMap::new(),
                peer_wants_state: true,
                again: false,
                pending_snapshots: BTreeSet::new(),
                shape: None,
                views_handle,
                mirrors: Vec::new(),
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
        state.peer_wants_state = true;
        // A hold asking for the host's own principal forwards none: it acts
        // as itself (upstream `isSelf`).
        let principal = state.principal.clone().filter(|principal| {
            !self
                .self_principal
                .as_ref()
                .is_some_and(|self_principal| self_principal == principal)
        });
        let frame = Frame::Hold {
            id,
            at: state.at.clone(),
            token: Token::Description(state.contract.clone()),
            input: state.input.clone(),
            principal,
            origin: None,
            state: None,
            again: state.again.then_some(true),
        };
        self.send(conn_id, frame);
    }

    /// Settles a reference that was never sent: an invalid input found
    /// before anything went on the wire.
    fn ref_gone_nosend(&mut self, reference: RefId, error: ChordError) {
        let queued = self
            .refs
            .get_mut(&reference)
            .map(|state| std::mem::take(&mut state.queued));
        let Some(queued) = queued else { return };
        self.set_ref_status(reference, RefStatus::Gone(error.clone()));
        for call in queued {
            let _ = call.reply.send(Err(error.clone()));
        }
        self.refs.remove(&reference);
    }

    fn ref_shaped(&mut self, reference: RefId, shape: Shape) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        if state.settled() {
            return;
        }
        // The reference is live once every promised snapshot is here; a
        // shape without state members hydrates at once.
        let promised = shape.state.clone();
        let ready = promised.is_empty();
        state.pending_snapshots = promised.into_iter().collect();
        state.shape = Some(shape);
        if ready {
            self.hydrate(reference);
        }
    }

    /// The last promised snapshot arrived: the shape's reference goes live.
    fn hydrate(&mut self, reference: RefId) {
        let shape = {
            let Some(state) = self.refs.get(&reference) else {
                return;
            };
            state.shape.clone()
        };
        let Some(shape) = shape else { return };
        if let Some(state) = self.refs.get_mut(&reference) {
            if !state.settled() {
                self.set_ref_status(reference, RefStatus::Live(shape));
            }
        }
        self.ref_live(reference);
    }

    /// The reference became live: its queued calls go out.
    fn ref_live(&mut self, reference: RefId) {
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
        let settled = self.refs.get(&reference).map(|state| state.settled());
        let Some(false) = settled else {
            return;
        };
        let queued = self
            .refs
            .get_mut(&reference)
            .map(|state| std::mem::take(&mut state.queued));
        self.set_ref_status(reference, RefStatus::Gone(error.clone()));
        if let Some(state) = self.refs.get_mut(&reference) {
            state.binding = None;
        }
        let queued = queued.unwrap_or_default();
        for call in queued {
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
        state.again = true;
        if matches!(state.status(), RefStatus::Live(_)) {
            self.set_ref_status(reference, RefStatus::Loading);
        }
    }

    /// Sets a binding's status, mirrored into every view's channel.
    fn set_ref_status(&mut self, reference: RefId, status: RefStatus) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        state.status.send_replace(status.clone());
        for mirror in std::mem::take(&mut state.mirrors) {
            mirror.send_replace(status.clone());
            if let Some(state) = self.refs.get_mut(&reference) {
                if !state.mirrors.iter().any(|m| m.same_channel(&mirror)) {
                    state.mirrors.push(mirror);
                }
            }
        }
    }

    /// The binding a reference addresses: itself, or the binding it
    /// shares.
    fn binding_of(&self, reference: RefId) -> RefId {
        self.ref_alias.get(&reference).copied().unwrap_or(reference)
    }

    fn ref_release(&mut self, reference: RefId) {
        // A view of a shared binding ends here; the binding (and its wire
        // hold) ends when the last view leaves.
        if let Some(binding) = self.ref_alias.get(&reference).copied() {
            self.ref_alias.remove(&reference);
            self.binding_keys.remove(&reference);
            if let Some(handle) = self.binding_handles.get(&binding) {
                if handle.fetch_sub(1, Ordering::SeqCst) > 1 {
                    return;
                }
            }
            self.ref_release_binding(binding);
            return;
        }
        self.ref_release_binding(reference);
    }

    fn ref_release_binding(&mut self, reference: RefId) {
        let settled = self.refs.get(&reference).map(|state| state.settled());
        let Some(false) = settled else {
            return;
        };
        let queued = self
            .refs
            .get_mut(&reference)
            .map(|state| std::mem::take(&mut state.queued));
        self.set_ref_status(reference, RefStatus::Released);
        let released = ChordError::new(ErrorCode::Released, "reference released");
        for call in queued.unwrap_or_default() {
            let _ = call.reply.send(Err(released.clone()));
        }
        if let Some(state) = self.refs.get_mut(&reference) {
            for readable in state.readables.values_mut() {
                readable.sinks.clear();
            }
            if let Some((conn, id)) = state.binding.take() {
                if let Some(conn_state) = self.conns.get_mut(&conn) {
                    conn_state.remotes.remove(&id);
                }
                self.send(conn, Frame::Release { id });
            }
        }
        if let Some(state) = self.refs.get(&reference) {
            let _ = state;
        }
        self.refs.remove(&reference);
        self.binding_handles.remove(&reference);
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

    /// A listener arrived for `member`: the readable exists, and the hold
    /// wants its state (upstream `sub`, once anyone listens).
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
        let readable = state.readables.entry(member.clone()).or_insert(Readable {
            sinks: Vec::new(),
            decoder: Decoder::new(),
            value: None,
        });
        if let Some(value) = &readable.value {
            let _ = sender.send(StateUpdate::Snapshot(value.clone()));
        }
        readable.sinks.push((sink, sender));
        self.want_state(reference, true);
    }

    /// A listener left `member`; when the last one of the hold leaves, the
    /// hold stops wanting state (upstream `unsub`).
    fn ref_unsubscribe(&mut self, reference: RefId, member: &str, sink: u64) {
        let listening = {
            let Some(readable) = self
                .refs
                .get_mut(&reference)
                .and_then(|state| state.readables.get_mut(member))
            else {
                return;
            };
            readable.sinks.retain(|(id, _)| *id != sink);
            self.refs
                .get(&reference)
                .map(|state| state.readables.values().any(|r| !r.sinks.is_empty()))
                .unwrap_or(false)
        };
        if !listening {
            self.want_state(reference, false);
        }
    }

    /// Starts or stops wanting the state of `reference`'s hold: `sub` /
    /// `unsub` on the binding, and the hold's `state` flag when it is
    /// (re)opened.
    /// A reference's wish for state is fixed when it is made and always
    /// wants it (upstream's `RemoteRef` `wantsState`, default true):
    /// listener changes never move the hold's. `sub`/`unsub` exist for
    /// method-only references, which noel does not make; the goldens'
    /// holds all open with state implied, so no scenario shows either
    /// frame.
    fn want_state(&mut self, _reference: RefId, _wanted: bool) {}

    fn readables_stop(&mut self, reference: RefId) {
        let Some(state) = self.refs.get_mut(&reference) else {
            return;
        };
        for readable in state.readables.values_mut() {
            readable.sinks.clear();
        }
        self.want_state(reference, false);
    }

    fn readable_receive(&mut self, reference: RefId, frame: StateFrame) {
        let member = frame.m.clone();
        // A promised snapshot arrived: the reference may hydrate now, with
        // or without a listener for the member.
        let hydrated = {
            let Some(state) = self.refs.get_mut(&reference) else {
                return;
            };
            state.pending_snapshots.remove(&member) && state.pending_snapshots.is_empty()
        };
        if hydrated {
            self.hydrate(reference);
        }
        let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(&member))
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
        } else if let Some(value) = frame.value.clone() {
            readable.decoder = Decoder::new();
            readable.value = Some(value.clone());
            match frame.as_kind {
                StateAs::Reset => StateUpdate::Reset(value),
                _ => StateUpdate::Snapshot(value),
            }
        } else {
            return;
        };
        let Some(readable) = self
            .refs
            .get_mut(&reference)
            .and_then(|state| state.readables.get_mut(&member))
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
    let variant = object.variant.clone().unwrap_or_default();
    Shape {
        variant: object.variant.clone(),
        members: object
            .members
            .iter()
            .map(|(name, _)| name.clone())
            .collect(),
        contract: object.service.fingerprint.clone(),
        state: object
            .members
            .iter()
            .filter(|(name, member)| {
                matches!(member, Member::State(_))
                    && object
                        .service
                        .description
                        .variants
                        .get(&variant)
                        .is_some_and(|members| {
                            matches!(members.get(name), Some(MemberDescription::State { .. }))
                        })
            })
            .map(|(name, _)| name.clone())
            .collect(),
    }
}
