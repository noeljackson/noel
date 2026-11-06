//! A client of `noel server` over Chord, shared by every front end: line-mode
//! `noel client`, the TUI attached to a session (`noel --attach`), and native
//! apps through bindings.
//!
//! - [`contracts`]: the server's contracts, `noel.sessions`, `noel.chat` and
//!   `noel.models`, as the server serves them.
//! - [`Connector`]: how a link to the server is opened. The crate opens no
//!   sockets and starts no processes itself; a front end supplies the
//!   transport (the local socket, `ssh … noel bridge`, an SSH channel from
//!   an iOS app) and [`websocket_link`] speaks Chord over any byte stream.
//! - [`Client`]: one connection, reconnected through its connector when the
//!   link drops, with `noel.chat` calls retried while a worker starts and a
//!   session's view followed across lost workers and lost servers.
//!
//! It depends on no Unix sockets, processes or terminal, so it builds for
//! targets such as iOS.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use noel_chord::v2::host::{
    Host, HostEvent, HostOptions, LinkOptions, RefStatus, RemoteRef, StateStream, StateUpdate,
};
use noel_chord::v2::message::Address;
use noel_chord::v2::object::BoxFuture;
use noel_chord::v2::{Code, Link, WireError};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Notify;

pub mod app;

pub mod contracts {
    //! The contracts `noel server` serves, and its workers (Chord v2).

    use std::sync::{Arc, OnceLock};

    use noel_chord::v2::Contract;

    const CHAT: &str = include_str!("../contracts/v2/noel.chat.json");
    const SESSIONS: &str = include_str!("../contracts/v2/noel.sessions.json");
    const MODELS: &str = include_str!("../contracts/v2/noel.models.json");

    fn declared(cell: &'static OnceLock<Arc<Contract>>, text: &str) -> Arc<Contract> {
        cell.get_or_init(|| Arc::new(Contract::from_json(text).expect("the contract declares")))
            .clone()
    }

    /// `noel.chat`, served by the worker at `Session({id})`.
    pub fn chat() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, CHAT)
    }

    /// `noel.sessions`, served by `Server()`.
    pub fn sessions() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, SESSIONS)
    }

    /// `noel.plugin`, served by each plugin at `Plugin({name})`.
    pub fn plugin() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, include_str!("../contracts/v2/noel.plugin.json"))
    }

    /// `noel.plugins`, served by `Server()`: the plugins and their
    /// contributions.
    pub fn plugins() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, include_str!("../contracts/v2/noel.plugins.json"))
    }

    /// `noel.devices`, served by `Server()`: paired devices (spec §16).
    pub fn devices() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, include_str!("../contracts/v2/noel.devices.json"))
    }

    /// `noel.accounts`, served by `Server()`: the server machine's provider
    /// accounts, and logins relayed to a client.
    pub fn accounts() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, include_str!("../contracts/v2/noel.accounts.json"))
    }

    /// `noel.settings`, served by `Server()`: the server machine's user
    /// settings.
    pub fn settings() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, include_str!("../contracts/v2/noel.settings.json"))
    }

    /// `noel.models`, served by `Server()`.
    pub fn models() -> Arc<Contract> {
        static CELL: OnceLock<Arc<Contract>> = OnceLock::new();
        declared(&CELL, MODELS)
    }
}

/// The server's host address, `Server()`.
pub fn server_address() -> Address {
    Address {
        kind: "server".to_owned(),
        params: json!({}),
    }
}

/// A plugin's host address, `Plugin({name})`.
pub fn plugin_address(name: &str) -> Address {
    Address {
        kind: "plugin".to_owned(),
        params: json!({ "name": name }),
    }
}

/// A session's worker's host address, `Session({id})`.
pub fn session_address(session: &str) -> Address {
    Address {
        kind: "session".to_owned(),
        params: json!({ "id": session }),
    }
}

/// A random id (a version 4 UUID) for a `requestId`.
pub fn new_request_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system randomness");
    bytes[6] = 0x40 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Where a client reports what it does on its own: starting a server,
/// reconnecting, sending a call again. Line mode prints it; the TUI shows
/// it in its transcript.
pub type Report = Arc<dyn Fn(String) + Send + Sync>;

/// Opens links to the server.
pub trait Connector: Send + Sync + 'static {
    /// A new link to the server. `interactive` is true only for the first
    /// link: a reconnect must not ask anyone anything (a password, an
    /// unknown host key). What it does on its own goes to `report`.
    fn open(&self, interactive: bool, report: Report) -> BoxFuture<Result<Link>>;
}

