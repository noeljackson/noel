//! noel's client for native apps (roadmap phase 10): noel-client's
//! [`app`](noel_client::app) through UniFFI, for Swift (the iOS app) and
//! Kotlin.
//!
//! The app opens the byte channel itself (on iOS an SSH channel running
//! `noel bridge`, through the app's SSH library) when [`ChannelOpener::open`]
//! asks, sends the bytes [`ByteChannel::send`] gives it, and hands what
//! arrives to the [`Delivery`] it was given; the link is the same WebSocket
//! link `noel client --remote` runs over ssh. A lost channel is opened
//! again. Events come to a [`NoelListener`] as plain records; panels keep
//! their view as JSON in the widget vocabulary of spec §13.

use std::sync::{Arc, Mutex, PoisonError};

use noel_chord::v2::object::BoxFuture;
use noel_chord::v2::Link;
use noel_client::app::{Command, Event, EventSink};
use noel_client::{websocket_link, Connector, Report};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

uniffi::setup_scaffolding!();

/// A session in the directory.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub message_count: u64,
    pub busy: bool,
}

/// An entry of a session's history, as text.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct EntryItem {
    pub id: u64,
    /// `pi.user`, `pi.assistant`, `pi.tool-result`, `pi.compaction`, ...
    pub kind: String,
    /// `you`, `assistant`, the tool's name, ...
    pub label: String,
    pub text: String,
    pub is_error: bool,
}

/// A plugin panel: its view as JSON (spec §13's widgets).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PanelItem {
    pub id: String,
    pub title: String,
    pub view_json: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PluginItem {
    pub name: String,
    pub running: bool,
    pub commands: Vec<String>,
    pub panels: Vec<PanelItem>,
}

/// What the server tells the app.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum NoelEvent {
    Connected,
    Status {
        text: String,
    },
    Sessions {
        sessions: Vec<SessionItem>,
    },
    /// The selected session's history, and the reply streaming now.
    View {
        session: String,
        entries: Vec<EntryItem>,
        streaming: Option<String>,
    },
    Plugins {
        plugins: Vec<PluginItem>,
    },
}

/// Where the app hears events (called from a background thread).
#[uniffi::export(with_foreign)]
pub trait NoelListener: Send + Sync {
    fn on_event(&self, event: NoelEvent);
}

/// The app's side of one byte channel.
#[uniffi::export(with_foreign)]
pub trait ByteChannel: Send + Sync {
    /// Bytes to write to the channel.
    fn send(&self, bytes: Vec<u8>);
    /// The link is done with the channel.
    fn close(&self);
}

/// Opens a byte channel to `noel bridge` (the app's SSH library), giving
/// what arrives to `delivery`; `None` when it cannot now.
#[uniffi::export(with_foreign)]
pub trait ChannelOpener: Send + Sync {
    fn open_channel(&self, delivery: Arc<Delivery>) -> Option<Arc<dyn ByteChannel>>;
}

/// Where the app hands what arrives on one channel.
#[derive(uniffi::Object)]
pub struct Delivery {
    inbound: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

#[uniffi::export]
impl Delivery {
    /// Bytes that arrived on the channel.
    pub fn receive(&self, bytes: Vec<u8>) {
        if let Some(inbound) = &*self.inbound.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = inbound.send(bytes);
        }
    }

    /// The channel ended.
    pub fn closed(&self) {
        self.inbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

struct Channels(Arc<dyn ChannelOpener>);

impl Connector for Channels {
    fn open(&self, _interactive: bool, _report: Report) -> BoxFuture<anyhow::Result<Link>> {
        let opener = self.0.clone();
        Box::pin(async move {
            let (inbound, mut arrived) = mpsc::unbounded_channel::<Vec<u8>>();
            let delivery = Arc::new(Delivery {
                inbound: Mutex::new(Some(inbound)),
            });
            let channel = tokio::task::spawn_blocking(move || opener.open_channel(delivery))
                .await?
                .ok_or_else(|| anyhow::anyhow!("the channel could not be opened"))?;
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            let (mut reader, mut writer) = tokio::io::split(theirs);
            // What the link writes goes to the channel.
            let sending = channel.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 1 << 16];
                loop {
                    match reader.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => sending.send(buffer[..read].to_vec()),
                    }
                }
                sending.close();
            });
            // What arrives goes to the link.
            tokio::spawn(async move {
                while let Some(bytes) = arrived.recv().await {
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                let _ = writer.shutdown().await;
            });
            websocket_link(ours).await
        })
    }
}

struct Listener(Arc<dyn NoelListener>);

impl EventSink for Listener {
    fn send(&self, event: Event) -> bool {
        self.0.on_event(convert(event));
        true
    }
}

