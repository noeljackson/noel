//! noel-chord's link against upstream Chord's
//! (`tests/fixtures/ts-parity/chord-link.json`, made by
//! `scripts/ts-parity/chord-link-fixture.ts`): the same scenarios, with the
//! same contracts, over tapped in-memory links, and the frames each side
//! sends on each link compared with upstream's.
//!
//! Within one link and direction, frames are compared in order and exactly,
//! ids included. Ping and pong `n`, error messages and the random part of
//! root ids are placeholders. `ack` frames are not recorded (their timing is
//! the scheduler's); the flow scenarios hold the client's back and compare
//! the one they release.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use noel_chord::delta::{Op, Seg};
use noel_chord::host::{AcceptOptions, Host, HostOptions, LinkOptions, PrincipalMatch, Route};
use noel_chord::object::{BoxFuture, Cell, Implementation};
use noel_chord::{
    canonical_json, ChordError, Delivery, Description, Frame, Link, StateUpdate, Transport,
};
use serde::Deserialize;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/chord-link.json");
const MAKEFILE: &str = include_str!("../../../Makefile");

#[derive(Deserialize)]
struct Fixture {
    contracts: Contracts,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Contracts {
    counter: Description,
    counter_drifted: Description,
    counter_v2: Description,
    missing: Description,
    session: Description,
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the fixture is JSON")
}

fn contracts() -> Contracts {
    // Parsed from the text, so variants and members keep upstream's order.
    serde_json::from_str::<Fixture>(FIXTURE)
        .expect("contracts parse")
        .contracts
}

// ─── Recording ───────────────────────────────────────────────────────────────

type Log = Arc<Mutex<Vec<(String, String, Value)>>>;

/// One end of a tapped in-memory link: records what it sends (but `ack`),
/// and can hold back its acknowledgements.
struct TapEnd {
    link: String,
    side: String,
    log: Log,
    peer: OnceLock<Delivery>,
    paused: AtomicBool,
    held: Mutex<Option<Frame>>,
    /// A closed end sends nowhere and records nothing.
    closed: AtomicBool,
}

impl TapEnd {
    fn transmit_value(&self, frame: Value) -> usize {
        if self.closed.load(Ordering::SeqCst) {
            return frame.to_string().encode_utf16().count();
        }
        let text = frame.to_string();
        if frame["t"] != "ack" {
            self.log
                .lock()
                .unwrap()
                .push((self.link.clone(), self.side.clone(), frame.clone()));
        }
        if let Some(peer) = self.peer.get() {
            peer.deliver(frame);
        }
        text.encode_utf16().count()
    }

    /// Sends the held acknowledgement, if any.
    fn release(&self) -> Option<Frame> {
        self.paused.store(false, Ordering::SeqCst);
        let held = self.held.lock().unwrap().take();
        if let Some(frame) = &held {
            self.transmit_value(frame.to_value());
        }
        held
    }
}

struct TapTransport(Arc<TapEnd>);

impl Transport for TapTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        if matches!(frame, Frame::Ack { .. }) && self.0.paused.load(Ordering::SeqCst) {
            *self.0.held.lock().unwrap() = Some(frame.clone());
            return Some(0);
        }
        Some(
            self.0
                .transmit_value(serde_json::from_str(&frame.to_text()).unwrap()),
        )
    }

    fn shutdown(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
        if let Some(peer) = self.0.peer.get() {
            peer.end("peer closed");
        }
    }
}