/// A Chord link over `stream`: the WebSocket upgrade the server expects,
/// then frames. `stream` is any byte stream to the server's socket: the
/// socket itself, or a tunnel to it such as `ssh <host> noel bridge`.
pub async fn websocket_link<S>(stream: S) -> Result<Link>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (socket, _) = tokio_tungstenite::client_async("ws://localhost/chord", stream)
        .await
        .map_err(|error| anyhow!("websocket upgrade: {error}"))?;
    Ok(noel_chord::v2::transport::websocket(socket))
}

/// What following a session's view ([`Client::follow`]) yields.
pub enum Followed {
    /// The view: its snapshot (again after each hold), then its changes.
    Update(StateUpdate),
    /// The link to the server dropped; reconnecting.
    ServerLost,
    /// Connected again; the view is held again.
    Reconnected,
    /// The hold ended (its worker was lost), for this reason; holding again.
    ViewEnded(String),
}

/// A connection to the server, reconnected through its connector when it
/// drops.
#[derive(Clone)]
pub struct Client {
    host: Host,
    connector: Arc<dyn Connector>,
    lost: Arc<Notify>,
    report: Report,
}

impl Client {
    pub async fn connect(connector: Arc<dyn Connector>, report: Report) -> Result<Self> {
        let lost = Arc::new(Notify::new());
        let signal = lost.clone();
        let host = Host::new(HostOptions::default());
        host.observe(move |event| match event {
            HostEvent::Closed { .. } => signal.notify_one(),
            HostEvent::ProtocolError { error, .. }
                if std::env::var_os("NOEL_CLIENT_DEBUG").is_some() =>
            {
                eprintln!("noel client: chord: {error}")
            }
            _ => {}
        });
        host.connect(connector.open(true, report.clone()).await?, server_link());
        Ok(Self {
            host,
            connector,
            lost,
            report,
        })
    }

    /// Closes the connection, releasing what it holds.
    pub async fn close(&self) {
        self.host.close().await;
    }

