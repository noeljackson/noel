//! The MCP client, as upstream's `client.ts`: the initialize handshake
//! (the server's chosen version must be one noel accepts), paged tool
//! listings with duplicate-cursor and page-cap guards, tool calls, ping,
//! and requests with timeouts. Server-initiated requests answer
//! `method not found` except `ping`, which upstream answers too.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::protocol::jsonrpc::{
    JsonRpcId, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    METHOD_NOT_FOUND,
};
use crate::protocol::types::{Implementation, InitializeResult, Tool};
use crate::protocol::{CallToolResult, LATEST_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
use crate::stdio::{StdioOptions, StdioTransport, TransportEvent};

/// How long a request waits by default.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A listing longer than this many pages is a server bug, not a list.
const MAX_LIST_PAGES: usize = 1_000;

impl From<crate::stdio::McpTransportError> for McpError {
    fn from(error: crate::stdio::McpTransportError) -> Self {
        match error {
            crate::stdio::McpTransportError::Closed(why) => McpError::Closed(why),
            crate::stdio::McpTransportError::TooLarge(bytes) => {
                McpError::Invalid(format!("message exceeds {bytes} bytes"))
            }
        }
    }
}

/// Why a client call failed.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The server answered this JSON-RPC error.
    #[error("MCP server error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("MCP request timed out after {ms}ms")]
    Timeout { ms: u64 },
    #[error("MCP connection closed: {0}")]
    Closed(String),
    #[error("invalid MCP answer: {0}")]
    Invalid(String),
    #[error("MCP server {0}")]
    Spawn(String),
    /// The server answered 401 (or a 403 asking more scope): the OAuth
    /// layer's cue. The body is kept for its error payload.
    #[error("MCP server requires authentication")]
    AuthRequired { body: String },
    /// A 404 while holding a session id: the session expired.
    #[error("MCP session expired")]
    SessionExpired(String),
}

/// A connected MCP client over one stdio server.
/// Which transport a client runs on.
enum Transport {
    Stdio(StdioTransport),
    Http(crate::http::HttpTransport),
}

impl Transport {
    async fn send(&mut self, message: &JsonRpcMessage) -> Result<Option<JsonRpcMessage>, McpError> {
        match self {
            Self::Stdio(transport) => {
                // stdio echoes nothing back through send; answers arrive
                // as events, as the client's request pump reads them.
                transport.send(message).await?;
                Ok(None)
            }
            Self::Http(transport) => transport.send(message).await,
        }
    }

    async fn next_event(&mut self) -> Option<crate::stdio::TransportEvent> {
        match self {
            Self::Stdio(transport) => transport.next_event().await,
            Self::Http(_) => None,
        }
    }

    async fn close(&mut self) {
        match self {
            Self::Stdio(transport) => transport.close().await,
            Self::Http(transport) => transport.close().await,
        }
    }

    fn stderr_tail(&self) -> String {
        match self {
            Self::Stdio(transport) => transport.stderr_tail(),
            Self::Http(_) => String::new(),
        }
    }
}

pub struct McpClient {
    transport: Transport,
    next_id: AtomicI64,
    /// Pending requests by id, with a sender each: answering completes
    /// the caller's future.
    pending: HashMap<JsonRpcId, mpsc::UnboundedSender<Result<Value, McpError>>>,
    /// The server's handshake answer, once connected.
    pub server_info: Option<Implementation>,
    pub protocol_version: Option<String>,
    pub instructions: Option<String>,
    timeout: Duration,
}

impl McpClient {
    /// Connects to a stdio server: spawn, initialize, notify
    /// `notifications/initialized`. The server's chosen protocol version
    /// must be one noel supports; anything else is an error, as
    /// upstream.
    pub async fn connect(options: StdioOptions) -> Result<Self, McpError> {
        Self::with_timeout(options, DEFAULT_REQUEST_TIMEOUT).await
    }

    /// Connects to a streamable-HTTP server (the handshake the same;
    /// answers may arrive as JSON or SSE, which the transport splits).
    pub async fn connect_http(options: crate::http::HttpOptions) -> Result<Self, McpError> {
        let transport = crate::http::HttpTransport::new(options);
        let mut client = Self {
            transport: Transport::Http(transport),
            next_id: AtomicI64::new(1),
            pending: HashMap::new(),
            server_info: None,
            protocol_version: None,
            instructions: None,
            timeout: DEFAULT_REQUEST_TIMEOUT,
        };
        client.handshake().await?;
        Ok(client)
    }