/// A link to the server and what the app asks of it.
#[derive(uniffi::Object)]
pub struct Noel {
    commands: mpsc::UnboundedSender<Command>,
    _runtime: Arc<tokio::runtime::Runtime>,
}

#[uniffi::export]
impl Noel {
    /// Connects over channels `opener` opens, telling `listener` what
    /// happens; it runs on threads of its own.
    #[uniffi::constructor]
    pub fn connect(opener: Arc<dyn ChannelOpener>, listener: Arc<dyn NoelListener>) -> Arc<Noel> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("a tokio runtime"),
        );
        let (commands, receiver) = mpsc::unbounded_channel();
        let events: Arc<dyn EventSink> = Arc::new(Listener(listener.clone()));
        runtime.spawn(async move {
            if let Err(error) =
                noel_client::app::run(Arc::new(Channels(opener)), events, receiver).await
            {
                listener.on_event(NoelEvent::Status {
                    text: format!("connect: {error:#}"),
                });
            }
        });
        Arc::new(Noel {
            commands,
            _runtime: runtime,
        })
    }

    /// Follows the session.
    pub fn select(&self, session: String) {
        let _ = self.commands.send(Command::Select(session));
    }

    /// Sends the text to the selected session (queued as a follow-up while
    /// a run is active).
    pub fn send(&self, text: String) {
        let _ = self.commands.send(Command::Send(text));
    }

    /// Starts a session in `cwd` (a directory on the server's machine).
    pub fn new_session(&self, cwd: String) {
        let _ = self.commands.send(Command::New { cwd });
    }

    pub fn abort(&self) {
        let _ = self.commands.send(Command::Abort);
    }

    pub fn run_plugin_command(&self, plugin: String, name: String) {
        let _ = self.commands.send(Command::PluginRun { plugin, name });
    }

    pub fn panel_action(&self, plugin: String, panel: String, action: String) {
        let _ = self.commands.send(Command::PanelAction {
            plugin,
            panel,
            action,
        });
    }
}

fn convert(event: Event) -> NoelEvent {
    match event {
        Event::Connected => NoelEvent::Connected,
        Event::Status(text) => NoelEvent::Status { text },
        Event::Sessions(sessions) => NoelEvent::Sessions {
            sessions: sessions.iter().map(session_item).collect(),
        },
        Event::View {
            session,
            view,
            entries,
        } => {
            let streaming = view["docs"]["pi.live"]["generation"]["message"]["content"]
                .as_array()
                .map(|parts| text_of_parts(parts))
                .filter(|text| !text.is_empty());
            NoelEvent::View {
                session,
                entries: entries.iter().map(entry_item).collect(),
                streaming,
            }
        }
        Event::Plugins(plugins) => NoelEvent::Plugins {
            plugins: plugins.iter().map(plugin_item).collect(),
        },
    }
}

fn session_item(session: &Value) -> SessionItem {
    let id = session["sessionId"].as_str().unwrap_or_default().to_owned();
    let title = session["name"]
        .as_str()
        .or(session["firstPrompt"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| id.clone());
    SessionItem {
        id,
        title,
        message_count: session["messageCount"].as_u64().unwrap_or(0),
        busy: session["live"]["busy"] == Value::Bool(true),
    }
}

fn text_of_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| match part["type"].as_str() {
            Some("text") => part["text"].as_str().map(str::to_owned),
            Some("toolCall") => Some(format!(
                "-> {}({})",
                part["name"].as_str().unwrap_or_default(),
                part["arguments"]
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn entry_item(entry: &Value) -> EntryItem {
    let message = &entry["model"][0];
    let kind = entry["kind"].as_str().unwrap_or_default().to_owned();
    let text = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => text_of_parts(parts),
        _ if kind == "pi.compaction" => entry["data"]["summary"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    };
    let label = match kind.as_str() {
        "pi.user" => "you".to_owned(),
        "pi.assistant" => "assistant".to_owned(),
        "pi.tool-result" => message["toolName"].as_str().unwrap_or("tool").to_owned(),
        "pi.compaction" => "compacted".to_owned(),
        other => other.to_owned(),
    };
    EntryItem {
        id: entry["id"].as_u64().unwrap_or(0),
        kind,
        label,
        text,
        is_error: message["isError"] == Value::Bool(true),
    }
}

fn plugin_item(plugin: &Value) -> PluginItem {
    let contributions = &plugin["contributions"];
    PluginItem {
        name: plugin["name"].as_str().unwrap_or_default().to_owned(),
        running: plugin["running"] == Value::Bool(true),
        commands: contributions["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|command| command["name"].as_str().map(str::to_owned))
            .collect(),
        panels: contributions["panels"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|panel| PanelItem {
                id: panel["id"].as_str().unwrap_or_default().to_owned(),
                title: panel["title"].as_str().unwrap_or_default().to_owned(),
                view_json: panel["view"].to_string(),
            })
            .collect(),
    }
}
