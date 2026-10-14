//! A Chord client over WebSocket framing on a Unix socket: holds the counter
//! at `server`, subscribes to `count`, calls `add` twice, and checks that it
//! sees the provider's snapshot first, then ops.
//!
//! ```text
//! cargo run -p noel-chord --example chord-client -- --socket PATH [--name NAME] [--hold]
//! ```
//!
//! Prints one JSON line per state update and a summary; exits 0 when the
//! updates were the whole value `{"n": 0}` (a snapshot, or a reset when the
//! provider was still activating the object), then ops to 1 and to 3.
//! `--hold` keeps the hold until the process is killed.

#[allow(dead_code)]
#[path = "support/counter.rs"]
mod counter;

use std::process::ExitCode;
use std::time::Duration;

use noel_chord::host::{Host, HostOptions, LinkOptions, StateStream, StateUpdate};
use noel_chord::link::{websocket, WebSocketOptions};
use serde_json::{json, Value};
use tokio::net::UnixStream;

fn update_json(update: &StateUpdate) -> Value {
    match update {
        StateUpdate::Snapshot(value) => json!({"snapshot": value}),
        StateUpdate::Reset(value) => json!({"reset": value}),
        StateUpdate::Ops { ops, value } => {
            json!({"ops": ops.iter().map(|op| op.to_json()).collect::<Vec<_>>(), "value": value})
        }
        StateUpdate::Error(error) => json!({"error": error.to_string()}),
    }
}

async fn next(updates: &mut StateStream) -> Result<StateUpdate, String> {
    tokio::time::timeout(Duration::from_secs(10), updates.next())
        .await
        .map_err(|_| "no state update within 10 s".to_owned())?
        .ok_or_else(|| "the subscription ended".to_owned())
}

async fn journey(socket: &str, name: &str, hold: bool) -> Result<(), String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|error| format!("connect {socket}: {error}"))?;
    let (socket, _) = tokio_tungstenite::client_async("ws://localhost/chord", stream)
        .await
        .map_err(|error| format!("websocket upgrade: {error}"))?;
    let host = Host::new(HostOptions {
        on_error: Some(std::sync::Arc::new(|error| eprintln!("chord: {error}"))),
        ..HostOptions::client("client")
    });
    host.connect(
        websocket(socket, WebSocketOptions::default()),
        LinkOptions::default(),
    );
    let reference = host.get(counter::contract(), json!({"name": name}), "server");
    let mut updates = reference.subscribe("count");
    let shape = reference
        .ready()
        .await
        .map_err(|error| format!("hold: {error}"))?;
    println!(
        "{}",
        json!({"shape": {"variant": shape.variant, "members": shape.members, "contract": shape.contract}})
    );
    let mut seen = Vec::new();
    let first = next(&mut updates).await?;
    println!("{}", update_json(&first));
    seen.push(first);
    let mut results = Vec::new();
    for delta in [1, 2] {
        let result = reference
            .call("add", Some(json!(delta)))
            .await
            .map_err(|error| format!("add: {error}"))?;
        results.push(result.unwrap_or(Value::Null));
        let update = next(&mut updates).await?;
        println!("{}", update_json(&update));
        seen.push(update);
    }
    println!("{}", json!({"results": results}));
    // A provider whose object is still activating when the subscription
    // arrives sends the value as a reset instead (upstream does over a real
    // socket): either way, the whole value comes first.
    let snapshot_first = matches!(
        &seen[0],
        StateUpdate::Snapshot(value) | StateUpdate::Reset(value) if *value == json!({"n": 0})
    );
    let ops_after = matches!(&seen[1], StateUpdate::Ops { value, .. } if *value == json!({"n": 1}))
        && matches!(&seen[2], StateUpdate::Ops { value, .. } if *value == json!({"n": 3}));
    if !(snapshot_first && ops_after && results == [json!(1), json!(3)]) {
        return Err("expected a snapshot of {\"n\":0}, then ops to 1 and 3".to_owned());
    }
    if hold {
        println!("holding until killed");
        std::future::pending::<()>().await;
    }
    reference.release();
    host.close().await;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut socket = None;
    let mut name = "interop".to_owned();
    let mut hold = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--name" => name = args.next().unwrap_or(name),
            "--hold" => hold = true,
            other => {
                eprintln!("chord-client: unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("chord-client: --socket PATH is required");
        return ExitCode::from(2);
    };
    match journey(&socket, &name, hold).await {
        Ok(()) => {
            println!("{}", json!({"ok": true}));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("chord-client: {error}");
            println!("{}", json!({"ok": false, "error": error}));
            ExitCode::FAILURE
        }
    }
}