/// A tapped pair: `(a's end, b's end)`, with handles to both taps.
fn tap_pair(log: &Log, link: &str, a: &str, b: &str) -> ((Link, Arc<TapEnd>), (Link, Arc<TapEnd>)) {
    let end = |side: &str| {
        Arc::new(TapEnd {
            link: link.to_owned(),
            side: side.to_owned(),
            log: log.clone(),
            peer: OnceLock::new(),
            paused: AtomicBool::new(false),
            held: Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    };
    let (left_end, right_end) = (end(a), end(b));
    let mut left_delivery = None;
    let mut right_delivery = None;
    let left = Link::new(|delivery| {
        left_delivery = Some(delivery);
        TapTransport(left_end.clone())
    });
    let right = Link::new(|delivery| {
        right_delivery = Some(delivery);
        TapTransport(right_end.clone())
    });
    let _ = left_end.peer.set(right_delivery.unwrap());
    let _ = right_end.peer.set(left_delivery.unwrap());
    ((left, left_end), (right, right_end))
}

// ─── Serving ─────────────────────────────────────────────────────────────────

fn kind(principal: &Value) -> &str {
    principal.get("kind").and_then(Value::as_str).unwrap_or("")
}

fn serve_counter(host: &Host, contract: &Description) {
    let schema = match contract
        .variants
        .get("owner")
        .and_then(|members| members.get("count"))
    {
        Some(noel_chord::MemberDescription::State { schema }) => schema.clone(),
        _ => panic!("the counter has a count"),
    };
    host.serve(contract.clone(), move |input, principal| {
        let name = input["name"].as_str().unwrap_or("").to_owned();
        let who = kind(principal);
        if who != "owner" && who != "guest" {
            return Err(ChordError::forbidden(format!(
                "{name} is not for this principal"
            )));
        }
        if who != "owner" && name.starts_with("private") {
            return Err(ChordError::forbidden(format!("{name} is private")));
        }
        let count = Cell::new(schema.clone(), json!({"n": 0}))?;
        if who != "owner" {
            return Ok(Implementation::new("viewer").state("count", &count));
        }
        let (add, set) = (count.clone(), count.clone());
        Ok(Implementation::new("owner")
            .state("count", &count)
            .method("add", move |args, _ctx| {
                let count = add.clone();
                async move {
                    let delta = args.as_ref().and_then(Value::as_i64).unwrap_or(0);
                    let value = count.change(|value| {
                        let n = value["n"].as_i64().unwrap_or(0) + delta;
                        vec![Op::Set(vec![Seg::Key("n".to_owned())], json!(n))]
                    })?;
                    Ok(value["n"].clone())
                }
            })
            .method("set", move |args, _ctx| {
                let count = set.clone();
                async move {
                    count.replace(json!({"n": args.unwrap_or(Value::Null)}))?;
                    Ok(Value::Null)
                }
            })
            .method("wait", |_args, ctx| async move {
                Err(ctx.signal.aborted().await)
            })
            .method("fail", |_args, _ctx| async {
                Err(ChordError::unavailable("boom"))
            }))
    });
}

fn serve_session(host: &Host, contract: &Description) {
    host.serve(contract.clone(), |input, principal| {
        let session = input["session"].as_str().unwrap_or("").to_owned();
        let who = kind(principal);
        let allowed = who == "owner" || (who == "guest" && principal["session"] == session);
        if !allowed {
            return Err(ChordError::forbidden(format!(
                "session {session} is not for this principal"
            )));
        }
        let log = Cell::new(
            json!({"type": "array", "items": {"type": "string"}}),
            json!([]),
        )?;
        if who == "guest" && principal["mode"] == "view" {
            return Ok(Implementation::new("viewer").state("log", &log));
        }
        let say = log.clone();
        let principal = principal.clone();
        Ok(Implementation::new("participant")
            .state("log", &log)
            .method("say", move |args, _ctx| {
                let log = say.clone();
                async move {
                    let value = log.change(|value| {
                        let length = value.as_array().map(Vec::len).unwrap_or(0) as u64;
                        vec![Op::Splice(
                            Vec::new(),
                            length,
                            0,
                            vec![args.unwrap_or(Value::Null)],
                        )]
                    })?;
                    Ok(json!(value.as_array().map(Vec::len).unwrap_or(0)))
                }
            })
            .method("who", move |_args, _ctx| {
                let principal = principal.clone();
                async move { Ok(principal) }
            })
            .method("wait", |_args, ctx| async move {
                Err(ctx.signal.aborted().await)
            }))
    });
}

/// The server's `session:` route: spawns a worker host over a tapped link.
struct Sessions {
    log: Log,
    contract: Description,
    workers: Mutex<HashMap<String, Link>>,
    hosts: Arc<Mutex<Vec<Host>>>,
}

impl Route for Sessions {
    fn authorize(&self, principal: &Value, host_name: &str) -> BoxFuture<bool> {
        let allowed = kind(principal) == "owner"
            || (kind(principal) == "guest"
                && principal["session"]
                    .as_str()
                    .map(|session| format!("session:{session}"))
                    .as_deref()
                    == Some(host_name));
        Box::pin(async move { allowed })
    }

    fn resolve(&self, host_name: &str) -> BoxFuture<Result<Link, ChordError>> {
        let mut workers = self.workers.lock().unwrap();
        let link = workers
            .entry(host_name.to_owned())
            .or_insert_with(|| {
                let ((down, _), (up, _)) = tap_pair(&self.log, "worker", "server", "worker");
                let worker = Host::new(quiet(HostOptions::host(host_name)));
                serve_session(&worker, &self.contract);
                worker.connect(up, LinkOptions::default());
                self.hosts.lock().unwrap().push(worker);
                down
            })
            .clone();
        Box::pin(async move { Ok(link) })
    }

    fn principal(&self, host_name: &str) -> Value {
        json!({"kind": "worker", "session": host_name.trim_start_matches("session:"), "generation": 1})
    }
}

fn quiet(options: HostOptions) -> HostOptions {
    HostOptions {
        on_error: Some(Arc::new(|_| {})),
        ..options
    }
}

fn owner() -> Value {
    json!({"kind": "owner", "via": "device"})
}

fn guest(session: &str, mode: &str) -> Value {
    json!({"kind": "guest", "id": "g1", "name": "Guest", "session": session, "mode": mode, "revision": 1})
}

/// Fresh hosts for one scenario.
struct World {
    log: Log,
    contracts: Contracts,
    clients: Vec<Host>,
    servers: Vec<Host>,
    workers: Arc<Mutex<Vec<Host>>>,
    /// Subscriptions stay until the hosts close, as upstream's listeners do.
    streams: Vec<noel_chord::StateStream>,
}

type Configure = Box<dyn FnOnce(&mut AcceptOptions)>;

#[derive(Default)]
struct ClientOptions {
    link: Option<&'static str>,
    accept: Option<Configure>,
    upstream: LinkOptions,
}

impl World {
    fn new() -> World {
        World {
            log: Arc::new(Mutex::new(Vec::new())),
            contracts: contracts(),
            clients: Vec::new(),
            servers: Vec::new(),
            workers: Arc::new(Mutex::new(Vec::new())),
            streams: Vec::new(),
        }
    }

    fn server(&mut self) -> Host {
        let host = Host::new(quiet(HostOptions::host("server")));
        serve_counter(&host, &self.contracts.counter);
        host.route(
            "session:",
            Sessions {
                log: self.log.clone(),
                contract: self.contracts.session.clone(),
                workers: Mutex::new(HashMap::new()),
                hosts: self.workers.clone(),
            },
        );
        self.servers.push(host.clone());
        host
    }

    /// A client host accepted by `server` as `principal`; also its tap.
    fn client(
        &mut self,
        server: &Host,
        principal: Value,
        options: ClientOptions,
    ) -> (Host, Arc<TapEnd>) {
        let name = options.link.unwrap_or("client");
        let ((down, _), (up, tap)) = tap_pair(&self.log, name, "server", "client");
        let mut accept = AcceptOptions::client(principal);
        if let Some(configure) = options.accept {
            configure(&mut accept);
        }
        server.accept(down, accept);
        let host = Host::new(quiet(HostOptions {
            self_principal: Some(owner()),
            ..HostOptions::client("client")
        }));
        host.connect(up, options.upstream);
        self.clients.push(host.clone());
        (host, tap)
    }

    /// A bare link end accepted by `server`.
    fn raw(&mut self, server: &Host, link: &str, accept: AcceptOptions) -> (Link, Arc<TapEnd>) {
        let ((down, _), (up, tap)) = tap_pair(&self.log, link, "server", "raw");
        server.accept(down, accept);
        (up, tap)
    }

    async fn finish(self) -> Vec<(String, String, Value)> {
        settle().await;
        for host in self.clients.iter().chain(&self.servers) {
            host.close().await;
            settle().await;
        }
        let workers: Vec<Host> = self.workers.lock().unwrap().clone();
        for host in workers {
            host.close().await;
            settle().await;
        }
        drop(self.streams);
        let log = self.log.lock().unwrap().clone();
        log
    }
}

/// Lets in-memory traffic finish. Time is paused, so this returns once every
/// task is idle.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(25)).await;
}

fn send_raw(tap: &TapEnd, frame: Value) {
    tap.transmit_value(frame);
}

// ─── Scenarios ───────────────────────────────────────────────────────────────

async fn run(name: &str, gates: &mut Vec<(String, Value)>) -> Vec<(String, String, Value)> {
    let mut world = World::new();
    let c = world.contracts.clone();
    match name {
        "hello" => {
            let s = world.server();
            world.client(&s, owner(), ClientOptions::default());
        }
        "hold" => {
            let s = world.server();
            let (client, _) = world.client(&s, guest("s1", "full"), ClientOptions::default());
            let a = client.get(c.counter.clone(), json!({"name": "a"}), "server");
            a.ready().await.expect("live");
            let refs = [
                client.get(c.counter.clone(), json!({"name": "private"}), "server"),
                client.get(c.missing.clone(), json!({}), "server"),
                client.get(c.counter_drifted.clone(), json!({"name": "a"}), "server"),
                client.get(c.counter_v2.clone(), json!({"name": "a"}), "server"),
                client.get(c.counter.clone(), json!({"name": 5}), "server"),
            ];
            for reference in refs {
                assert!(reference.ready().await.is_err());
            }
            a.release();
            settle().await;
        }
        "state" => {
            let s = world.server();
            let (client, _) = world.client(&s, owner(), ClientOptions::default());
            let a = client.get(c.counter.clone(), json!({"name": "a"}), "server");
            let stream = a.subscribe("count");
            a.ready().await.expect("live");
            settle().await;
            for (method, value) in [("add", 1), ("add", 2), ("add", 3), ("set", 10), ("add", 1)] {
                a.call(method, Some(json!(value))).await.expect("called");
            }
            settle().await;
            drop(stream);
            settle().await;
            world.streams.push(a.subscribe("count"));
            settle().await;
            a.release();
            settle().await;
        }
        "call" => {
            let s = world.server();
            let (client, _) = world.client(&s, owner(), ClientOptions::default());
            let a = client.get(c.counter.clone(), json!({"name": "a"}), "server");
            a.ready().await.expect("live");
            assert_eq!(a.call("add", Some(json!(5))).await, Ok(Some(json!(5))));
            assert_eq!(a.call("set", Some(json!(7))).await, Ok(None));
            let error = a.call("add", Some(json!("x"))).await.unwrap_err();
            assert_eq!(error.reason.as_deref(), Some("args"));
            assert!(a.call("fail", None).await.is_err());
            settle().await;
        }
        "cancel" => {
            let s = world.server();
            let options = ClientOptions {
                accept: Some(Box::new(|accept| accept.max_calls = Some(1))),
                ..ClientOptions::default()
            };
            let (client, _) = world.client(&s, owner(), options);
            let a = client.get(c.counter.clone(), json!({"name": "a"}), "server");
            world.streams.push(a.subscribe("count"));
            a.ready().await.expect("live");
            settle().await;
            let waiting = tokio::spawn({
                let a = a.clone();
                async move { a.call("wait", None).await }
            });
            settle().await;
            let refused = a.call("add", Some(json!(1))).await.unwrap_err();
            assert_eq!(refused.reason.as_deref(), Some("too_many_calls"));
            settle().await;
            waiting.abort();
            settle().await;
            a.release();
            settle().await;
        }
        "relay" => {
            let s = world.server();
            let (client, _) = world.client(&s, owner(), ClientOptions::default());
            let session = client.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
            world.streams.push(session.subscribe("log"));
            session.ready().await.expect("live");
            settle().await;
            assert_eq!(
                session.call("say", Some(json!("hi"))).await,
                Ok(Some(json!(1)))
            );
            assert_eq!(session.call("who", None).await, Ok(Some(owner())));
            settle().await;
            let waiting = tokio::spawn({
                let session = session.clone();
                async move { session.call("wait", None).await }
            });
            settle().await;
            waiting.abort();
            settle().await;
            session.release();
            settle().await;
        }
        "refused" => {
            let s = world.server();
            let (client, _) = world.client(&s, guest("s1", "full"), ClientOptions::default());
            let refused = client.get(c.session.clone(), json!({"session": "s2"}), "session:s2");
            assert_eq!(
                refused.ready().await.unwrap_err().code,
                noel_chord::ErrorCode::Forbidden
            );
            // Upstream's scenario: a first get of s1 (whose ref stays
            // alive), then the same binding as the host's own principal —
            // the two share one hold, so one release leaves it open.
            let first = client.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
            first.ready().await.expect("live");
            let forged = client.get_as(
                c.session.clone(),
                json!({"session": "s1"}),
                "session:s1",
                owner(),
            );
            forged.ready().await.expect("live");
            assert_eq!(
                forged.call("who", None).await,
                Ok(Some(guest("s1", "full")))
            );
            settle().await;
            forged.release();
            // `first` still holds the binding: no release reaches the wire,
            // exactly as upstream's scenario (the host's close ends it).
            drop(first);
            settle().await;
        }
        "invalidate" => {
            let s = world.server();
            let restamp = ClientOptions {
                accept: Some(Box::new(|accept| {
                    accept.restamp = Some(Arc::new(|principal: &Value| {
                        let mut next = principal.clone();
                        next["revision"] = json!(2);
                        Some(next)
                    }))
                })),
                ..ClientOptions::default()
            };
            let (kept, _) = world.client(&s, guest("s1", "full"), restamp);
            let closing = ClientOptions {
                link: Some("client2"),
                ..ClientOptions::default()
            };
            let (closed, _) = world.client(&s, guest("s1", "view"), closing);
            let a = kept.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
            world.streams.push(a.subscribe("log"));
            a.ready().await.expect("live");
            let b = closed.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
            b.ready().await.expect("live");
            settle().await;
            let pattern = json!({"kind": "guest", "id": "g1"})
                .as_object()
                .unwrap()
                .clone();
            s.invalidate_principal(PrincipalMatch::Pattern(pattern))
                .await;
            settle().await;
            let again = kept.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
            let mut revised = guest("s1", "full");
            revised["revision"] = json!(2);
            assert_eq!(again.call("who", None).await, Ok(Some(revised)));
            settle().await;
        }
        "ping" => {
            let s = world.server();
            let fast = LinkOptions {
                ping_interval: Duration::from_millis(100),
                ..LinkOptions::default()
            };
            let options = ClientOptions {
                accept: Some(Box::new({
                    let fast = fast.clone();
                    move |accept| accept.link = fast
                })),
                upstream: fast,
                ..ClientOptions::default()
            };
            world.client(&s, owner(), options);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        "ping-timeout" => {
            let s = world.server();
            let mut accept = AcceptOptions::client(guest("s1", "full"));
            accept.link = LinkOptions {
                ping_interval: Duration::from_millis(100),
                ping_timeout: Duration::from_millis(250),
                ..LinkOptions::default()
            };
            let (_raw, tap) = world.raw(&s, "raw", accept);
            send_raw(
                &tap,
                json!({"t": "hello", "protocol": 2, "name": "raw", "kind": "client", "codeVersion": 1}),
            );
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        "flow-coalesce" | "flow-reset" => {
            let (window, adds) = if name == "flow-coalesce" {
                (200, 40)
            } else {
                (64, 100)
            };
            let s = world.server();
            let options = ClientOptions {
                accept: Some(Box::new(move |accept| accept.link.window = Some(window))),
                ..ClientOptions::default()
            };
            let (client, tap) = world.client(&s, owner(), options);
            let a = client.get(c.counter.clone(), json!({"name": "a"}), "server");
            world.streams.push(a.subscribe("count"));
            a.ready().await.expect("live");
            settle().await;
            tap.paused.store(true, Ordering::SeqCst);
            for _ in 0..adds {
                a.call("add", Some(json!(1))).await.expect("added");
            }
            settle().await;
            let released = tap
                .release()
                .map(|frame| frame.to_value())
                .unwrap_or(Value::Null);
            gates.push((name.to_owned(), released));
            settle().await;
        }
        "version-old-peer" => {
            let s = world.server();
            let (_raw, tap) = world.raw(&s, "raw", AcceptOptions::client(guest("s1", "full")));
            send_raw(
                &tap,
                json!({"t": "hello", "name": "old", "kind": "client", "codeVersion": 1}),
            );
            settle().await;
        }
        "version-new-server" => {
            let ((_server_end, tap), (up, _)) = tap_pair(&world.log, "raw", "server", "client");
            let client = Host::new(quiet(HostOptions::client("client")));
            client.connect(up, LinkOptions::default());
            world.clients.push(client);
            send_raw(
                &tap,
                json!({"t": "hello", "protocol": 3, "name": "server", "kind": "host", "codeVersion": 1, "acks": true}),
            );
            settle().await;
        }
        "violations" => {
            let s = world.server();
            let hello = json!({"t": "hello", "protocol": 2, "name": "raw", "kind": "client", "codeVersion": 1});
            let token = serde_json::to_value(&c.counter).unwrap();
            let accept = || AcceptOptions::client(guest("s1", "full"));
            let (_r1, malformed) = world.raw(&s, "raw1", accept());
            send_raw(&malformed, hello.clone());
            send_raw(&malformed, json!({"t": "call", "hold": 1}));
            settle().await;
            let (_r2, duplicate) = world.raw(&s, "raw2", accept());
            send_raw(&duplicate, hello.clone());
            send_raw(
                &duplicate,
                json!({"t": "hold", "id": 1, "at": "server", "token": token, "input": {"name": "a"}}),
            );
            settle().await;
            send_raw(
                &duplicate,
                json!({"t": "hold", "id": 1, "at": "server", "token": token, "input": {"name": "b"}}),
            );
            settle().await;
            let (_r3, control) = world.raw(&s, "raw3", accept());
            send_raw(&control, hello.clone());
            send_raw(&control, json!({"t": "invalidate", "pattern": {}}));
            settle().await;
            let (_r4, host) = world.raw(&s, "raw4", accept());
            let mut as_host = hello.clone();
            as_host["kind"] = json!("host");
            as_host["name"] = json!("session:x");
            send_raw(&host, as_host);
            settle().await;
            let (_r5, ack) = world.raw(&s, "raw5", accept());
            send_raw(&ack, hello.clone());
            send_raw(&ack, json!({"t": "ack", "n": 5}));
            settle().await;
        }
        other => panic!("no scenario {other}"),
    }
    world.finish().await
}

// ─── Comparison ──────────────────────────────────────────────────────────────

/// Placeholders for what may differ: ping and pong `n`, error messages, and
/// the random part of root ids.
fn normalize(frame: &Value) -> Value {
    let mut frame = frame.clone();
    if frame["t"] == "ping" || frame["t"] == "pong" {
        frame["n"] = json!("<n>");
    }
    if let Some(error) = frame.get_mut("error").and_then(Value::as_object_mut) {
        error.insert("message".to_owned(), json!("<message>"));
    }
    let root = |id: &mut Value| {
        if let Some(text) = id.as_str() {
            if let (Some(hash), Some(colon)) = (text.find('#'), text.rfind(':')) {
                *id = json!(format!("{}#*{}", &text[..hash], &text[colon..]));
            }
        }
    };
    if let Some(id) = frame.get_mut("rootId") {
        root(id);
    }
    // A hold's origin names its client connection: `<incarnation>:<seq>`,
    // random per run; the incarnation is masked, the sequence kept.
    if let Some(origin) = frame.get_mut("origin") {
        let masked = origin
            .as_str()
            .and_then(|text| text.split_once(':'))
            .map(|(_, rest)| format!("<origin>:{rest}"))
            .unwrap_or_else(|| "<origin>".to_owned());
        *origin = json!(masked);
    }
    if let Some(id) = frame
        .get_mut("root")
        .and_then(|root| root.get_mut("rootId"))
    {
        root(id);
    }
    frame
}

type Streams = BTreeMap<(String, String), Vec<Value>>;

fn streams<'a>(frames: impl Iterator<Item = (&'a str, &'a str, &'a Value)>) -> Streams {
    let mut streams = Streams::new();
    for (link, from, frame) in frames {
        streams
            .entry((link.to_owned(), from.to_owned()))
            .or_default()
            .push(normalize(frame));
    }
    streams
}

fn show(streams: &Streams) -> String {
    let mut out = String::new();
    for ((link, from), frames) in streams {
        out.push_str(&format!("  {link} from {from}:\n"));
        for frame in frames {
            let mut frame = frame.clone();
            if let Some(token) = frame.get_mut("token") {
                *token = json!(format!(
                    "<{}@{}>",
                    token["name"].as_str().unwrap_or("?"),
                    token["version"]
                ));
            }
            out.push_str(&format!("    {frame}\n"));
        }
    }
    out
}

#[tokio::test(start_paused = true)]
async fn a_second_client_after_a_release_gets_the_state_again() {
    let mut world = World::new();
    let c = world.contracts.clone();
    let s = world.server();
    let (first, _) = world.client(&s, owner(), ClientOptions::default());
    let a = first.get(c.counter.clone(), json!({"name": "a"}), "server");
    let mut stream = a.subscribe("count");
    a.ready().await.expect("live");
    // Let a change flow.
    a.call("add", Some(json!(1))).await.expect("added");
    settle().await;
    let _ = stream.next().await;
    // The first client leaves.
    a.release();
    drop(stream);
    settle().await;
    // A second client holds the same object: its snapshot must arrive.
    let (second, _) = world.client(&s, owner(), ClientOptions::default());
    let b = second.get(c.counter.clone(), json!({"name": "a"}), "server");
    let mut updates = b.subscribe("count");
    b.ready().await.expect("live again");
    let first_value = tokio::time::timeout(std::time::Duration::from_secs(5), updates.next())
        .await
        .expect("a snapshot arrives for the second client");
    assert!(
        first_value.is_some(),
        "the second client's state never arrived"
    );
    world.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_second_client_after_a_release_gets_the_state_again_over_a_route() {
    let mut world = World::new();
    let c = world.contracts.clone();
    let s = world.server();
    let (first, _) = world.client(&s, owner(), ClientOptions::default());
    let a = first.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
    let mut stream = a.subscribe("log");
    a.ready().await.expect("live");
    a.call("say", Some(json!("hi"))).await.expect("said");
    settle().await;
    let _ = stream.next().await;
    a.release();
    drop(stream);
    settle().await;
    let (second, _) = world.client(&s, owner(), ClientOptions::default());
    let b = second.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
    let mut updates = b.subscribe("log");
    b.ready().await.expect("live again");
    let first_value = tokio::time::timeout(std::time::Duration::from_secs(5), updates.next())
        .await
        .expect("a snapshot arrives for the second client over the route");
    assert!(
        first_value.is_some(),
        "the second client's state never arrived"
    );
    world.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_watcher_keeps_receiving_state_after_a_caller_leaves() {
    let mut world = World::new();
    let c = world.contracts.clone();
    let s = world.server();
    // The watcher holds first, before anything happens.
    let (watcher, _) = world.client(&s, owner(), ClientOptions::default());
    let watched = watcher.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
    let mut updates = watched.subscribe("log");
    watched.ready().await.expect("live");
    // A short-lived caller holds the same object, calls, and leaves.
    let (caller, _) = world.client(&s, owner(), ClientOptions::default());
    let chat = caller.get(c.session.clone(), json!({"session": "s1"}), "session:s1");
    chat.call("say", Some(json!("one"))).await.expect("said");
    chat.release();
    // The caller host stays alive until the end of the test; only its
    // reference is released, as a short-lived client process would leave.
    settle().await;
    // The watcher still receives the change.
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), updates.next())
        .await
        .expect("the watcher hears the change after the caller left");
    assert!(
        matches!(
            next,
            Some(StateUpdate::Ops { .. })
                | Some(StateUpdate::Snapshot(_))
                | Some(StateUpdate::Reset(_))
        ),
        "watcher heard nothing: {next:?}"
    );
    world.finish().await;
}

#[tokio::test(start_paused = true)]
async fn every_scenario_sends_the_frames_upstream_sends() {
    let fixture = fixture();
    let scenarios = fixture["scenarios"].as_array().expect("scenarios");
    assert!(scenarios.len() >= 15, "{} scenarios", scenarios.len());
    let mut gates = Vec::new();
    let mut failures = Vec::new();
    for scenario in scenarios {
        let name = scenario["name"].as_str().expect("a name");
        // `CHORD_LINK_SCENARIO=name` runs one scenario while debugging.
        if std::env::var("CHORD_LINK_SCENARIO").is_ok_and(|only| only != name) {
            continue;
        }
        let expected = streams(scenario["frames"].as_array().expect("frames").iter().map(
            |entry| {
                (
                    entry["link"].as_str().unwrap(),
                    entry["from"].as_str().unwrap(),
                    &entry["frame"],
                )
            },
        ));
        let recorded = run(name, &mut gates).await;
        let mut actual = streams(
            recorded
                .iter()
                .map(|(link, from, frame)| (link.as_str(), from.as_str(), frame)),
        );
        let mut expected = expected;
        if name == "ping" {
            // Both sides' timers fire together: a side's own ping and its pong
            // to the other's may go out in either order.
            for frames in expected.values_mut().chain(actual.values_mut()) {
                frames.sort_by_key(canonical_json);
            }
        }
        if actual != expected {
            failures.push(format!(
                "scenario {name}\nexpected (upstream):\n{}actual (noel):\n{}",
                show(&expected),
                show(&actual)
            ));
        }
    }
    let expected_gates: Vec<(String, Value)> = fixture["gate"]
        .as_array()
        .expect("gate")
        .iter()
        .map(|gate| {
            (
                gate["scenario"].as_str().unwrap().to_owned(),
                gate["released"].clone(),
            )
        })
        .collect();
    if std::env::var("CHORD_LINK_SCENARIO").is_err() && gates != expected_gates {
        failures.push(format!(
            "released acknowledgements: expected {expected_gates:?}, got {gates:?}"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_fixture_comes_from_the_pinned_commit() {
    let fixture = fixture();
    let pin = MAKEFILE
        .lines()
        .find_map(|line| line.strip_prefix("CHORD_REFERENCE_REF ?= "))
        .expect("the Makefile pins Chord");
    assert_eq!(
        fixture["source"]["ref"], pin,
        "regenerate chord-link.json after moving the pin"
    );
}

#[test]
fn the_fixture_covers_every_scenario_the_link_needs() {
    let fixture = fixture();
    let names: Vec<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scenario| scenario["name"].as_str().unwrap())
        .collect();
    for wanted in [
        "hello",
        "hold",
        "state",
        "call",
        "cancel",
        "relay",
        "refused",
        "invalidate",
        "ping",
        "ping-timeout",
        "flow-coalesce",
        "flow-reset",
        "version-old-peer",
        "version-new-server",
        "violations",
    ] {
        assert!(names.contains(&wanted), "{wanted} is missing");
    }
    let kinds: std::collections::BTreeSet<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scenario| scenario["frames"].as_array().unwrap())
        .map(|entry| entry["frame"]["t"].as_str().unwrap())
        .collect();
    for wanted in [
        "hello",
        "hold",
        "shape",
        "gone",
        "release",
        "call",
        "result",
        "cancel",
        "state",
        "ping",
        "pong",
        "bye",
        "root_end",
        "invalidate",
    ] {
        assert!(kinds.contains(wanted), "no {wanted} frame recorded");
    }
}
