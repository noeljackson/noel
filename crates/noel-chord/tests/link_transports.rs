//! noel-chord's transports end to end: WebSocket framing over a Unix socket,
//! packed binary frames (decoded when accepted, refused from clients), NDJSON
//! process links with control lines, and what a noel worker answers to
//! reload control.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::write::DeflateEncoder;
use flate2::Compression;
use futures_util::{SinkExt, StreamExt};
use noel_chord::delta::{Op, Seg};
use noel_chord::host::{AcceptOptions, Host, HostOptions, LinkOptions, StateUpdate};
use noel_chord::link::{ndjson, websocket, WebSocketOptions};
use noel_chord::object::{Cell, Implementation};
use noel_chord::{Description, Frame, Link, MemberDescription, Placement, Transport};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;

/// Where this file's tally lives: the tests that serve it on the server say
/// so; the ndjson worker test overrides it.
fn where_tally_lives() -> &'static str {
    "server"
}

fn tally(host: &str) -> Description {
    let count =
        json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}});
    Description::remote(
        "noel.test.tally",
        1,
        json!({"type": "object", "properties": {}}),
        Placement::Named {
            name: host.to_string(),
        },
        [(
            "main",
            vec![
                ("count", MemberDescription::state(count)),
                (
                    "add",
                    MemberDescription::method(
                        Some(json!({"type": "integer"})),
                        Some(json!({"type": "integer"})),
                    ),
                ),
            ],
        )],
    )
    .expect("a valid contract")
}

fn serve_tally(host: &Host) {
    serve_tally_on(host, where_tally_lives());
}

fn serve_tally_on(host: &Host, at: &str) {
    host.serve(tally(at), |_input, _principal| {
        let count = Cell::new(
            json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}),
            json!({"n": 0}),
        )?;
        let add = count.clone();
        Ok(Implementation::new("main")
            .state("count", &count)
            .method("add", move |args, _ctx| {
                let count = add.clone();
                async move {
                    let delta = args.as_ref().and_then(Value::as_i64).unwrap_or(0);
                    let value = count.change(|value| {
                        vec![Op::Set(
                            vec![Seg::Key("n".to_owned())],
                            json!(value["n"].as_i64().unwrap_or(0) + delta),
                        )]
                    })?;
                    Ok(value["n"].clone())
                }
            }))
    });
}

fn owner() -> Value {
    json!({"kind": "owner", "via": "device"})
}

async fn websocket_pair() -> (
    tokio_tungstenite::WebSocketStream<UnixStream>,
    tokio_tungstenite::WebSocketStream<UnixStream>,
) {
    let (server, client) = UnixStream::pair().expect("a socket pair");
    let accepting = tokio::spawn(async move {
        tokio_tungstenite::accept_async(server)
            .await
            .expect("accepted")
    });
    let (client, _) = tokio_tungstenite::client_async("ws://localhost/chord", client)
        .await
        .expect("upgraded");
    (accepting.await.expect("joined"), client)
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("within 10 s")
}

#[tokio::test]
async fn a_client_holds_subscribes_and_calls_over_websocket_framing_on_a_unix_socket() {
    let (server_socket, client_socket) = websocket_pair().await;
    let server = Host::new(HostOptions::host("server"));
    serve_tally(&server);
    server.accept(
        websocket(
            server_socket,
            WebSocketOptions {
                accept_packed: false,
            },
        ),
        AcceptOptions::client(owner()),
    );
    let client = Host::new(HostOptions::client("client"));
    client.connect(
        websocket(client_socket, WebSocketOptions::default()),
        LinkOptions::default(),
    );
    let tally = client.get(tally(where_tally_lives()), json!({}), "server");
    let mut updates = tally.subscribe("count");
    let shape = within(tally.ready()).await.expect("live");
    assert_eq!(shape.members, ["count", "add"]);
    assert_eq!(
        within(updates.next()).await,
        Some(StateUpdate::Snapshot(json!({"n": 0})))
    );
    assert_eq!(
        within(tally.call("add", Some(json!(2)))).await,
        Ok(Some(json!(2)))
    );
    match within(updates.next()).await {
        Some(StateUpdate::Ops { value, .. }) => assert_eq!(value, json!({"n": 2})),
        other => panic!("expected ops, got {other:?}"),
    }
    client.close().await;
    server.close().await;
}

fn packed(envelope: Value, payload: &str) -> Vec<u8> {
    let envelope = envelope.to_string();
    let mut message = (envelope.len() as u32).to_be_bytes().to_vec();
    message.extend_from_slice(envelope.as_bytes());
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(payload.as_bytes()).unwrap();
    message.extend(encoder.finish().unwrap());
    message
}

