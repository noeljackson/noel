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
use tokio::sync::watch;

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
    getrandom::fill(&mut bytes).expect("system randomness");
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
    /// The connection's state, which the supervisor owns (#137): it
    /// reconnects whether or not anything follows a session.
    connection: watch::Receiver<Connection>,
    /// Stops the supervisor when the last clone goes, or on `close`.
    supervisor: Arc<Supervisor>,
    report: Report,
}

/// Where the connection to the server is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Connected,
    /// The link dropped; the supervisor is connecting again.
    Lost,
}

/// The task that reconnects; aborted when the client goes.
struct Supervisor(tokio::task::JoinHandle<()>);

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Client {
    pub async fn connect(connector: Arc<dyn Connector>, report: Report) -> Result<Self> {
        Self::connect_with(connector, report, HostOptions::default()).await
    }

    /// [`Client::connect`] with these host options (a test's heartbeat).
    async fn connect_with(
        connector: Arc<dyn Connector>,
        report: Report,
        options: HostOptions,
    ) -> Result<Self> {
        let (closed, mut closes) = tokio::sync::mpsc::unbounded_channel();
        let host = Host::new(options);
        host.observe(move |event| match event {
            HostEvent::Closed { .. } => {
                let _ = closed.send(());
            }
            HostEvent::ProtocolError { error, .. }
                if std::env::var_os("NOEL_CLIENT_DEBUG").is_some() =>
            {
                eprintln!("noel client: chord: {error}")
            }
            _ => {}
        });
        host.connect(connector.open(true, report.clone()).await?, server_link());
        let (state, connection) = watch::channel(Connection::Connected);
        let supervised = (host.clone(), connector, report.clone());
        let supervisor = tokio::spawn(async move {
            let (host, connector, report) = supervised;
            while closes.recv().await.is_some() {
                let _ = state.send(Connection::Lost);
                let link = reconnect(&*connector, &report).await;
                // Closes so far were the old link's: one after the new link
                // is attached is the new link's (#137: a stale signal opened
                // a second link).
                while closes.try_recv().is_ok() {}
                host.connect(link, server_link());
                let _ = state.send(Connection::Connected);
            }
        });
        Ok(Self {
            host,
            connection,
            supervisor: Arc::new(Supervisor(supervisor)),
            report,
        })
    }

    /// The connection's state, as it changes.
    pub fn connection(&self) -> watch::Receiver<Connection> {
        self.connection.clone()
    }

    /// Closes the connection, releasing what it holds.
    pub async fn close(&self) {
        // A close is not a lost link: nothing connects again.
        self.supervisor.0.abort();
        self.host.close().await;
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
            let mut connection = self.connection();
            connection.mark_unchanged();
            let mut lost = false;
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
                    changed = connection.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        // A change is a loss, even one already over: the
                        // watch keeps only the latest state.
                        let now = *connection.borrow_and_update();
                        if !lost {
                            sink(Followed::ServerLost);
                        }
                        lost = now == Connection::Lost;
                        if !lost {
                            sink(Followed::Reconnected);
                        }
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

/// Opens a link to the server again, until it succeeds.
async fn reconnect(connector: &dyn Connector, report: &Report) -> Link {
    let mut delay = Duration::from_millis(100);
    loop {
        match connector.open(false, report.clone()).await {
            Ok(link) => return link,
            Err(error) => report(format!("reconnect: {error:#}")),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
}

/// A session's history as a client holds it (Durable's
/// `ConversationTranscript`): every entry it has seen in a view's window or
/// loaded with `noel.chat`'s `entries`, by id. The view holds only the
/// newest entries; [`Transcript::load_earlier`] pages back from its window.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: BTreeMap<u64, Value>,
    /// Every entry held, oldest first, as last shared; none once an entry
    /// is added or changes.
    shared: Option<Arc<Vec<Value>>>,
}

impl Transcript {
    /// Takes the entries of `view`'s window and returns every entry held,
    /// oldest first.
    pub fn merge(&mut self, view: &Value) -> Vec<Value> {
        self.add(&view["entries"]);
        self.entries.values().cloned().collect()
    }

    /// [`Transcript::merge`] without a copy of the history for each view:
    /// the entries are the same allocation until one is added or changes.
    pub fn merge_shared(&mut self, view: &Value) -> Arc<Vec<Value>> {
        self.add(&view["entries"]);
        self.shared
            .get_or_insert_with(|| Arc::new(self.entries.values().cloned().collect()))
            .clone()
    }

    /// Takes the entries of `earlier` that this transcript lacks.
    pub fn absorb(&mut self, earlier: Transcript) {
        for (id, entry) in earlier.entries {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.entries.entry(id) {
                slot.insert(entry);
                self.shared = None;
            }
        }
    }

    fn add(&mut self, entries: &Value) {
        for entry in entries.as_array().into_iter().flatten() {
            if let Some(id) = entry["id"].as_u64()
                && self.entries.get(&id) != Some(entry)
            {
                self.entries.insert(id, entry.clone());
                self.shared = None;
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
        // One hold for every page: a hold per page is a client that comes
        // and goes, and the session's view tells each follower of both.
        let chat = client.chat(session);
        let loaded = self.load_pages(&chat, view).await;
        chat.release();
        loaded
    }

    async fn load_pages(&mut self, chat: &RemoteRef, view: &Value) -> Result<()> {
        let mut next = view["entries"]
            .get(0)
            .and_then(|entry| entry["id"].as_u64());
        while let Some(before) = next {
            if before <= 1 || self.entries.contains_key(&(before - 1)) {
                break;
            }
            let page = call(chat, "entries", json!({ "before": before })).await?;
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

    /// A link that goes silent without a close (a laptop that slept while
    /// a relay dropped the connection; upstream p2 4b11a722): the
    /// heartbeat closes it, and the client connects again and holds the
    /// view again.
    #[tokio::test]
    async fn a_link_gone_silent_without_a_close_is_reconnected() {
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
        let silent = Silent::new(&server);
        let options = HostOptions {
            ping_ms: 100,
            close_ms: 300,
            tick_ms: 20,
            ..HostOptions::default()
        };
        let client = Client::connect_with(silent.clone(), quiet(), options)
            .await
            .unwrap();
        let (sender, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let follower = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .follow("s1", |followed| {
                        let _ = sender.send(match followed {
                            Followed::Update(StateUpdate::Snapshot(_)) => "snapshot".to_owned(),
                            Followed::ServerLost => "lost".into(),
                            Followed::Reconnected => "reconnected".into(),
                            _ => "other".into(),
                        });
                    })
                    .await
            })
        };
        assert_eq!(next(&mut seen).await, "snapshot");
        // Nothing crosses the first link any more, and nothing closes it.
        silent.mute_first();
        let mut events = Vec::new();
        while events.last().map(String::as_str) != Some("reconnected") {
            events.push(next(&mut seen).await);
        }
        assert!(events.contains(&"lost".to_owned()), "{events:?}");
        let mut after = next(&mut seen).await;
        while after != "snapshot" {
            after = next(&mut seen).await;
        }
        assert_eq!(silent.opens.lock().unwrap().len(), 2);
        follower.abort();
        client.close().await;
        server.close().await;
    }

    /// A connector whose links run through a relay that can stop
    /// delivering without closing anything.
    struct Silent {
        server: Host,
        opens: Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>,
    }

    impl Silent {
        fn new(server: &Host) -> Arc<Self> {
            Arc::new(Silent {
                server: server.clone(),
                opens: Mutex::new(Vec::new()),
            })
        }

        fn mute_first(&self) {
            self.opens.lock().unwrap()[0].store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Connector for Silent {
        fn open(&self, _interactive: bool, _report: Report) -> BoxFuture<Result<Link>> {
            let (ours, relay_client) = Link::memory_pair();
            let (relay_server, theirs) = Link::memory_pair();
            self.server.accept(
                theirs,
                LinkOptions::new(json!({"kind": "owner", "via": "device"})),
            );
            let muted = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.opens.lock().unwrap().push(muted.clone());
            for (from, to) in [
                (relay_client.clone(), relay_server.clone()),
                (relay_server, relay_client),
            ] {
                let muted = muted.clone();
                let mut inbound = from.take_inbound().expect("relay inbound");
                tokio::spawn(async move {
                    while let Some(message) = inbound.recv().await {
                        if !muted.load(std::sync::atomic::Ordering::SeqCst) {
                            to.send(message);
                        }
                    }
                });
            }
            Box::pin(async move { Ok(ours) })
        }
    }

    /// #137: the client reconnects with nothing following a session, and
    /// once: a lost link opens exactly one new one.
    #[tokio::test]
    async fn a_lost_link_is_reconnected_without_a_follower() {
        let server = host_at(session_address("s1"));
        let memory = Memory::new(&server);
        let client = Client::connect(memory.clone(), quiet()).await.unwrap();
        let mut connection = client.connection();
        assert_eq!(*connection.borrow(), Connection::Connected);
        memory.opens.lock().unwrap()[0].1.close();
        // Lost, then connected again (the watch may show only the latter).
        tokio::time::timeout(Duration::from_secs(5), connection.changed())
            .await
            .expect("a change")
            .unwrap();
        while *connection.borrow_and_update() != Connection::Connected {
            tokio::time::timeout(Duration::from_secs(5), connection.changed())
                .await
                .expect("reconnected")
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let opens: Vec<bool> = memory.opens.lock().unwrap().iter().map(|o| o.0).collect();
        assert_eq!(opens, [true, false]);
        // A close is not a loss: nothing connects again.
        client.close().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(memory.opens.lock().unwrap().len(), 2);
    }

    /// #157: a front end hears the link drop and come back, not only a
    /// status line: `Connected` again once the supervisor reconnected.
    #[tokio::test]
    async fn the_app_reports_the_link_coming_back() {
        let server = host_at(session_address("s1"));
        let memory = Memory::new(&server);
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (_commands, commands) = tokio::sync::mpsc::unbounded_channel();
        let events: app::Events = Arc::new(events);
        let running = tokio::spawn(app::run(memory.clone(), events, commands));
        let mut connected = 0;
        let mut reconnecting = 0;
        while connected < 2 {
            let event = tokio::time::timeout(Duration::from_secs(5), seen.recv())
                .await
                .expect("an event")
                .expect("the app runs");
            match event {
                app::Event::Connected => {
                    connected += 1;
                    if connected == 1 {
                        memory.opens.lock().unwrap()[0].1.close();
                    }
                }
                app::Event::Reconnecting => reconnecting += 1,
                _ => {}
            }
        }
        // The watch may coalesce the loss into the reconnect.
        assert!(reconnecting <= 1);
        running.abort();
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

    #[test]
    fn a_transcript_shares_its_entries_until_they_change() {
        let entry = |id: u64| json!({"id": id, "kind": "pi.user"});
        let mut transcript = Transcript::default();
        let first = transcript.merge_shared(&json!({"entries": [entry(3), entry(4)]}));
        // The same window again, as each change to the view's documents
        // brings: no new copy of the history.
        let again = transcript.merge_shared(&json!({"entries": [entry(3), entry(4)]}));
        assert!(Arc::ptr_eq(&first, &again));
        let grown = transcript.merge_shared(&json!({"entries": [entry(4), entry(5)]}));
        assert_eq!(*grown, [entry(3), entry(4), entry(5)]);
        // What was loaded from before the window goes in front; an entry
        // held already stays as it is.
        let mut earlier = Transcript::default();
        earlier.add(&json!([entry(1), entry(2), {"id": 3, "kind": "stale"}]));
        transcript.absorb(earlier);
        let whole = transcript.merge_shared(&json!({"entries": []}));
        assert_eq!(*whole, [entry(1), entry(2), entry(3), entry(4), entry(5)]);
    }
}
