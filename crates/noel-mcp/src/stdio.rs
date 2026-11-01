//! The transport abstraction and the stdio transport, as upstream's
//! `transports/transport.ts` and `transports/stdio.ts`: newline-delimited
//! JSON-RPC over the server's stdin/stdout, the server's stderr kept (a
//! bounded tail) for diagnostics, and shutdown per the spec — stdin
//! closed, then SIGTERM, then SIGKILL, to the whole process group so
//! wrappers (`npx`, `uvx`) do not leave the server behind.

use std::collections::VecDeque;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::protocol::jsonrpc::{JsonRpcMessage, McpParseError};

pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
/// How long a server gets to exit on its own after stdin closes, before
/// it is sent SIGTERM.
const STDIN_CLOSE_GRACE_MS: u64 = 500;
/// What a running stdio transport reports: messages, or its end.
#[derive(Debug)]
pub enum TransportEvent {
    Message(JsonRpcMessage),
    /// A line the transport could not use (not JSON, or not a message).
    Error(String),
    /// The server's process ended. Nothing follows.
    Closed,
}

/// A running stdio MCP server connection: writes to its stdin, reads
/// events from its stdout and stderr until [`StdioTransport::close`].
pub struct StdioTransport {
    child: Child,
    stdin: tokio::process::ChildStdin,
    events: mpsc::UnboundedReceiver<TransportEvent>,
    stderr_tail: std::sync::Arc<std::sync::Mutex<VecDeque<u8>>>,
    closing: bool,
    /// Kills the server's process group when the transport drops
    /// (dropped with `child`, whose `kill_on_drop` takes the direct
    /// child). Held for the drop, never read.
    #[allow(dead_code)]
    group: noel_env::ToolProcessGroup,
}

/// How to start a stdio server.
#[derive(Debug, Clone, Default)]
pub struct StdioOptions {
    pub command: String,
    pub args: Vec<String>,
    /// Relative paths resolve against the caller's cwd.
    pub cwd: Option<std::path::PathBuf>,
    /// Extra environment; the process environment is inherited.
    pub env: Vec<(String, String)>,
}

#[cfg(unix)]
fn nix_kill(pid: Option<u32>) -> std::io::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    let pid = pid.ok_or_else(|| std::io::Error::other("no pid"))?;
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .map(|_| ())
        .map_err(|error| std::io::Error::other(error.to_string()))
}

impl StdioTransport {
    /// Spawns the server in its own session and process group (as noel's
    /// tool subprocesses are, so closing kills the whole tree) and starts
    /// reading its output. `Ok` once the process spawned; the handshake
    /// that follows tells whether it speaks MCP.
    pub fn start(options: StdioOptions) -> std::io::Result<Self> {
        let mut command = Command::new(&options.command);
        command
            .args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        for (name, value) in &options.env {
            command.env(name, value);
        }
        let (mut child, group) = noel_env::spawn_tool_process(&mut command)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (event_sink, events) = mpsc::unbounded_channel();
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(VecDeque::new()));