    pub async fn with_timeout(options: StdioOptions, timeout: Duration) -> Result<Self, McpError> {
        let transport =
            StdioTransport::start(options).map_err(|error| McpError::Spawn(error.to_string()))?;
        let mut client = Self {
            transport: Transport::Stdio(transport),
            next_id: AtomicI64::new(1),
            pending: HashMap::new(),
            server_info: None,
            protocol_version: None,
            instructions: None,
            timeout,
        };
        client.handshake().await?;
        Ok(client)
    }

    /// The initialize exchange: send, check the chosen version, notify
    /// `notifications/initialized`.
    async fn handshake(&mut self) -> Result<(), McpError> {
        let result: InitializeResult = self
            .request_value(
                "initialize",
                Some(json!({
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "noel",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                })),
            )
            .await?
            .into_initialize_result()?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&result.protocol_version.as_str()) {
            return Err(McpError::Invalid(format!(
                "selected unsupported protocol version {}",
                result.protocol_version
            )));
        }
        self.server_info = Some(result.server_info.clone());
        self.protocol_version = Some(result.protocol_version.clone());
        self.instructions = result.instructions;
        if let Transport::Http(transport) = &mut self.transport {
            transport.set_protocol_version(
                self.protocol_version
                    .as_deref()
                    .unwrap_or(LATEST_PROTOCOL_VERSION),
            );
        }
        self.notify("notifications/initialized").await?;
        Ok(())
    }

    /// One JSON-RPC request: send, pump events until its answer or the
    /// timeout, and return the result.
    async fn request_value(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, McpError> {
        let id = JsonRpcId::Number(self.next_id.fetch_add(1, Ordering::SeqCst));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        self.pending.insert(id.clone(), done_tx);
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            method: method.to_string(),
            params,
        });
        // HTTP carries the answer back through send() (JSON or the SSE
        // stream it reads to the response); stdio answers arrive as
        // events the pump below reads.
        if matches!(self.transport, Transport::Http(_)) {
            match self.transport.send(&message).await {
                Ok(Some(JsonRpcMessage::Response(response))) => {
                    self.pending.remove(&id);
                    return match response {
                        JsonRpcResponse::Success { result, .. } => Ok(result),
                        JsonRpcResponse::Error { error, .. } => Err(McpError::Server {
                            code: error.code,
                            message: error.message,
                        }),
                    };
                }
                Ok(Some(other)) => {
                    // A server-initiated message before the answer.
                    let _ = self.dispatch(other).await;
                }
                Ok(None) => {
                    // Notifications answer nothing; a request without a
                    // response is the stream's internal-error answer.
                    self.pending.remove(&id);
                    return Err(McpError::Invalid(
                        "no answer on the HTTP stream".to_string(),
                    ));
                }
                Err(error) => {
                    self.pending.remove(&id);
                    return Err(error);
                }
            }
        }
        if let Err(error) = self.transport.send(&message).await {
            self.pending.remove(&id);
            return Err(error);
        }
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let event = tokio::select! {
                biased;
                answer = done_rx.recv() => {
                    // The reader (the same task pumping below) cannot
                    // exist yet; answers arrive through next_event.
                    let _ = answer;
                    unreachable!()
                }
                event = self.transport.next_event() => match event {
                    Some(event) => event,
                    None => {
                        self.pending.remove(&id);
                        return Err(McpError::Closed("server exited".to_string()));
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    self.pending.remove(&id);
                    // Tell the server we no longer wait, as upstream does.
                    let _ = self
                        .transport
                        .send(&JsonRpcMessage::Notification(JsonRpcNotification {
                            jsonrpc: "2.0".to_string(),
                            method: "notifications/cancelled".to_string(),
                            params: Some(json!({"requestId": id, "reason": "Request timed out"})),
                        }))
                        .await;
                    return Err(McpError::Timeout {
                        ms: self.timeout.as_millis() as u64,
                    });
                }
            };
            match event {
                TransportEvent::Message(message) => {
                    // Only the answer to our own request ends the wait;
                    // server-initiated traffic is answered inline.
                    if let JsonRpcMessage::Response(response) = &message
                        && response.id() != &id
                    {
                        continue;
                    }
                    if let Some(answer) = self.dispatch(message).await {
                        self.pending.remove(&id);
                        return answer;
                    }
                }
                TransportEvent::Error(_) => {}
                TransportEvent::Closed => {
                    self.pending.remove(&id);
                    return Err(McpError::Closed("server exited".to_string()));
                }
            }
        }
    }

    /// Applies one message: answers a pending request, or answers a
    /// server request (`ping` gets `{}`; anything else gets method-not-
    /// found, as upstream). `Some(result)` when it was our answer.
    async fn dispatch(&mut self, message: JsonRpcMessage) -> Option<Result<Value, McpError>> {
        match message {
            JsonRpcMessage::Response(response) => {
                let id = response.id().clone();
                match response {
                    JsonRpcResponse::Success { result, .. } => Some(Ok(result)),
                    JsonRpcResponse::Error { error, .. } => Some(Err(McpError::Server {
                        code: error.code,
                        message: error.message,
                    })),
                }
                .inspect(|_| {
                    let _ = id;
                })
            }
            JsonRpcMessage::Request(request) => {
                // Answer ping, refuse the rest, without waiting on the
                // send: the server cannot be blocked by our reply.
                let answer = if request.method == "ping" {
                    JsonRpcMessage::Response(JsonRpcResponse::Success {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        result: json!({}),
                    })
                } else {
                    JsonRpcMessage::Response(JsonRpcResponse::Error {
                        jsonrpc: "2.0".to_string(),
                        id: request.id,
                        error: crate::protocol::jsonrpc::JsonRpcErrorObject {
                            code: METHOD_NOT_FOUND,
                            message: format!("Method not found: {}", request.method),
                            data: None,
                        },
                    })
                };
                let _ = self.transport.send(&answer).await;
                None
            }
            JsonRpcMessage::Notification(_) => None,
        }
    }

    /// One notification, fire and forget.
    async fn notify(&mut self, method: &str) -> Result<(), McpError> {
        self.transport
            .send(&JsonRpcMessage::Notification(JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: method.to_string(),
                params: None,
            }))
            .await
            .map(|_| ())
    }

    /// Every tool the server offers, following `nextCursor` through all
    /// pages (upstream `listAll`, with its duplicate-cursor and page
    /// guards).
    pub async fn list_tools(&mut self) -> Result<Vec<Tool>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = std::collections::BTreeSet::new();
        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|cursor| json!({ "cursor": cursor }));
            let page = self.request_value("tools/list", params).await?;
            let page = parse_list_page("tools/list", "tools", &page)?;
            tools.extend(page.tools);
            match page.next_cursor {
                None => return Ok(tools),
                Some(next) => {
                    if !seen_cursors.insert(next.clone()) {
                        return Err(McpError::Invalid(format!(
                            "tools/list returned duplicate cursor: {next}"
                        )));
                    }
                    cursor = Some(next);
                }
            }
        }
        Err(McpError::Invalid(format!(
            "tools/list exceeded {MAX_LIST_PAGES} pages"
        )))
    }

    /// Calls a tool; the answer is validated as upstream's
    /// `validateCallToolResult` (content defaults to empty).
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult, McpError> {
        let mut params = json!({"name": name});
        if let Some(arguments) = arguments {
            params["arguments"] = arguments;
        }
        let value = self.request_value("tools/call", Some(params)).await?;
        parse_call_tool_result(&value)
    }

    /// Closes the connection (spec shutdown: stdin, SIGTERM, SIGKILL).
    pub async fn close(&mut self) {
        self.transport.close().await;
    }

    /// The server's stderr so far, for a failed connect's diagnostics.
    /// Sets the bearer token HTTP requests carry (the OAuth layer's
    /// stored or refreshed access token).
    pub fn set_token(&mut self, token: Option<String>) {
        if let Transport::Http(transport) = &mut self.transport {
            transport.set_token(token);
        }
    }

    /// Refreshes authorization after an `AuthRequired` failure using
    /// `refresh`: the stored client against the token endpoint the
    /// discovery found. `Ok(true)` means retry with the new token set.
    pub async fn refresh_after_auth_required(
        &mut self,
        refresh_token: &str,
        client_information: &crate::oauth::ClientInformation,
        token_endpoint: &str,
    ) -> Result<crate::oauth::OAuthTokens, crate::oauth::OAuthError> {
        let http = reqwest::Client::new();
        crate::oauth::refresh(&http, token_endpoint, client_information, refresh_token).await
    }

    pub fn stderr_tail(&self) -> String {
        self.transport.stderr_tail()
    }
}

