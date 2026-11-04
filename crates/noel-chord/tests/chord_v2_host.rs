//! Chord v2 hosts over memory links: a client host holds a server's objects
//! over its parent link. Snapshots and ops, calls and their errors, refused
//! holds and waking them, the state window against a consumer that does not
//! acknowledge, drops and re-holds, release, and cancellation.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use noel_chord::delta::Op;
use noel_chord::v2::channel::{BinaryCodec, Channel, ChannelOptions, Received};
use noel_chord::v2::check::uuidv7;
use noel_chord::v2::host::{HostOptions, LinkOptions, RefStatus, StateUpdate, WINDOW};
use noel_chord::v2::message::{Address, CauseKind, Message, Part};
use noel_chord::v2::object::Cell;
use noel_chord::v2::{
    ChordError, Code, Contract, Host, Implementation, Link, PluginError, WireError,
};
use serde_json::{json, Value};

fn counter() -> Contract {
    Contract::from_json(
        &json!({
            "name": "test.counter",
            "version": 1,
            "host": "session",
            "params": [["session", {"type": "string"}]],
            "variants": {
                "owner": {
                    "state": {"count": {"type": "integer"}},
                    "methods": {
                        "add": {
                            "args": {
                                "type": "object",
                                "required": ["by"],
                                "properties": {"by": {"type": "integer"}},
                                "additionalProperties": false
                            },
                            "result": {"type": "integer"}
                        },
                        "fail": {"args": {"type": "null"}, "result": {"type": "null"}},
                        "wait": {"args": {"type": "null"}, "result": {"type": "null"}}
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap()
}

/// A server serving the counter, with the cell and how many objects ended.
fn server(cell: &Cell, disposed: &Arc<AtomicUsize>) -> Host {
    let host = session_host("s1");
    serve(&host, cell, disposed);
    host
}

fn serve(host: &Host, cell: &Cell, disposed: &Arc<AtomicUsize>) {
    let cell = cell.clone();
    let disposed = disposed.clone();
    host.serve(counter(), move |_params, _principal| {
        let add_cell = cell.clone();
        let disposed = disposed.clone();
        Ok(Implementation::new("owner")
            .state("count", &cell)
            .method("add", move |args: Value, _| {
                let cell = add_cell.clone();
                async move {
                    let by = args["by"].as_i64().unwrap();
                    let value = cell
                        .change(|value| vec![Op::Replace(json!(value.as_i64().unwrap() + by))])
                        .unwrap();
                    Ok((*value).clone())
                }
            })
            .method("fail", |_, _| async {
                Err(WireError::Plugin(PluginError {
                    code: "nope".to_owned(),
                    message: "it failed".to_owned(),
                    details: None,
                }))
            })
            .method(
                "wait",
                |_, context: noel_chord::v2::CallContext| async move {
                    let reason = context.signal.aborted().await;
                    Err(reason.into())
                },
            )
            .on_dispose(move || {
                disposed.fetch_add(1, Ordering::SeqCst);
            }))
    });
}

/// A host at the address of session `id`.
fn session_host(id: &str) -> Host {
    Host::new(HostOptions {
        address: Some(Address {
            kind: "session".to_owned(),
            params: json!({"id": id}),
        }),
        ..HostOptions::default()
    })
}

fn client() -> Host {
    Host::new(HostOptions::default())
}

fn connect(server: &Host, client: &Host) -> (Link, Link) {
    let (up, down) = Link::memory_pair();
    server.accept(down.clone(), LinkOptions::new(json!({"kind": "device"})));
    client.connect(up.clone(), LinkOptions::new(json!({"kind": "server"})));
    (up, down)
}

async fn next(stream: &mut noel_chord::v2::StateStream) -> StateUpdate {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an update in time")
        .expect("an update")
}

async fn status(
    reference: &noel_chord::v2::RemoteRef,
    want: impl Fn(&RefStatus) -> bool,
) -> RefStatus {
    let mut statuses = reference.statuses();
    let found = tokio::time::timeout(Duration::from_secs(5), statuses.wait_for(|s| want(s)))
        .await
        .expect("the status in time")
        .expect("the host")
        .clone();
    found
}

#[tokio::test]
async fn a_client_reads_state_and_calls_methods() {
    let cell = Cell::new(json!({"type": "integer"}), json!(1)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let server = server(&cell, &disposed);
    let client = client();
    connect(&server, &client);

    let counter = client.get(Arc::new(counter()), json!({"session": "s1"}));
    assert_eq!(counter.ready().await.unwrap(), "owner");
    let mut count = counter.subscribe("count");
    assert_eq!(next(&mut count).await, StateUpdate::Snapshot(json!(1)));

    assert_eq!(
        counter.call("add", json!({"by": 2})).await.unwrap(),
        json!(3)
    );
    assert_eq!(
        next(&mut count).await,
        StateUpdate::Ops {
            ops: vec![Op::Replace(json!(3))],
            value: json!(3)
        }
    );
    cell.replace(json!(10)).unwrap();
    assert!(matches!(next(&mut count).await, StateUpdate::Ops { value, .. } if value == json!(10)));

    let error = counter.call("add", json!({"by": "x"})).await.unwrap_err();
    assert!(
        matches!(
            &error,
            WireError::Chord(ChordError {
                code: Code::Invalid,
                reason: Some("args"),
                ..
            })
        ),
        "{error:?}"
    );
    let error = counter.call("fail", Value::Null).await.unwrap_err();
    assert!(matches!(&error, WireError::Plugin(PluginError { code, .. }) if code == "nope"));
    let error = counter.call("missing", Value::Null).await.unwrap_err();
    assert!(matches!(
        &error,
        WireError::Chord(ChordError {
            reason: Some("member"),
            ..
        })
    ));

    counter.release();
    status(&counter, |s| *s == RefStatus::Released).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while disposed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the object ends with its last hold");
}

#[tokio::test]
async fn holds_wait_for_their_type_and_fail_on_another_contract() {
    let cell = Cell::new(json!({"type": "integer"}), json!(5)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let server = session_host("s1");
    let client = client();
    connect(&server, &client);

    let held = client.get(Arc::new(counter()), json!({"session": "s1"}));
    let failed = status(&held, |s| matches!(s, RefStatus::Failed { .. })).await;
    let RefStatus::Failed { cause, error, .. } = failed else {
        unreachable!()
    };
    assert_eq!(cause, CauseKind::Waiting);
    assert_eq!(
        error,
        WireError::Chord(ChordError::new(
            Code::Unavailable,
            "test.counter is not served here"
        ))
    );
    // Served now: the hold binds.
    serve(&server, &cell, &disposed);
    status(&held, |s| matches!(s, RefStatus::Live { .. })).await;

    let mut other = counter();
    other.fingerprint = "0000000000000000".to_owned();
    let mismatched = client.get(Arc::new(other), json!({"session": "s1"}));
    let error = mismatched.ready().await.unwrap_err();
    assert!(matches!(
        error,
        WireError::Chord(ChordError {
            code: Code::ContractMismatch,
            ..
        })
    ));

    // The same address, params the contract refuses.
    let invalid = client.get(Arc::new(counter()), json!({"session": "s1", "extra": 1}));
    let failed = status(&invalid, |s| matches!(s, RefStatus::Failed { .. })).await;
    assert!(matches!(
        failed,
        RefStatus::Failed {
            cause: CauseKind::Broken,
            error: WireError::Chord(ChordError {
                reason: Some("params"),
                ..
            }),
            ..
        }
    ));
}

#[tokio::test]
async fn a_dropped_parent_link_rebinds_on_the_next() {
    let cell = Cell::new(json!({"type": "integer"}), json!(1)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let server = server(&cell, &disposed);
    let client = client();
    let (up, _) = connect(&server, &client);

    let counter = client.get(Arc::new(counter()), json!({"session": "s1"}));
    let mut count = counter.subscribe("count");
    assert_eq!(next(&mut count).await, StateUpdate::Snapshot(json!(1)));
    up.close();
    status(&counter, |s| *s == RefStatus::Loading).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while disposed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the server releases the dropped link's holds");

    cell.replace(json!(7)).unwrap();
    connect(&server, &client);
    assert_eq!(next(&mut count).await, StateUpdate::Snapshot(json!(7)));
    assert_eq!(
        counter.call("add", json!({"by": 1})).await.unwrap(),
        json!(8)
    );
}

#[tokio::test]
async fn dropping_a_call_aborts_it_at_the_provider() {
    let cell = Cell::new(json!({"type": "integer"}), json!(1)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let server = server(&cell, &disposed);
    let client = client();
    connect(&server, &client);
    let counter = client.get(Arc::new(counter()), json!({"session": "s1"}));
    counter.ready().await.unwrap();
    let waiting = tokio::time::timeout(
        Duration::from_millis(100),
        counter.call("wait", Value::Null),
    )
    .await;
    assert!(waiting.is_err(), "the call waits until aborted");
    // The provider's abort ended the call there; the link still works.
    assert_eq!(
        counter.call("add", json!({"by": 1})).await.unwrap(),
        json!(2)
    );
}

/// A raw consumer that does not acknowledge: the provider sends at most
/// `WINDOW` messages of its own, then the pending ops in one `state` per
/// free slot.
#[tokio::test]
async fn the_window_holds_state_until_acknowledged() {
    let cell = Cell::new(json!({"type": "integer"}), json!(0)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let server = server(&cell, &disposed);
    let (raw, down) = Link::memory_pair();
    server.accept(down, LinkOptions::new(json!({"kind": "device"})));
    let mut inbound = raw.take_inbound().unwrap();
    let mut channel = Channel::new(
        BinaryCodec,
        ChannelOptions {
            incarnation: uuidv7(),
            calls: 1,
            trusted: false,
        },
        0,
    );
    let contract = counter();
    channel.send(&Message::Hold {
        origin: None,
        hold: 0,
        type_name: contract.declaration.name.clone(),
        version: 1,
        fingerprint: contract.fingerprint.clone(),
        params: Part::Text(json!({"session": "s1"}).to_string()),
        address: contract.address(&json!({"session": "s1"})),
        principal: None,
        incarnation: None,
        chain: None,
        limit: None,
    });
    let mut received = Vec::new();
    let receive = |channel: &mut Channel, bytes: Vec<u8>| match channel.receive(&bytes, 0).unwrap()
    {
        Received::Message(message) => Some(*message),
        _ => None,
    };
    // Hello, then the binding (one slot).
    while received.is_empty() {
        let bytes = inbound.recv().await.unwrap();
        if let Some(message) = receive(&mut channel, bytes) {
            received.push(message);
        }
        for bytes in channel.outgoing() {
            raw.send(bytes);
        }
    }
    assert_eq!(received[0].kind(), "bound");
    for value in 1..=5 {
        cell.replace(json!(value)).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    while let Ok(Some(bytes)) =
        tokio::time::timeout(Duration::from_millis(200), inbound.recv()).await
    {
        if let Some(message) = receive(&mut channel, bytes) {
            received.push(message);
        }
    }
    // The binding and one state message fill the window; the rest waits.
    assert_eq!(received.len(), WINDOW, "{received:?}");
    channel.send(&Message::Ack { origin: None });
    channel.send(&Message::Ack { origin: None });
    for bytes in channel.outgoing() {
        raw.send(bytes);
    }
    let bytes = tokio::time::timeout(Duration::from_secs(5), inbound.recv())
        .await
        .unwrap()
        .unwrap();
    let Some(Message::State { holds, .. }) = receive(&mut channel, bytes) else {
        panic!("a state message");
    };
    // The pending ops of the commits since, in one entry: each replaced
    // the value, so a replacement drops what was pending before it.
    let Part::Bytes(ops) = &holds[0].members[0].1 else {
        panic!("bytes")
    };
    let ops: Value = serde_json::from_slice(ops).unwrap();
    assert_eq!(ops, json!([["r", 5]]));
}

/// A route to one worker per session: each `resolve` makes a fresh memory
/// link to the worker of that session, if there is one.
struct Workers {
    workers: std::sync::Mutex<std::collections::HashMap<String, Host>>,
    resolves: AtomicUsize,
}

struct WorkerRoute(Arc<Workers>);

impl noel_chord::v2::host::Route for WorkerRoute {
    fn resolve(
        &self,
        address: Address,
    ) -> noel_chord::v2::object::BoxFuture<Result<noel_chord::v2::host::Resolved, ChordError>> {
        self.0.resolves.fetch_add(1, Ordering::SeqCst);
        let id = address.params["id"].as_str().unwrap_or_default().to_owned();
        let worker = self.0.workers.lock().unwrap().get(&id).cloned();
        Box::pin(async move {
            let Some(worker) = worker else {
                return Err(ChordError::new(
                    Code::Unavailable,
                    format!("no worker for {id}"),
                ));
            };
            let (route, parent) = Link::memory_pair();
            worker.connect(parent, LinkOptions::new(json!({"kind": "server"})));
            Ok(noel_chord::v2::host::Resolved {
                link: route,
                options: LinkOptions {
                    trusted: true,
                    ..LinkOptions::new(json!({"kind": "worker", "session": id}))
                },
            })
        })
    }
}

/// A client holds a session's object through a relay that routes it to the
/// session's worker: snapshot, ops and calls cross both links; when the
/// worker's link drops, the client goes loading and binds again over the
/// route's next link.
#[tokio::test]
async fn a_relay_routes_holds_to_workers() {
    let cell = Cell::new(json!({"type": "integer"}), json!(1)).unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let worker = server(&cell, &disposed);
    let workers = Arc::new(Workers {
        workers: std::sync::Mutex::new([("s1".to_owned(), worker.clone())].into()),
        resolves: AtomicUsize::new(0),
    });
    let relay = Host::new(HostOptions {
        address: Some(Address {
            kind: "server".to_owned(),
            params: json!({}),
        }),
        ..HostOptions::default()
    });
    relay.route("session", WorkerRoute(workers.clone()));
    let client = client();
    connect(&relay, &client);

    let held = client.get(Arc::new(counter()), json!({"session": "s1"}));
    assert_eq!(held.ready().await.unwrap(), "owner");
    let mut count = held.subscribe("count");
    assert_eq!(next(&mut count).await, StateUpdate::Snapshot(json!(1)));
    assert_eq!(held.call("add", json!({"by": 4})).await.unwrap(), json!(5));
    assert!(matches!(next(&mut count).await, StateUpdate::Ops { value, .. } if value == json!(5)));
    // More changes than the window: the relay forwards the acks upstream.
    for value in 6..12 {
        cell.replace(json!(value)).unwrap();
    }
    loop {
        if let StateUpdate::Ops { value, .. } = next(&mut count).await {
            if value == json!(11) {
                break;
            }
        }
    }
    let error = held.call("fail", Value::Null).await.unwrap_err();
    assert!(matches!(&error, WireError::Plugin(PluginError { code, .. }) if code == "nope"));

    // The worker's side of the route closes: the route resolves again.
    worker.close().await;
    status(&held, |s| !matches!(s, RefStatus::Live { .. })).await;
    let fresh = server(&cell, &disposed);
    workers
        .workers
        .lock()
        .unwrap()
        .insert("s1".to_owned(), fresh);
    // The first re-resolve may have raced the new worker: retry until live.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(held.status(), RefStatus::Live { .. }) {
                break;
            }
            if matches!(held.status(), RefStatus::Failed { .. }) {
                held.retry();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("live again");
    assert!(workers.resolves.load(Ordering::SeqCst) >= 2);
    assert_eq!(held.call("add", json!({"by": 1})).await.unwrap(), json!(12));

    // A session without a worker: the hold fails waiting with the route's
    // error.
    let other = client.get(Arc::new(counter()), json!({"session": "s2"}));
    let failed = status(&other, |s| matches!(s, RefStatus::Failed { .. })).await;
    assert!(matches!(
        failed,
        RefStatus::Failed {
            cause: CauseKind::Waiting,
            error: WireError::Chord(ChordError {
                code: Code::Unavailable,
                ..
            }),
            ..
        }
    ));
}
