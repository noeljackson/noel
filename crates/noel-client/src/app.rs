//! A front end's whole link to the server, for apps (the desktop app, the
//! iOS app through noel-ffi): it connects, follows the session directory
//! and the plugins, follows the selected session's view with its history,
//! and runs what the front end asks. The front end sends [`Command`]s and
//! gets [`Event`]s.

use std::sync::Arc;

use anyhow::Result;
use noel_chord::v2::host::StateUpdate;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{new_request_id, Client, Connector, Followed, Report, Transcript};

/// Where events go: any sender the front end reads from.
pub trait EventSink: Send + Sync + 'static {
    /// `false` once nobody listens.
    fn send(&self, event: Event) -> bool;
}

impl EventSink for mpsc::UnboundedSender<Event> {
    fn send(&self, event: Event) -> bool {
        mpsc::UnboundedSender::send(self, event).is_ok()
    }
}

/// Where a front end's events go.
pub type Events = Arc<dyn EventSink>;

trait SinkExt {
    fn unbounded_send(&self, event: Event) -> Result<(), ()>;
}

impl SinkExt for Arc<dyn EventSink> {
    fn unbounded_send(&self, event: Event) -> Result<(), ()> {
        if self.send(event) {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// What a front end asks of the server.
#[derive(Debug, Clone)]
pub enum Command {
    Select(String),
    Send(String),
    New {
        cwd: String,
    },
    Abort,
    PluginRun {
        plugin: String,
        name: String,
    },
    PanelAction {
        plugin: String,
        panel: String,
        action: String,
    },
}

/// What the server tells a front end.
#[derive(Debug, Clone)]
pub enum Event {
    Connected,
    Status(String),
    Sessions(Vec<Value>),
    /// The selected session's view and every entry seen of its history.
    View {
        session: String,
        view: Value,
        entries: Vec<Value>,
    },
    Plugins(Vec<Value>),
}

/// Connects through `connector` and serves the front end until its
/// commands end.
pub async fn run(
    connector: Arc<dyn Connector>,
    events: Events,
    mut commands: mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    let report_events = events.clone();
    let report: Report = Arc::new(move |line| {
        let _ = report_events.unbounded_send(Event::Status(line));
    });
    let client = Client::connect(connector, report).await?;
    let _ = events.unbounded_send(Event::Connected);
    follow_state(&client, "sessions", events.clone());
    follow_state(&client, "plugins", events.clone());
    let mut selected: Option<String> = None;
    let mut following: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(command) = commands.recv().await {
        let client = client.clone();
        let events = events.clone();
        match command {
            Command::Select(session) => {
                if let Some(task) = following.take() {
                    task.abort();
                }
                selected = Some(session.clone());
                following = Some(tokio::spawn(follow_session(client, session, events)));
            }
            Command::Send(text) => {
                let Some(session) = selected.clone() else {
                    let _ = events.unbounded_send(Event::Status("no session selected".into()));
                    continue;
                };
                tokio::spawn(async move {
                    let args = json!({"requestId": new_request_id(), "text": text, "whenBusy": "followUp"});
                    let status = match client.chat_call(&session, "submit", args, true).await {
                        Ok(result) => result["disposition"].as_str().unwrap_or("sent").to_owned(),
                        Err(error) => format!("send: {error:#}"),
                    };
                    let _ = events.unbounded_send(Event::Status(status));
                });
            }
            Command::New { cwd } => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let (creating, reporting) = (client.clone(), events.clone());
                tokio::spawn(async move {
                    let (client, events) = (creating, reporting);
                    let args = json!({"requestId": new_request_id(), "cwd": cwd});
                    match client.sessions_call("create", args).await {
                        Ok(result) => {
                            let _ = tx.send(result["sessionId"].as_str().map(str::to_owned));
                        }
                        Err(error) => {
                            let _ = events
                                .unbounded_send(Event::Status(format!("new session: {error:#}")));
                            let _ = tx.send(None);
                        }
                    }
                });
                if let Ok(Some(session)) = rx.await {
                    if let Some(task) = following.take() {
                        task.abort();
                    }
                    selected = Some(session.clone());
                    following = Some(tokio::spawn(follow_session(
                        client.clone(),
                        session,
                        events.clone(),
                    )));
                }
            }
            Command::Abort => {
                let Some(session) = selected.clone() else {
                    continue;
                };
                tokio::spawn(async move {
                    let status = match client.chat_call(&session, "abort", json!({}), false).await {
                        Ok(_) => "aborted".to_owned(),
                        Err(error) => format!("abort: {error:#}"),
                    };
                    let _ = events.unbounded_send(Event::Status(status));
                });
            }
            Command::PluginRun { plugin, name } => {
                tokio::spawn(async move {
                    let args = json!({"plugin": plugin, "name": name, "args": ""});
                    let status = match client.plugins_call("runCommand", args).await {
                        Ok(result) => result["output"].as_str().unwrap_or_default().to_owned(),
                        Err(error) => format!("{plugin} {name}: {error:#}"),
                    };
                    let _ = events.unbounded_send(Event::Status(status));
                });
            }
            Command::PanelAction {
                plugin,
                panel,
                action,
            } => {
                tokio::spawn(async move {
                    let args = json!({"plugin": plugin, "panel": panel, "action": action});
                    if let Err(error) = client.plugins_call("panelAction", args).await {
                        let _ = events.unbounded_send(Event::Status(format!("{error:#}")));
                    }
                });
            }
        }
    }
    Ok(())
}

/// Follows `noel.sessions`' directory or `noel.plugins`' directory.
fn follow_state(client: &Client, which: &'static str, events: Events) {
    let reference = match which {
        "sessions" => client.sessions(),
        _ => client.plugins(),
    };
    tokio::spawn(async move {
        let mut updates = reference.subscribe("directory");
        while let Some(update) = updates.next().await {
            let value = match update {
                StateUpdate::Snapshot(value) | StateUpdate::Ops { value, .. } => value,
                StateUpdate::Failed(_) => continue,
            };
            let key = if which == "sessions" {
                "sessions"
            } else {
                "plugins"
            };
            let list = value[key].as_array().cloned().unwrap_or_default();
            let event = if which == "sessions" {
                Event::Sessions(list)
            } else {
                Event::Plugins(list)
            };
            if events.unbounded_send(event).is_err() {
                return;
            }
        }
    });
}

/// Follows the session's view, with the history before its window.
async fn follow_session(client: Client, session: String, events: Events) {
    let (views, mut incoming) = mpsc::unbounded_channel();
    let follower = {
        let (client, session) = (client.clone(), session.clone());
        let events = events.clone();
        tokio::spawn(async move {
            let error = client
                .follow(&session, |followed| match followed {
                    Followed::Update(
                        StateUpdate::Snapshot(view) | StateUpdate::Ops { value: view, .. },
                    ) => {
                        let _ = views.send(view);
                    }
                    Followed::Update(StateUpdate::Failed(error)) => {
                        let _ = events.unbounded_send(Event::Status(format!("view: {error}")));
                    }
                    Followed::ServerLost => {
                        let _ = events
                            .unbounded_send(Event::Status("server lost; reconnecting".into()));
                    }
                    Followed::Reconnected => {
                        let _ = events.unbounded_send(Event::Status("reconnected".into()));
                    }
                    Followed::ViewEnded(why) => {
                        let _ = events.unbounded_send(Event::Status(format!("view ended ({why})")));
                    }
                })
                .await;
            let _ = events.unbounded_send(Event::Status(format!("{session}: {error:#}")));
        })
    };
    let mut transcript = Transcript::default();
    let mut loaded_earlier = false;
    while let Some(view) = incoming.recv().await {
        if !loaded_earlier {
            loaded_earlier = true;
            let _ = transcript.load_earlier(&client, &session, &view).await;
        }
        let entries = transcript.merge(&view);
        if events
            .unbounded_send(Event::View {
                session: session.clone(),
                view,
                entries,
            })
            .is_err()
        {
            break;
        }
    }
    follower.abort();
}