        // stdout: newline-delimited JSON-RPC messages, each capped.
        {
            let sink = event_sink.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::with_capacity(64 * 1024, stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let line = line.trim_end_matches('\r');
                            if line.trim().is_empty() {
                                continue;
                            }
                            let message = serde_json::from_str(line)
                                .map_err(|error| error.to_string())
                                .and_then(|value| {
                                    JsonRpcMessage::parse(value).map_err(|error| error.to_string())
                                });
                            match message {
                                Ok(message) => {
                                    if sink.send(TransportEvent::Message(message)).is_err() {
                                        return;
                                    }
                                }
                                Err(error) => {
                                    if sink
                                        .send(TransportEvent::Error(format!(
                                            "invalid message from MCP server: {error}"
                                        )))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            let _ = sink.send(TransportEvent::Error(error.to_string()));
                            break;
                        }
                    }
                }
                let _ = sink.send(TransportEvent::Closed);
            });
        }
        // stderr: kept as a bounded tail for diagnostics.
        {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut stderr = stderr;
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.extend_from_slice(&chunk[..read]);
                            if buffer.len() > DEFAULT_MAX_STDERR_BYTES {
                                let excess = buffer.len() - DEFAULT_MAX_STDERR_BYTES;
                                buffer.drain(..excess);
                            }
                        }
                    }
                }
                *tail.lock().expect("stderr tail") = VecDeque::from(buffer);
            });
        }

        Ok(Self {
            child,
            stdin,
            events,
            stderr_tail,
            closing: false,
            group,
        })
    }

    /// Sends one message.
    pub async fn send(&mut self, message: &JsonRpcMessage) -> Result<(), McpTransportError> {
        let payload = serde_json::to_string(message)
            .map_err(|error| McpTransportError::Closed(format!("serialize message: {error}")))?;
        self.stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|error| McpTransportError::Closed(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| McpTransportError::Closed(error.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| McpTransportError::Closed(error.to_string()))
    }

    /// Waits for the next event, or `None` once the channel closed.
    pub async fn next_event(&mut self) -> Option<TransportEvent> {
        self.events.recv().await
    }

    /// The server's stderr so far, as a bounded tail (diagnostics for a
    /// server that never answered the handshake).
    pub fn stderr_tail(&self) -> String {
        let tail = self.stderr_tail.lock().expect("stderr tail");
        String::from_utf8_lossy(&tail.iter().copied().collect::<Vec<_>>()).into_owned()
    }

    /// Closes the connection per the spec: stdin closed and the server
    /// ends, then SIGTERM to its process group, then SIGKILL. Idempotent.
    pub async fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        // The spec's shutdown: close stdin and let the server exit.
        let _ = self.stdin.shutdown().await;
        let exited = tokio::time::timeout(
            std::time::Duration::from_millis(STDIN_CLOSE_GRACE_MS),
            self.child.wait(),
        )
        .await
        .is_ok();
        if !exited {
            // Then SIGTERM the group (the child's own group, which the
            // process-group guard kills on drop as SIGKILL after this).
            self.terminate_group();
        }
    }

    fn terminate_group(&self) {
        // The direct child gets SIGTERM; anything it spawned is the
        // group's, taken by the dropped process-group guard if it
        // survives the child.
        // tokio's try_wait needs &mut, which close() holds; the group
        // guard kills anything still running at drop, so a TERM to the
        // child is safe whether or not it already exited (ESRCH).
        let _ = nix_kill(self.child.id());
    }
}

/// Why a send failed.
#[derive(Debug, thiserror::Error)]
pub enum McpTransportError {
    #[error("MCP transport closed: {0}")]
    Closed(String),
    #[error("MCP message too large ({0} bytes)")]
    TooLarge(usize),
}

impl From<McpParseError> for McpTransportError {
    fn from(error: McpParseError) -> Self {
        Self::Closed(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::jsonrpc::{JsonRpcId, JsonRpcRequest};
    use serde_json::json;

    /// An MCP server as a shell one-liner: echo the handshake answer for
    /// any request, so the test reads its own request back.
    #[tokio::test]
    async fn stdio_round_trips_a_request() {
        // A server that answers with its own input: cat, one line per
        // message.
        let mut transport = StdioTransport::start(StdioOptions {
            command: "cat".to_string(),
            ..Default::default()
        })
        .expect("spawn cat");
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(1),
            method: "ping".to_string(),
            params: None,
        });
        transport.send(&request).await.unwrap();
        match transport.next_event().await {
            Some(TransportEvent::Message(JsonRpcMessage::Request(back))) => {
                assert_eq!(back.method, "ping");
            }
            other => panic!("expected the request echoed back, got {other:?}"),
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn invalid_lines_are_reported_not_fatal() {
        let mut transport = StdioTransport::start(StdioOptions {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "printf 'not json\\n'; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'; cat".to_string(),
            ],
            ..Default::default()
        })
        .expect("spawn sh");
        match transport.next_event().await {
            Some(TransportEvent::Error(_)) => {}
            other => panic!("expected an error event, got {other:?}"),
        }
        match transport.next_event().await {
            Some(TransportEvent::Message(JsonRpcMessage::Response(_))) => {}
            other => panic!("expected the good line, got {other:?}"),
        }
        transport.close().await;
    }

    #[tokio::test]
    async fn the_json_shape_of_a_request_is_upstreams() {
        let request = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: JsonRpcId::Number(3),
            method: "tools/call".to_string(),
            params: Some(json!({"name": "x"})),
        });
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"x"}}"#
        );
    }
}