/// One page of a tool listing, checked as upstream `validateListPage`.
struct ListPage {
    tools: Vec<Tool>,
    next_cursor: Option<String>,
}

fn parse_list_page(method: &str, key: &str, value: &Value) -> Result<ListPage, McpError> {
    let invalid = |what: String| McpError::Invalid(format!("invalid MCP {method} result: {what}"));
    let Some(object) = value.as_object() else {
        return Err(invalid("not an object".to_string()));
    };
    let Some(items) = object.get(key).and_then(Value::as_array) else {
        return Err(invalid(format!("no {key} array")));
    };
    let mut tools = Vec::with_capacity(items.len());
    for item in items {
        let Some(item) = item.as_object() else {
            return Err(invalid("an entry is not an object".to_string()));
        };
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("a tool has no name".to_string()))?;
        let schema = item
            .get("inputSchema")
            .ok_or_else(|| invalid("a tool has no inputSchema".to_string()))?;
        tools.push(Tool {
            name: name.to_string(),
            title: item
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: item
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_schema: schema.clone(),
            output_schema: item.get("outputSchema").cloned(),
            annotations: item
                .get("annotations")
                .and_then(Value::as_object)
                .map(|annotations| serde_json::from_value(Value::Object(annotations.clone())))
                .transpose()
                .map_err(|_| invalid("invalid annotations".to_string()))?
                .unwrap_or_default(),
        });
    }
    // Some servers end pagination with `null` or `""` instead of
    // omitting the cursor.
    let next_cursor = match object.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(invalid("cursor is not a string".to_string())),
    };
    Ok(ListPage { tools, next_cursor })
}