    /// Connects again after the server was lost, until it succeeds.
    async fn reconnect(&self) {
        let mut delay = Duration::from_millis(100);
        loop {
            match self.connector.open(false, self.report.clone()).await {
                Ok(link) => {
                    self.host.connect(link, server_link());
                    return;
                }
                Err(error) => (self.report)(format!("reconnect: {error:#}")),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// A reference on the server's `noel.sessions`.
    pub fn sessions(&self) -> RemoteRef {
        self.host.get(contracts::sessions(), json!({}))
    }

    /// A reference on the server's `noel.models`.
    pub fn models(&self) -> RemoteRef {
        self.host.get(contracts::models(), json!({}))
    }

    /// Calls a method of a server object (`noel.accounts`, `noel.settings`,
    /// `noel.sessions`, …) and lets the reference go.
    pub async fn server_call(
        &self,
        contract: Arc<noel_chord::v2::Contract>,
        method: &str,
        args: Value,
    ) -> Result<Value> {
        let reference = self.host.get(contract, json!({}));
        let outcome = call(&reference, method, args).await;
        reference.release();
        outcome
    }

    /// A server object's state `name` as it is now.
    pub async fn server_state(
        &self,
        contract: Arc<noel_chord::v2::Contract>,
        name: &str,
    ) -> Result<Value> {
        let reference = self.host.get(contract, json!({}));
        let value = current(&reference, name).await;
        reference.release();
        value
    }

    /// A reference on a session's `noel.chat`.
    pub fn chat(&self, session: &str) -> RemoteRef {
        self.host
            .get(contracts::chat(), json!({ "session": session }))
    }

    /// A reference on the server's `noel.plugins`.
    pub fn plugins(&self) -> RemoteRef {
        self.host.get(contracts::plugins(), json!({}))
    }

    /// The server's plugins as they are now.
    pub async fn plugin_directory(&self) -> Result<Value> {
        let plugins = self.plugins();
        let value = current(&plugins, "directory").await;
        plugins.release();
        value
    }

    /// Calls a `noel.plugins` method (`runCommand`, `panelAction`).
    pub async fn plugins_call(&self, method: &str, args: Value) -> Result<Value> {
        let plugins = self.plugins();
        let outcome = call(&plugins, method, args).await;
        plugins.release();
        outcome
    }

    /// The server's paired devices as they are now (`noel.devices`).
    pub async fn device_directory(&self) -> Result<Value> {
        let devices = self.host.get(contracts::devices(), json!({}));
        let value = current(&devices, "devices").await;
        devices.release();
        value
    }

    /// Calls a `noel.devices` method (`approve`, `revoke`).
    pub async fn devices_call(&self, method: &str, args: Value) -> Result<Value> {
        let devices = self.host.get(contracts::devices(), json!({}));
        let outcome = call(&devices, method, args).await;
        devices.release();
        outcome
    }

    /// Calls a `noel.sessions` method (`create`, `fork`).
    pub async fn sessions_call(&self, method: &str, args: Value) -> Result<Value> {
        let sessions = self.sessions();
        let outcome = call(&sessions, method, args).await;
        sessions.release();
        outcome
    }

    /// Calls a `noel.chat` method. With `retry`, a call or hold that fails
    /// `unavailable` is made again, with the same arguments, a few times.
    pub async fn chat_call(
        &self,
        session: &str,
        method: &str,
        args: Value,
        retry: bool,
    ) -> Result<Value> {
        let mut delay = Duration::from_millis(200);
        let mut attempt = 1;
        loop {
            let chat = self.chat(session);
            let outcome = call(&chat, method, args.clone()).await;
            chat.release();
            match outcome {
                Err(error)
                    if retry
                        && attempt < 10
                        && error
                            .downcast_ref::<WireError>()
                            .is_some_and(|error| error.is(Code::Unavailable)) =>
                {
                    (self.report)(format!("{method}: {error}; sending again ({attempt})"));
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                outcome => return outcome,
            }
        }
    }

    /// The server's session directory as it is now.
    pub async fn directory(&self) -> Result<Value> {
        let sessions = self.sessions();
        let value = current(&sessions, "directory").await;
        sessions.release();
        value
    }

    /// The session's view as it is now.
    pub async fn view(&self, session: &str) -> Result<Value> {
        let chat = self.chat(session);
        let value = current(&chat, "view").await;
        chat.release();
        value
    }

    /// Follows the session's view into `sink`: its snapshot, then each
    /// change. It holds the view again when the worker is lost, and
    /// reconnects when the server is. It ends only when the first hold
    /// fails, with that error.
    pub async fn follow(&self, session: &str, mut sink: impl FnMut(Followed)) -> anyhow::Error {
        let mut first = true;
        loop {
            let chat = self.chat(session);
            let mut updates = chat.subscribe("view");
            match chat.ready().await {
                Ok(_) => {}
                Err(error) if error.is(Code::Unavailable) && !first => {
                    (self.report)(format!("watch: {error}; holding again"));
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                Err(error) => return error.into(),
            }
            first = false;
            let mut statuses = chat.statuses();
            let mut live = true;
            loop {
                tokio::select! {
                    update = updates.next() => match update {
                        Some(update) => sink(Followed::Update(update)),
                        None => break,
                    },
                    // The hold left its binding (its worker was lost): a
                    // snapshot follows once it binds again.
                    changed = statuses.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let status = statuses.borrow_and_update().clone();
                        match status {
                            RefStatus::Live { .. } => live = true,
                            RefStatus::Loading | RefStatus::Failed { .. } if live => {
                                live = false;
                                let why = match status {
                                    RefStatus::Failed { error, .. } => error.to_string(),
                                    _ => "its worker was lost".to_owned(),
                                };
                                sink(Followed::ViewEnded(why));
                            }
                            _ => {}
                        }
                    }
                    () = self.lost.notified() => {
                        sink(Followed::ServerLost);
                        self.reconnect().await;
                        sink(Followed::Reconnected);
                    }
                }
            }
            let why = match chat.status() {
                RefStatus::Gone(error) => error.to_string(),
                other => format!("{other:?}"),
            };
            sink(Followed::ViewEnded(why));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// A session's history as a client holds it (Durable's
/// `ConversationTranscript`): every entry it has seen in a view's window or
/// loaded with `noel.chat`'s `entries`, by id. The view holds only the
/// newest entries; [`Transcript::load_earlier`] pages back from its window.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: BTreeMap<u64, Value>,
}

impl Transcript {
    /// Takes the entries of `view`'s window and returns every entry held,
    /// oldest first.
    pub fn merge(&mut self, view: &Value) -> Vec<Value> {
        self.add(&view["entries"]);
        self.entries.values().cloned().collect()
    }

    fn add(&mut self, entries: &Value) {
        for entry in entries.as_array().into_iter().flatten() {
            if let Some(id) = entry["id"].as_u64() {
                self.entries.insert(id, entry.clone());
            }
        }
    }

    /// Loads the entries before `view`'s window that this transcript lacks,
    /// paging back until it meets an entry it holds or the first one.
    pub async fn load_earlier(
        &mut self,
        client: &Client,
        session: &str,
        view: &Value,
    ) -> Result<()> {
        let mut next = view["entries"]
            .get(0)
            .and_then(|entry| entry["id"].as_u64());
        while let Some(before) = next {
            if before <= 1 || self.entries.contains_key(&(before - 1)) {
                break;
            }
            let page = client
                .chat_call(session, "entries", json!({ "before": before }), false)
                .await?;
            let ids: Vec<u64> = page["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| entry["id"].as_u64())
                .collect();
            self.add(&page["entries"]);
            next = match (ids.first(), page["before"].as_u64()) {
                (Some(first), Some(_)) => Some(*first),
                _ => None,
            };
        }
        Ok(())
    }
}

/// The parent link to the server, as this client stamps it.
fn server_link() -> LinkOptions {
    LinkOptions::new(json!({ "kind": "server" }))
}

/// Calls `method` on `reference` once it is ready.
pub async fn call(reference: &RemoteRef, method: &str, args: Value) -> Result<Value> {
    reference.ready().await?;
    Ok(reference.call(method, args).await?)
}

/// The first value of the state member `member`: its snapshot.
async fn current(reference: &RemoteRef, member: &str) -> Result<Value> {
    let mut updates: StateStream = reference.subscribe(member);
    reference.ready().await?;
    loop {
        match tokio::time::timeout(Duration::from_secs(30), updates.next()).await {
            Ok(Some(StateUpdate::Snapshot(value))) => return Ok(value),
            Ok(Some(StateUpdate::Failed(error))) => return Err(error.into()),
            Ok(Some(StateUpdate::Ops { .. })) => {}
            Ok(None) => return Err(anyhow!("{member} ended")),
            Err(_) => return Err(anyhow!("no {member} within 30 s")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noel_chord::v2::object::{Cell, Implementation};
    use noel_chord::v2::{ChordError, Contract};
    use std::sync::Mutex;

    #[test]
    fn contracts_parse_with_their_names() {
        assert_eq!(contracts::chat().declaration.name, "noel.chat");
        assert_eq!(contracts::sessions().declaration.name, "noel.sessions");
        assert_eq!(contracts::models().declaration.name, "noel.models");
    }

    #[test]
    fn request_ids_are_v4_uuids() {
        let id = new_request_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_ne!(id, new_request_id());
    }

    /// A connector that opens in-memory links to `server`, recording each
    /// open's `interactive` and keeping the links so a test can drop one.
    struct Memory {
        server: Host,
        opens: Mutex<Vec<(bool, Link)>>,
    }

    impl Memory {
        fn new(server: &Host) -> Arc<Self> {
            Arc::new(Memory {
                server: server.clone(),
                opens: Mutex::new(Vec::new()),
            })
        }
    }

    impl Connector for Memory {
        fn open(&self, interactive: bool, _report: Report) -> BoxFuture<Result<Link>> {
            let (ours, theirs) = Link::memory_pair();
            self.server.accept(
                theirs,
                LinkOptions::new(json!({"kind": "owner", "via": "device"})),
            );
            self.opens.lock().unwrap().push((interactive, ours.clone()));
            Box::pin(async move { Ok(ours) })
        }
    }

    async fn next(seen: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("in time")
            .expect("following")
    }

    fn quiet() -> Report {
        Arc::new(|_| {})
    }

    fn state_schema(contract: &Contract, variant: &str, member: &str) -> Value {
        contract.declaration.variants[variant].state[member].clone()
    }

    fn host_at(address: Address) -> Host {
        Host::new(HostOptions {
            address: Some(address),
            ..HostOptions::default()
        })
    }

    #[tokio::test]
    async fn a_client_reads_the_directory_and_creates_sessions() {
        let server = host_at(server_address());
        let directory = Cell::new(
            state_schema(&contracts::sessions(), "owner", "directory"),
            json!({"sessions": []}),
        )
        .unwrap();
        server.serve(contracts::sessions(), move |_input, _principal| {
            let cell = directory.clone();
            Ok(Implementation::new("owner")
                .state("directory", &directory)
                .method("create", move |args, _ctx| {
                    let cell = cell.clone();
                    async move {
                        let id = args["requestId"].clone();
                        cell.replace(json!({"sessions": [{
                            "sessionId": id, "cwd": args["cwd"], "modified": 1,
                            "messageCount": 0,
                            "live": {"worker": false, "busy": false, "viewers": 0},
                        }]}))
                        .map_err(|error| WireError::internal(error.to_string()))?;
                        Ok(json!({"sessionId": id, "created": true}))
                    }
                })
                .method("fork", |_args, _ctx| async {
                    Err(ChordError::new(Code::Unavailable, "no forks").into())
                }))
        });
        let client = Client::connect(Memory::new(&server), quiet())
            .await
            .unwrap();

        assert_eq!(client.directory().await.unwrap(), json!({"sessions": []}));
        let created = client
            .sessions_call("create", json!({"requestId": "s1", "cwd": "/w"}))
            .await
            .unwrap();
        assert_eq!(created, json!({"sessionId": "s1", "created": true}));
        assert_eq!(
            client.directory().await.unwrap()["sessions"][0]["sessionId"],
            "s1"
        );
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn following_a_view_survives_a_lost_link() {
        let server = host_at(session_address("s1"));
        let view = Cell::new(
            state_schema(&contracts::chat(), "viewer", "view"),
            json!({"conversation": {"id": "s1", "cwd": "/w"}, "entries": [], "docs": {}}),
        )
        .unwrap();
        let served = view.clone();
        server.serve(contracts::chat(), move |_input, _principal| {
            Ok(Implementation::new("viewer")
                .state("view", &served)
                .method("entries", |_args, _ctx| async {
                    Ok(json!({"entries": []}))
                }))
        });
        let memory = Memory::new(&server);
        let client = Client::connect(memory.clone(), quiet()).await.unwrap();

        let (sender, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let follower = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .follow("s1", |followed| {
                        let _ = sender.send(match followed {
                            Followed::Update(StateUpdate::Snapshot(value)) => {
                                format!("snapshot {}", value["entries"].as_array().unwrap().len())
                            }
                            Followed::Update(StateUpdate::Ops { value, .. }) => {
                                format!("ops {}", value["entries"].as_array().unwrap().len())
                            }
                            Followed::Update(other) => format!("{other:?}"),
                            Followed::ServerLost => "lost".into(),
                            Followed::Reconnected => "reconnected".into(),
                            Followed::ViewEnded(_) => "ended".into(),
                        });
                    })
                    .await
            })
        };
        assert_eq!(next(&mut seen).await, "snapshot 0");

        memory.opens.lock().unwrap()[0].1.close();
        // Lost and reconnected, with the hold leaving its binding meanwhile.
        let mut events = Vec::new();
        while events.last().map(String::as_str) != Some("reconnected") {
            events.push(next(&mut seen).await);
        }
        assert!(events.contains(&"lost".to_owned()), "{events:?}");
        // The hold goes again over the new link; a reconnect never asks.
        let mut after = next(&mut seen).await;
        while after != "snapshot 0" {
            after = next(&mut seen).await;
        }
        let opens: Vec<bool> = memory.opens.lock().unwrap().iter().map(|o| o.0).collect();
        assert_eq!(opens, [true, false]);

        view.replace(json!({
            "conversation": {"id": "s1", "cwd": "/w"},
            "entries": [{"role": "user", "content": "hi"}],
            "docs": {},
        }))
        .unwrap();
        let changed = next(&mut seen).await;
        assert!(changed == "ops 1" || changed == "snapshot 1", "{changed}");

        follower.abort();
        client.close().await;
        server.close().await;
    }

    #[test]
    fn a_transcript_keeps_every_entry_it_saw_in_order() {
        let entry = |id: u64| json!({"id": id, "kind": "pi.user"});
        let mut transcript = Transcript::default();
        let merged = transcript.merge(&json!({"entries": [entry(3), entry(4)]}));
        assert_eq!(merged, [entry(3), entry(4)]);
        // The window moved on: what left it stays, what is new is added.
        let merged = transcript.merge(&json!({"entries": [entry(5), entry(6)]}));
        assert_eq!(merged, [entry(3), entry(4), entry(5), entry(6)]);
        transcript.add(&json!([entry(1), entry(2)]));
        let ids: Vec<u64> = transcript
            .merge(&json!({"entries": []}))
            .iter()
            .map(|entry| entry["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6]);
    }
}
