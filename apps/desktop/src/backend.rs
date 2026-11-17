//! The app's link to `noel server`: noel-client's [`app`](noel_client::app)
//! on a tokio runtime of its own thread, through a bridge.
//!
//! The server is reached through `noel bridge` (a local server, started
//! when none runs) or `ssh -T <host> noel bridge` (a remote one): the
//! bridge's stdin and stdout carry the WebSocket link, as for `noel client
//! --remote`.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures::channel::mpsc::UnboundedSender;
use noel_chord::v2::object::BoxFuture;
use noel_chord::v2::Link;
use noel_client::app::EventSink;
pub use noel_client::app::{Command, Event};
use noel_client::{websocket_link, Connector, Report};
use tokio::sync::mpsc;

/// Where the server is.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// `noel` to run `bridge` with (local or remote).
    pub program: String,
    /// The ssh destination of a remote server.
    pub remote: Option<String>,
}

struct Bridge(Endpoint);

impl Connector for Bridge {
    fn open(&self, interactive: bool, report: Report) -> BoxFuture<Result<Link>> {
        let endpoint = self.0.clone();
        Box::pin(async move {
            let mut command = match &endpoint.remote {
                Some(destination) => {
                    let mut command = tokio::process::Command::new("ssh");
                    command.arg("-T").arg("-e").arg("none");
                    if !interactive {
                        command.args(["-o", "BatchMode=yes"]);
                    }
                    command.args(["--", destination, &format!("'{}' bridge", endpoint.program)]);
                    command
                }
                None => {
                    let mut command = tokio::process::Command::new(&endpoint.program);
                    command.arg("bridge");
                    command
                }
            };
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            let mut child = command
                .spawn()
                .map_err(|error| anyhow!("start the bridge: {error}"))?;
            let stdout = child.stdout.take().expect("piped");
            let stdin = child.stdin.take().expect("piped");
            tokio::spawn(async move {
                if let Ok(status) = child.wait().await {
                    if !status.success() {
                        report(format!("the bridge ended: {status}"));
                    }
                }
            });
            websocket_link(tokio::io::join(stdout, stdin)).await
        })
    }
}

/// Events into the window's channel.
struct Window(UnboundedSender<Event>);

impl EventSink for Window {
    fn send(&self, event: Event) -> bool {
        self.0.unbounded_send(event).is_ok()
    }
}

/// Connects to `endpoint` and serves the window until it is gone.
pub async fn run(
    endpoint: Endpoint,
    events: UnboundedSender<Event>,
    commands: mpsc::UnboundedReceiver<Command>,
) -> Result<()> {
    noel_client::app::run(
        Arc::new(Bridge(endpoint)),
        Arc::new(Window(events)),
        commands,
    )
    .await
}