async fn next_frame(socket: &mut tokio_tungstenite::WebSocketStream<UnixStream>, t: &str) -> Value {
    loop {
        match within(socket.next()).await {
            Some(Ok(Message::Text(text))) => {
                let frame: Value = serde_json::from_str(text.as_str()).unwrap();
                if frame["t"] == t {
                    return frame;
                }
            }
            Some(Ok(_)) => {}
            other => panic!("the socket ended waiting for {t}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn packed_binary_frames_are_decoded_when_accepted_and_close_the_link_when_not() {
    let hello =
        json!({"t": "hello", "protocol": 4, "name": "peer", "kind": "client", "codeVersion": 1});
    let hold = json!({"t": "hold", "id": 1, "at": "server", "token": tally(where_tally_lives()).to_value(), "input": {}});
    let call = packed(
        json!({"t": "call", "id": 2, "hold": 1, "member": "add", "z": "args"}),
        "5",
    );
    for accept_packed in [true, false] {
        let (server_socket, mut peer) = websocket_pair().await;
        let server = Host::new(HostOptions {
            on_error: Some(Arc::new(|_| {})),
            ..HostOptions::host("server")
        });
        serve_tally(&server);
        server.accept(
            websocket(server_socket, WebSocketOptions { accept_packed }),
            AcceptOptions::client(owner()),
        );
        peer.send(Message::text(hello.to_string())).await.unwrap();
        peer.send(Message::text(hold.to_string())).await.unwrap();
        next_frame(&mut peer, "shape").await;
        peer.send(Message::binary(call.clone())).await.unwrap();
        if accept_packed {
            assert_eq!(
                next_frame(&mut peer, "result").await,
                json!({"t": "result", "id": 2, "value": 5})
            );
        } else {
            assert_eq!(
                next_frame(&mut peer, "bye").await,
                json!({"t": "bye", "reason": "protocol error"})
            );
        }
        server.close().await;
    }
}

#[tokio::test]
async fn an_ndjson_link_joins_a_server_and_its_worker_and_carries_control_lines() {
    let (server_side, worker_side) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (worker_read, worker_write) = tokio::io::split(worker_side);
    let (control_sender, mut control) = tokio::sync::mpsc::unbounded_channel();
    let (to_worker, _) = ndjson(server_read, server_write, Some(control_sender));
    let (to_server, worker_control) = ndjson(worker_read, worker_write, None);
    let worker = Host::new(HostOptions::host("session:w"));
    serve_tally_on(&worker, "session:w");
    worker.connect(to_server, LinkOptions::default());
    assert!(worker_control.send(&json!({"type": "ready"})));
    assert_eq!(within(control.recv()).await, Some(json!({"type": "ready"})));
    let server = Host::new(HostOptions::host("server"));
    server.accept(
        to_worker,
        AcceptOptions::host(
            "session:w",
            json!({"kind": "worker", "session": "w", "generation": 1}),
        ),
    );
    let tally = server.get(tally("session:w"), json!({}), "session:w");
    within(tally.ready()).await.expect("live");
    assert_eq!(
        within(tally.call("add", Some(json!(4)))).await,
        Ok(Some(json!(4)))
    );
    server.close().await;
    worker.close().await;
}

/// Captures what a host sends on a link.
struct Capture(Arc<Mutex<Vec<Value>>>);

impl Transport for Capture {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        self.0.lock().unwrap().push(frame.to_value());
        None
    }

    fn shutdown(&self) {}
}

#[tokio::test]
async fn a_noel_worker_votes_no_to_a_reload_and_ignores_commits() {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let mut parent = None;
    let link = Link::new(|delivery| {
        parent = Some(delivery);
        Capture(sent.clone())
    });
    let parent = parent.unwrap();
    let worker = Host::new(HostOptions::host("session:w"));
    worker.connect(link, LinkOptions::default());
    parent.deliver(
        json!({"t": "hello", "protocol": 4, "name": "server", "kind": "host", "codeVersion": 1}),
    );
    parent.deliver(
        json!({"t": "prepare", "reload": 7, "codeVersion": 2, "plugins": [], "options": {}}),
    );
    parent.deliver(json!({"t": "commit", "reload": 7}));
    parent.deliver(json!({"t": "ping", "n": 1}));
    for _ in 0..100 {
        if sent
            .lock()
            .unwrap()
            .iter()
            .any(|frame| frame["t"] == "pong")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sent = sent.lock().unwrap().clone();
    let vote = sent
        .iter()
        .find(|frame| frame["t"] == "vote")
        .expect("a vote");
    assert_eq!(vote["reload"], 7);
    assert_eq!(vote["error"]["code"], "unavailable");
    assert_eq!(vote["error"]["reason"], "not_coordinator");
    assert!(
        sent.iter().all(|frame| frame["t"] != "bye"),
        "the link stays open: {sent:?}"
    );
}
