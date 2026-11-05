//! noel's side of `make chord2-interop` (scripts/chord2-interop.sh): a Chord
//! v2 client host that holds `interop.counter` on upstream's server host at a
//! Unix socket. It prints the snapshot, the value after a call, after a burst
//! of commits (more than the window holds), and after its link drops and the
//! reference holds again over a new one (a fresh object upstream).
//!
//! Usage: chord2-client <socket> <counter.json>

use std::sync::Arc;
use std::time::Duration;

use noel_chord::v2::host::{Host, HostOptions, LinkOptions, StateStream, StateUpdate};
use noel_chord::v2::{Contract, Link};
use serde_json::{json, Value};
use tokio::net::UnixStream;

async fn dial(socket: &str) -> Link {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (socket, _) = tokio_tungstenite::client_async("ws://localhost/chord", stream)
        .await
        .expect("websocket upgrade");
    noel_chord::v2::transport::websocket(socket)
}

/// Reads updates until the count is `want`.
async fn until(updates: &mut StateStream, want: i64) {
    loop {
        let update = tokio::time::timeout(Duration::from_secs(20), updates.next())
            .await
            .unwrap_or_else(|_| panic!("no count {want} in time"))
            .expect("updates");
        match update {
            StateUpdate::Snapshot(value) | StateUpdate::Ops { value, .. }
                if value == json!(want) =>
            {
                return
            }
            StateUpdate::Failed(error) => panic!("count failed: {error}"),
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (socket, contract) = (&args[1], &args[2]);
    let contract = Arc::new(
        Contract::from_json(&std::fs::read_to_string(contract).expect("the contract"))
            .expect("declares"),
    );
    let host = Host::new(HostOptions::default());
    let server = || LinkOptions::new(json!({"kind": "server"}));
    let link = dial(socket).await;
    host.connect(link.clone(), server());
    let counter = host.get(contract, json!({}));
    let mut count = counter.subscribe("count");
    let first = tokio::time::timeout(Duration::from_secs(20), count.next())
        .await
        .expect("a snapshot in time")
        .expect("updates");
    match first {
        StateUpdate::Snapshot(value) => println!("snapshot {value}"),
        other => panic!("not a snapshot: {other:?}"),
    }
    counter.call("add", json!({"by": 5})).await.expect("add");
    until(&mut count, 5).await;
    println!("after add 5");
    counter
        .call("burst", json!({"times": 200}))
        .await
        .expect("burst");
    until(&mut count, 205).await;
    println!("after burst 205");

    link.close();
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.connect(dial(socket).await, server());
    // The reference binds again with a fresh snapshot: upstream ended the
    // object with its last hold, so it starts over.
    let again = loop {
        let update = tokio::time::timeout(Duration::from_secs(20), count.next())
            .await
            .expect("a snapshot after the re-hold")
            .expect("updates");
        if let StateUpdate::Snapshot(value) = update {
            break value.as_i64().expect("a count");
        }
    };
    let added: Value = counter
        .call("add", json!({"by": 1}))
        .await
        .expect("add again");
    assert_eq!(added, Value::Null);
    until(&mut count, again + 1).await;
    println!("reheld {again} then {}", again + 1);
    host.close().await;
}
