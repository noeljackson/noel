//! noel-ffi over a byte channel, as the iOS app drives it: the test's
//! `ChannelOpener` connects a duplex stream to a server host that accepts
//! the WebSocket link, serves `noel.sessions` and `noel.plugins`, and routes
//! the session to a session host serving `noel.chat`. The listener hears
//! the directory, the plugins and the session's view; a send reaches
//! `submit`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use noel_chord::v2::host::{Host, HostOptions, LinkOptions, Resolved, Route};
use noel_chord::v2::message::Address;
use noel_chord::v2::object::{BoxFuture, Cell, Implementation};
use noel_chord::v2::{ChordError, Link};
use noel_client::{contracts, server_address, session_address};
use noel_ffi::{ByteChannel, ChannelOpener, Delivery, Noel, NoelEvent, NoelListener};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Recorder(Mutex<Vec<NoelEvent>>);

impl NoelListener for Recorder {
    fn on_event(&self, event: NoelEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Writes into the server's end of a duplex stream.
struct Pipe(tokio::sync::mpsc::UnboundedSender<Option<Vec<u8>>>);

impl ByteChannel for Pipe {
    fn send(&self, bytes: Vec<u8>) {
        let _ = self.0.send(Some(bytes));
    }

    fn close(&self) {
        let _ = self.0.send(None);
    }
}

/// Opens channels to `server`: each a duplex stream whose other end the
/// server accepts as a WebSocket.
struct Opener {
    server: Host,
    runtime: tokio::runtime::Handle,
}

impl ChannelOpener for Opener {
    fn open(&self, delivery: Arc<Delivery>) -> Option<Arc<dyn ByteChannel>> {
        let (app_end, server_end) = tokio::io::duplex(1 << 16);
        let server = self.server.clone();
        self.runtime.spawn(async move {
            let socket = tokio_tungstenite::accept_async(server_end).await.unwrap();
            server.accept(
                noel_chord::v2::transport::websocket(socket),
                LinkOptions::new(json!({"kind": "owner", "via": "tunnel"})),
            );
        });
        let (mut reader, mut writer) = tokio::io::split(app_end);
        let (sending, mut outbound) = tokio::sync::mpsc::unbounded_channel::<Option<Vec<u8>>>();
        self.runtime.spawn(async move {
            while let Some(Some(bytes)) = outbound.recv().await {
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        self.runtime.spawn(async move {
            let mut buffer = vec![0u8; 1 << 16];
            loop {
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => delivery.receive(buffer[..read].to_vec()),
                }
            }
            delivery.closed();
        });
        Some(Arc::new(Pipe(sending)))
    }
}

/// The server's route to the session host.
struct ToSession(Host);

impl Route for ToSession {
    fn resolve(&self, _address: Address) -> BoxFuture<Result<Resolved, ChordError>> {
        let session = self.0.clone();
        Box::pin(async move {
            let (ours, theirs) = Link::memory_pair();
            session.connect(theirs, LinkOptions::new(json!({"kind": "server"})));
            Ok(Resolved {
                link: ours,
                options: LinkOptions::new(json!({"kind": "worker", "session": "s1"})),
            })
        })
    }
}

fn state_schema(contract: &noel_chord::v2::Contract, variant: &str, member: &str) -> Value {
    contract.declaration.variants[variant].state[member].clone()
}

#[test]
fn the_app_follows_sessions_plugins_and_a_view_over_its_channel() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let (server, _session) = runtime.block_on(async {
        let server = Host::new(HostOptions {
            address: Some(server_address()),
            ..HostOptions::default()
        });
        let directory = Cell::new(
            state_schema(&contracts::sessions(), "owner", "directory"),
            json!({"sessions": [{"sessionId": "s1", "name": "first", "cwd": "/w", "modified": 1,
                "messageCount": 1, "live": {"worker": true, "busy": false, "viewers": 0}}]}),
        )
        .unwrap();
        server.serve(contracts::sessions(), move |_, _| {
            Ok(Implementation::new("owner").state("directory", &directory))
        });
        let plugins = Cell::new(
            state_schema(&contracts::plugins(), "owner", "directory"),
            json!({"plugins": [{"name": "sample", "running": true, "restarts": 0,
                "requested": [], "granted": [],
                "contributions": {"tools": [], "commands": [{"name": "count", "description": "Counts."}],
                "panels": [{"id": "counter", "title": "Counter", "view": {"type": "text", "text": "count 0"}}]}}]}),
        )
        .unwrap();
        server.serve(contracts::plugins(), move |_, _| {
            Ok(Implementation::new("owner").state("directory", &plugins))
        });
        let session = Host::new(HostOptions {
            address: Some(session_address("s1")),
            ..HostOptions::default()
        });
        let view = Cell::new(
            state_schema(&contracts::chat(), "participant", "view"),
            json!({"conversation": {"id": "s1", "cwd": "/w"}, "docs": {},
                "entries": [{"id": 1, "kind": "pi.user", "model": [{"role": "user", "content": "hi"}]}]}),
        )
        .unwrap();
        let recorded = submitted.clone();
        session.serve(contracts::chat(), move |_, _| {
            let recorded = recorded.clone();
            Ok(Implementation::new("participant")
                .state("view", &view)
                .method("submit", move |args: Value, _| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(args["text"].as_str().unwrap_or_default().to_owned());
                        Ok(json!({"submissionId": "x", "disposition": "started"}))
                    }
                })
                .method("entries", |_, _| async { Ok(json!({"entries": []})) }))
        });
        server.route("session", ToSession(session.clone()));
        (server, session)
    });

    let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
    let noel = Noel::connect(
        Arc::new(Opener {
            server,
            runtime: runtime.handle().clone(),
        }),
        recorder.clone(),
    );
    let wait = |what: &str, found: &dyn Fn(&[NoelEvent]) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if found(&recorder.0.lock().unwrap()) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {what}: {:?}",
                recorder.0.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait("sessions", &|events| {
        events.iter().any(|event| {
            matches!(event, NoelEvent::Sessions { sessions } if sessions.len() == 1 && sessions[0].title == "first")
        })
    });
    wait("plugins", &|events| {
        events.iter().any(|event| matches!(event, NoelEvent::Plugins { plugins }
            if plugins[0].commands == ["count"] && plugins[0].panels[0].view_json.contains("count 0")))
    });
    noel.select("s1".into());
    wait("the view", &|events| {
        events.iter().any(|event| matches!(event, NoelEvent::View { session, entries, .. }
            if session == "s1" && entries.len() == 1 && entries[0].text == "hi" && entries[0].label == "you"))
    });
    noel.send("hello from swift".into());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while submitted.lock().unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no submit");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(submitted.lock().unwrap()[0], "hello from swift");
}