fn parse_call_tool_result(value: &Value) -> Result<CallToolResult, McpError> {
    // `content` is required by the spec, but servers that return only
    // `structuredContent` omit it; the client defaults it to empty.
    if let Some(content) = value.get("content")
        && !content.is_array()
    {
        return Err(McpError::Invalid(
            "invalid MCP tools/call result: content is not an array".to_string(),
        ));
    }
    serde_json::from_value(value.clone())
        .map_err(|error| McpError::Invalid(format!("invalid MCP tools/call result: {error}")))
}

/// Extends a raw initialize answer with validation, upstream's
/// `validateInitializeResult`.
trait InitializeResultExt {
    fn into_initialize_result(self) -> Result<InitializeResult, McpError>;
}

impl InitializeResultExt for Value {
    fn into_initialize_result(self) -> Result<InitializeResult, McpError> {
        let invalid = || McpError::Invalid("invalid MCP initialize result".to_string());
        let object = self.as_object().ok_or_else(invalid)?;
        if !object.get("protocolVersion").is_some_and(Value::is_string) {
            return Err(invalid());
        }
        let server_info = object
            .get("serverInfo")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        if !server_info.get("name").is_some_and(Value::is_string)
            || !server_info.get("version").is_some_and(Value::is_string)
        {
            return Err(invalid());
        }
        if let Some(instructions) = object.get("instructions")
            && !instructions.is_string()
        {
            return Err(invalid());
        }
        serde_json::from_value(self)
            .map_err(|_| McpError::Invalid("invalid MCP initialize result".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny MCP server as a shell script: answers initialize and
    /// tools/list, echoes tools/call.
    fn server_script() -> String {
        r##"
answer() { printf '%s\n' "$1"; }
while read -r line; do
    method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$method" in
        initialize)
            answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{}},\"serverInfo\":{\"name\":\"test\",\"version\":\"1.0\"}}}" ;;
        notifications/*) ;;
        tools/list)
            answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echoes\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
        tools/call)
            answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}}" ;;
        ping)
            answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}" ;;
        *)
            answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32601,\"message\":\"nope\"}}" ;;
    esac
done
"##
        .to_string()
    }

    #[tokio::test]
    async fn a_client_connects_lists_and_calls() {
        let mut client = McpClient::connect(StdioOptions {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), server_script()],
            ..Default::default()
        })
        .await
        .expect("connect");
        assert_eq!(client.protocol_version.as_deref(), Some("2025-06-18"));
        assert_eq!(
            client.server_info.as_ref().map(|info| info.name.as_str()),
            Some("test")
        );
        let tools = client.list_tools().await.expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = client
            .call_tool("echo", Some(json!({"text": "ping"})))
            .await
            .expect("call");
        assert_eq!(result.content.len(), 1);
        client.close().await;
    }

    #[tokio::test]
    async fn an_unsupported_protocol_version_is_refused() {
        let script = server_script().replace("2025-06-18", "1999-01-01");
        let error = match McpClient::connect(StdioOptions {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script],
            ..Default::default()
        })
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected the version to be refused"),
        };
        assert!(
            error.to_string().contains("unsupported protocol"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_missing_command_reports_the_spawn() {
        let error = match McpClient::connect(StdioOptions {
            command: "definitely-not-a-command-xyz".to_string(),
            ..Default::default()
        })
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("expected the spawn to fail"),
        };
        assert!(matches!(error, McpError::Spawn(_)), "{error}");
    }
}
