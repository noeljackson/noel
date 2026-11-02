//! The streamable-HTTP transport, ported from upstream's
//! `transports/streamable-http.ts`: JSON-RPC over HTTP POST, with the
//! answer as JSON or an SSE stream; a server-to-client GET stream opened
//! after initialization; `Mcp-Session-Id` and `MCP-Protocol-Version`
//! headers; resumption by `Last-Event-ID` when a stream drops after the
//! server assigned event ids; and reconnection with exponential backoff
//! (or the server's `retry` field). 401 (and a 403 asking more scope) is
//! reported as [`McpError::AuthRequired`] for the OAuth layer; a 404 with
//! a session is [`McpError::SessionExpired`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::client::McpError;
use crate::protocol::jsonrpc::{JsonRpcId, JsonRpcMessage, JsonRpcResponse, INTERNAL_ERROR};
use crate::protocol::LATEST_PROTOCOL_VERSION;
use crate::stdio::TransportEvent;

const DEFAULT_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_MESSAGE_BODY_CHARS: usize = 500;
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
const DEFAULT_RECONNECT_MAX_RETRIES: u32 = 5;

/// How to start an HTTP server connection.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub url: String,
    /// Extra headers (an `Authorization` from config; OAuth sets its own).
    pub headers: Vec<(String, String)>,
    /// A bearer token every request carries from the start, including
    /// the handshake (the OAuth layer's stored access token).
    pub token: Option<String>,
    /// Open the server-to-client GET stream after initialization.
    pub open_get_stream: bool,
    /// Reconnection of dropped SSE streams.
    pub reconnect_initial_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
    pub reconnect_max_retries: u32,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            headers: Vec::new(),
            token: None,
            open_get_stream: true,
            reconnect_initial_delay_ms: DEFAULT_RECONNECT_INITIAL_DELAY_MS,
            reconnect_max_delay_ms: DEFAULT_RECONNECT_MAX_DELAY_MS,
            reconnect_max_retries: DEFAULT_RECONNECT_MAX_RETRIES,
        }
    }
}

/// One SSE event off a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Parses one SSE stream to its end, calling `on_event` per event.
/// Line discipline per the SSE spec: `data:` lines join with `\n`, a
/// blank line dispatches, `:` starts a comment, `id` without NUL is
/// remembered, numeric `retry` reported.
pub async fn consume_sse_stream<R, F>(mut reader: R, mut on_event: F) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(SseEvent),
{
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    let mut event_name: Option<String> = None;
    let mut event_id: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut data_bytes = 0usize;

    let dispatch = |event_name: &mut Option<String>,
                    event_id: &mut Option<String>,
                    data_lines: &mut Vec<String>,
                    data_bytes: &mut usize,
                    on_event: &mut F| {
        if data_lines.is_empty() {
            *event_name = None;
            *event_id = None;
            return;
        }
        let data = data_lines.join("\n");
        on_event(SseEvent {
            event: event_name.take(),
            data,
            id: event_id.take(),
        });
        data_lines.clear();
        *data_bytes = 0;
    };

    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        buffered.extend_from_slice(&chunk[..read]);
        while let Some(newline) = buffered.iter().position(|byte| *byte == b'\n') {
            let line_bytes = buffered.drain(..=newline).collect::<Vec<_>>();
            let raw = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
            let line = raw.strip_suffix('\r').unwrap_or(&raw);
            if line.is_empty() {
                dispatch(
                    &mut event_name,
                    &mut event_id,
                    &mut data_lines,
                    &mut data_bytes,
                    &mut on_event,
                );
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.find(':') {
                Some(colon) => {
                    let value = &line[colon + 1..];
                    (&line[..colon], value.strip_prefix(' ').unwrap_or(value))
                }
                None => (line, ""),
            };
            match field {
                "data" => {
                    data_bytes += value.len() + usize::from(!data_lines.is_empty());
                    if data_bytes > DEFAULT_MAX_EVENT_BYTES {
                        return Err(format!(
                            "MCP SSE event exceeds {DEFAULT_MAX_EVENT_BYTES} bytes"
                        ));
                    }
                    data_lines.push(value.to_string());
                }
                "event" => event_name = Some(value.to_string()),
                "id" if !value.contains('\0') => event_id = Some(value.to_string()),
                _ => {}
            }
        }
        if buffered.len() > DEFAULT_MAX_EVENT_BYTES {
            return Err(format!(
                "MCP SSE event exceeds {DEFAULT_MAX_EVENT_BYTES} bytes"
            ));
        }
    }
    if !buffered.is_empty() {
        let raw = String::from_utf8_lossy(&buffered).into_owned();
        // A final line without its newline dispatches like any other.
        for line in raw.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                dispatch(
                    &mut event_name,
                    &mut event_id,
                    &mut data_lines,
                    &mut data_bytes,
                    &mut on_event,
                );
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.find(':') {
                Some(colon) => {
                    let value = &line[colon + 1..];
                    (&line[..colon], value.strip_prefix(' ').unwrap_or(value))
                }
                None => (line, ""),
            };
            if field == "data" {
                data_lines.push(value.to_string());
            } else if field == "event" {
                event_name = Some(value.to_string());
            } else if field == "id" && !value.contains('\0') {
                event_id = Some(value.to_string());
            }
        }
    }
    dispatch(
        &mut event_name,
        &mut event_id,
        &mut data_lines,
        &mut data_bytes,
        &mut on_event,
    );
    Ok(())
}

/// The state one stream's resumption needs: the last event id the server
/// assigned, the `retry` it asked for, and whether it delivered anything
/// since it was (re)opened.
#[derive(Debug, Default, Clone)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry_ms: Option<u64>,
    received: bool,
    event_name: Option<String>,
}

/// A running HTTP MCP connection.
pub struct HttpTransport {
    options: HttpOptions,
    client: reqwest::Client,
    /// Out-of-band messages (the GET stream, or SSE answers other than
    /// what the caller's request waits for) surface here.
    events: mpsc::UnboundedReceiver<TransportEvent>,
    event_sink: mpsc::UnboundedSender<TransportEvent>,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<String>,
    closed: AtomicBool,
    last_token: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(options: HttpOptions) -> Self {
        let (event_sink, events) = mpsc::unbounded_channel();
        // A token in the options rides every request from the start,
        // including the handshake (the OAuth layer's stored token).
        let token = options.token.clone();
        Self {
            client: reqwest::Client::builder().build().unwrap_or_default(),
            options,
            events,
            event_sink,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(LATEST_PROTOCOL_VERSION.to_string()),
            closed: AtomicBool::new(false),
            last_token: Mutex::new(token),
        }
    }

    /// Sets the bearer token requests carry (the OAuth layer refreshes
    /// it; `None` sends none).
    pub fn set_token(&self, token: Option<String>) {
        *self.last_token.lock().expect("token") = token;
    }

    /// Sets the protocol version the server chose, sent as
    /// `MCP-Protocol-Version` from then on.
    pub fn set_protocol_version(&self, version: &str) {
        *self.protocol_version.lock().expect("version") = version.to_string();
    }

    fn headers(&self, extra: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &self.options.headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            ) {
                headers.insert(name, value);
            }
        }
        if let Some(session) = self.session_id.lock().expect("session").as_deref() {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::try_from("Mcp-Session-Id"),
                reqwest::header::HeaderValue::try_from(session),
            ) {
                headers.insert(name, value);
            }
        }
        if let Ok(value) = reqwest::header::HeaderValue::try_from(
            self.protocol_version.lock().expect("version").as_str(),
        ) {
            headers.insert("MCP-Protocol-Version", value);
        }
        if let Some(token) = self.last_token.lock().expect("token").as_deref() {
            if let Ok(value) = reqwest::header::HeaderValue::try_from(format!("Bearer {token}")) {
                headers.insert("Authorization", value);
            }
        }
        for (name, value) in extra {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::try_from(*name),
                reqwest::header::HeaderValue::try_from(*value),
            ) {
                headers.insert(name, value);
            }
        }
        headers
    }

    /// Sends one message; the answer, when the message is a request,
    /// arrives as `send`'s return or through [`Self::next_event`] (an
    /// SSE-carried answer to an earlier request). A notification's 202
    /// is not an answer and returns `Ok(None)`.
    pub async fn send(
        &mut self,
        message: &JsonRpcMessage,
    ) -> Result<Option<JsonRpcMessage>, McpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpError::Closed("transport closed".into()));
        }
        let body = serde_json::to_string(message)
            .map_err(|error| McpError::Closed(format!("serialize: {error}")))?;
        let response = self
            .client
            .post(&self.options.url)
            .headers(self.headers(&[
                ("accept", "application/json, text/event-stream"),
                ("content-type", "application/json"),
            ]))
            .body(body)
            .send()
            .await
            .map_err(|error| McpError::Spawn(error.to_string()))?;
        let response = self.check_response(response).await?;
        self.capture_session(&response);

        let is_request = matches!(message, JsonRpcMessage::Request(_));
        if !is_request {
            // Notifications are acknowledged without a reply. The
            // server-to-client stream opens once the session is
            // initialized.
            if let JsonRpcMessage::Notification(notification) = message {
                if notification.method == "notifications/initialized" {
                    self.start_get_stream();
                }
            }
            return Ok(None);
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        if status == 202 || status == 204 {
            return Err(McpError::Invalid(format!(
                "server accepted the request without a response (HTTP {status})"
            )));
        }
        if content_type == "application/json" {
            let body: Value = response
                .json()
                .await
                .map_err(|error| McpError::Invalid(format!("JSON answer: {error}")))?;
            // One message, or a batch (an array).
            let items = body
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![body.clone()]);
            let mut answer = None;
            for item in items {
                let message = JsonRpcMessage::parse(item)
                    .map_err(|error| McpError::Invalid(error.to_string()))?;
                match (&message, answer.is_none()) {
                    (JsonRpcMessage::Response(_), true) => answer = Some(message),
                    _ => {
                        let _ = self.event_sink.send(TransportEvent::Message(message));
                    }
                }
            }
            return Ok(answer);
        }
        if content_type == "text/event-stream" {
            let request_id = match message {
                JsonRpcMessage::Request(request) => request.id.clone(),
                _ => unreachable!("checked above"),
            };
            let answer = self.consume_response_stream(response, request_id).await?;
            return Ok(answer);
        }
        Err(McpError::Invalid(format!(
            "unsupported MCP response content type: {content_type}"
        )))
    }

    /// Waits for the next out-of-band event (GET-stream messages, errors,
    /// close), or `None` at the end.
    pub async fn next_event(&mut self) -> Option<TransportEvent> {
        self.events.recv().await
    }

    /// Closes the connection: aborts streams and, best effort with a 1 s
    /// budget, DELETEs the session.
    pub async fn close(&mut self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // The session guard must not live through the body: `headers()`
        // locks the same mutex (a temporary in an `if let` scrutinee
        // would deadlock).
        let session = self.session_id.lock().expect("session").clone();
        if session.is_some() {
            let request = self
                .client
                .delete(&self.options.url)
                .headers(self.headers(&[]))
                .timeout(Duration::from_secs(1))
                .send()
                .await;
            let _ = request;
        }
        let _ = self.event_sink.send(TransportEvent::Closed);
    }

    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().expect("session") = Some(session.to_string());
        }
    }

    async fn check_response(
        &self,
        response: reqwest::Response,
    ) -> Result<reqwest::Response, McpError> {
        let status = response.status().as_u16();
        if response.status().is_success() {
            return Ok(response);
        }
        let body = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(MAX_ERROR_BODY_BYTES)
            .collect::<String>();
        if status == 401 {
            return Err(McpError::AuthRequired { body });
        }
        if status == 404 && self.session_id.lock().expect("session").is_some() {
            return Err(McpError::SessionExpired(body));
        }
        let snippet = body
            .trim()
            .chars()
            .take(ERROR_MESSAGE_BODY_CHARS)
            .collect::<String>();
        Err(McpError::Invalid(format!(
            "MCP HTTP request failed with status {status}{snippet}",
        )))
    }

    /// Reads the SSE stream answering one request until its response
    /// arrives, resuming with GET + `Last-Event-ID` when the stream ends
    /// or breaks after the server assigned event ids (the server may
    /// close response streams at will).
    async fn consume_response_stream(
        &self,
        response: reqwest::Response,
        request_id: JsonRpcId,
    ) -> Result<Option<JsonRpcMessage>, McpError> {
        let mut cursor = StreamCursor::default();
        let mut answered: Option<JsonRpcMessage> = None;
        let mut stream = Some(response);
        let mut attempt: u32 = 0;
        loop {
            if let Some(response) = stream.take() {
                let sink = self.event_sink.clone();
                let id = request_id.clone();
                let outcome =
                    read_response_events(response, &mut cursor, &mut |message: JsonRpcMessage| {
                        if let JsonRpcMessage::Response(answer) = &message {
                            if answer.id() == &id {
                                return Some(message.clone());
                            }
                        }
                        let _ = sink.send(TransportEvent::Message(message));
                        None
                    })
                    .await;
                if let Ok(Some(answer)) = outcome {
                    answered = Some(answer);
                }
            }
            if answered.is_some() || self.closed.load(Ordering::SeqCst) {
                return Ok(answered);
            }
            if cursor.last_event_id.is_none() || attempt >= self.options.reconnect_max_retries {
                break;
            }
            if cursor.received {
                attempt = 0;
            }
            cursor.received = false;
            let delay = cursor
                .retry_ms
                .unwrap_or_else(|| self.reconnect_delay(attempt));
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            match self.open_sse_stream(cursor.last_event_id.as_deref()).await {
                Ok(next) => stream = next,
                Err(_) => continue,
            }
        }
        if self.closed.load(Ordering::SeqCst) {
            return Ok(None);
        }
        // The stream failed without answering: an internal-error response
        // ends the caller's wait, as upstream emits.
        Ok(Some(JsonRpcMessage::Response(JsonRpcResponse::Error {
            jsonrpc: "2.0".to_string(),
            id: request_id,
            error: crate::protocol::jsonrpc::JsonRpcErrorObject {
                code: INTERNAL_ERROR,
                message: "MCP response stream failed: stream ended without a response".to_string(),
                data: None,
            },
        })))
    }

    fn start_get_stream(&self) {
        if !self.options.open_get_stream || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let sink = self.event_sink.clone();
        let this = SharedSelf {
            client: self.client.clone(),
            url: self.options.url.clone(),
            headers: self.options.headers.clone(),
            protocol_version: Arc::new(Mutex::new(
                self.protocol_version.lock().expect("version").clone(),
            )),
            token: Arc::new(Mutex::new(self.last_token.lock().expect("token").clone())),
        };
        tokio::spawn(async move {
            // The task ends when the transport closes: the channel's
            // receiver is dropped, so the next send fails and the loop
            // exits on its error path.
            run_get_stream(this, sink).await;
        });
    }

    fn reconnect_delay(&self, attempt: u32) -> u64 {
        (self
            .options
            .reconnect_initial_delay_ms
            .saturating_mul(1 << attempt.min(16)))
        .min(self.options.reconnect_max_delay_ms)
    }

    /// Opens a GET SSE stream; `Ok(None)` when the server answers 405
    /// (it offers no GET stream).
    async fn open_sse_stream(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<Option<reqwest::Response>, McpError> {
        let mut request = self
            .client
            .get(&self.options.url)
            .headers(self.headers(&[("accept", "text/event-stream")]));
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }
        let response = request
            .send()
            .await
            .map_err(|error| McpError::Spawn(error.to_string()))?;
        if response.status().as_u16() == 405 {
            return Ok(None);
        }
        let response = self.check_response(response).await?;
        self.capture_session(&response);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        if content_type != "text/event-stream" {
            return Err(McpError::Invalid(format!(
                "unsupported MCP GET response content type: {content_type}"
            )));
        }
        Ok(Some(response))
    }
}

/// The SSE line-reading state one stream needs, shared between chunks.
#[derive(Default)]
struct LineAssembler {
    buffer: String,
    done: bool,
}

impl LineAssembler {
    /// Yields complete lines from `chunk`; a trailing partial stays
    /// buffered. The stream's final partial line is yielded once
    /// `finish` is called.
    fn push(&mut self, chunk: &str, lines: &mut Vec<String>) {
        self.buffer.push_str(chunk);
        while let Some(newline) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=newline).collect();
            lines.push(line.trim_end_matches('\n').to_string());
        }
    }

    fn finish(&mut self, lines: &mut Vec<String>) {
        if !self.buffer.is_empty() {
            lines.push(std::mem::take(&mut self.buffer));
        }
        self.done = true;
    }
}

/// Applies one SSE line to the cursor; `Some(event)` when the line
/// dispatched an event (a blank line with data collected).
fn apply_sse_line(
    cursor: &mut StreamCursor,
    line: &str,
    data_lines: &mut Vec<String>,
) -> Option<SseEvent> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.is_empty() {
        if data_lines.is_empty() {
            return None;
        }
        let data = data_lines.join("\n");
        data_lines.clear();
        cursor.received = true;
        return Some(SseEvent {
            event: cursor.event_name.take(),
            data,
            id: cursor.last_event_id.clone(),
        });
    }
    if line.starts_with(':') {
        return None;
    }
    let (field, value) = match line.find(':') {
        Some(colon) => {
            let value = &line[colon + 1..];
            (&line[..colon], value.strip_prefix(' ').unwrap_or(value))
        }
        None => (line, ""),
    };
    match field {
        "data" => data_lines.push(value.to_string()),
        "event" => cursor.event_name = Some(value.to_string()),
        "id" if !value.contains('\0') => cursor.last_event_id = Some(value.to_string()),
        "retry" => {
            if let Ok(delay) = value.parse::<u64>() {
                cursor.retry_ms = Some(delay);
            }
        }
        _ => {}
    }
    None
}

/// Decodes an SSE event into a JSON-RPC message; non-message events and
/// parse failures are reported as transport errors, as upstream does.
fn decode_message(event: &SseEvent) -> Option<JsonRpcMessage> {
    if event.data.trim().is_empty() {
        return None;
    }
    if let Some(name) = &event.event {
        if name != "message" {
            return None;
        }
    }
    serde_json::from_str::<Value>(&event.data)
        .map_err(|error| error.to_string())
        .and_then(|value| JsonRpcMessage::parse(value).map_err(|error| error.to_string()))
        .ok()
}

/// The GET stream task's view of the transport.
struct SharedSelf {
    client: reqwest::Client,
    url: String,
    headers: Vec<(String, String)>,
    protocol_version: Arc<Mutex<String>>,
    token: Arc<Mutex<Option<String>>>,
}

async fn run_get_stream(this: SharedSelf, sink: mpsc::UnboundedSender<TransportEvent>) {
    let mut cursor = StreamCursor::default();
    let mut attempt: u32 = 0;
    loop {
        match open_get_stream(&this, cursor.last_event_id.as_deref()).await {
            Ok(Some(response)) => {
                let opened_at = std::time::Instant::now();
                let forwarding_sink = sink.clone();
                let outcome =
                    read_response_events(response, &mut cursor, &mut |message: JsonRpcMessage| {
                        let _ = forwarding_sink.send(TransportEvent::Message(message));
                        None
                    })
                    .await;
                if let Err(error) = outcome {
                    if !error_is_transient_text(&error) {
                        // The receiver gone (transport closed) is the
                        // quiet exit; anything else is reported.
                        let _ = sink.send(TransportEvent::Error(error));
                        return;
                    }
                }
                if cursor.received
                    || opened_at.elapsed() > Duration::from_millis(DEFAULT_RECONNECT_MAX_DELAY_MS)
                {
                    attempt = 0;
                }
            }
            Ok(None) => return, // 405: the server offers no GET stream
            Err(error) => {
                if !error_is_transient(&error) {
                    let _ = sink.send(TransportEvent::Error(error.to_string()));
                    return;
                }
            }
        }
        if sink.is_closed() {
            return;
        }
        cursor.received = false;
        if attempt >= 5 {
            let _ = sink.send(TransportEvent::Error(
                "MCP server-to-client stream dropped and could not be reopened".to_string(),
            ));
            return;
        }
        let delay = cursor
            .retry_ms
            .unwrap_or_else(|| (1000u64.saturating_mul(1 << attempt.min(16))).min(30_000));
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
}

/// Reads one SSE response body to its end, applying lines to `cursor` and
/// handing every decoded message to `on_message` (whose `Some` return
/// stops the read with that message — the response a caller waits for).
async fn read_response_events(
    response: reqwest::Response,
    cursor: &mut StreamCursor,
    on_message: &mut (dyn FnMut(JsonRpcMessage) -> Option<JsonRpcMessage> + Send),
) -> Result<Option<JsonRpcMessage>, String> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut assembler = LineAssembler::default();
    let mut data_lines: Vec<String> = Vec::new();
    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                let chunk = String::from_utf8_lossy(&bytes).into_owned();
                let mut lines = Vec::new();
                assembler.push(&chunk, &mut lines);
                for line in lines {
                    if let Some(event) = apply_sse_line(cursor, &line, &mut data_lines) {
                        if let Some(message) = decode_message(&event) {
                            if let Some(answer) = on_message(message) {
                                return Ok(Some(answer));
                            }
                        }
                    }
                }
            }
            Some(Err(error)) => return Err(error.to_string()),
            None => {
                let mut lines = Vec::new();
                assembler.finish(&mut lines);
                for line in lines {
                    if let Some(event) = apply_sse_line(cursor, &line, &mut data_lines) {
                        if let Some(message) = decode_message(&event) {
                            if let Some(answer) = on_message(message) {
                                return Ok(Some(answer));
                            }
                        }
                    }
                }
                return Ok(None);
            }
        }
    }
}

async fn open_get_stream(
    this: &SharedSelf,
    last_event_id: Option<&str>,
) -> Result<Option<reqwest::Response>, McpError> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &this.headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::try_from(name.as_str()),
            reqwest::header::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
    if let Ok(value) = reqwest::header::HeaderValue::try_from(
        this.protocol_version.lock().expect("version").as_str(),
    ) {
        headers.insert("MCP-Protocol-Version", value);
    }
    if let Some(token) = this.token.lock().expect("token").as_deref() {
        if let Ok(value) = reqwest::header::HeaderValue::try_from(format!("Bearer {token}")) {
            headers.insert("Authorization", value);
        }
    }
    let mut request = this.client.get(&this.url).headers(headers);
    if let Some(id) = last_event_id {
        request = request.header("Last-Event-ID", id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| McpError::Spawn(error.to_string()))?;
    if response.status().as_u16() == 405 {
        return Ok(None);
    }
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(McpError::Invalid(format!(
            "MCP GET stream failed with status {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    Ok(Some(response))
}

/// The same rule over the raw error text (the GET stream reader reports
/// strings).
fn error_is_transient_text(error: &str) -> bool {
    error.contains("status 408")
        || error.contains("status 429")
        || (500..=599).any(|status| error.contains(&format!("status {status}")))
}

/// Network failures and transient statuses are retried; auth, session and
/// protocol errors are not.
fn error_is_transient(error: &McpError) -> bool {
    match error {
        McpError::Spawn(_) => true,
        McpError::Invalid(text) => error_is_transient_text(text),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_lines_join_and_dispatch() {
        // data: lines join with \n; a blank line dispatches; id is kept.
        let mut cursor = StreamCursor::default();
        let mut data = Vec::new();
        assert!(apply_sse_line(&mut cursor, "id: 7", &mut data).is_none());
        assert!(apply_sse_line(&mut cursor, "data: one", &mut data).is_none());
        assert!(apply_sse_line(&mut cursor, "data: two", &mut data).is_none());
        let event = apply_sse_line(&mut cursor, "", &mut data).expect("dispatched");
        assert_eq!(event.data, "one\ntwo");
        assert_eq!(event.id.as_deref(), Some("7"));
    }

    #[test]
    fn comments_and_ignored_fields() {
        let mut cursor = StreamCursor::default();
        let mut data = Vec::new();
        assert!(apply_sse_line(&mut cursor, ": heartbeat", &mut data).is_none());
        assert!(apply_sse_line(&mut cursor, "retry: 2500", &mut data).is_none());
        assert_eq!(cursor.retry_ms, Some(2500));
        assert!(apply_sse_line(&mut cursor, "event: ping", &mut data).is_none());
        // No data collected: a blank line dispatches nothing.
        assert!(apply_sse_line(&mut cursor, "", &mut data).is_none());
    }

    #[tokio::test]
    async fn a_stream_parses_end_to_end() {
        let stream = tokio::io::BufReader::new(
            "id: 1\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n: keepalive\ndata: {}\n\n"
                .as_bytes(),
        );
        let mut events = Vec::new();
        consume_sse_stream(stream, |event| events.push(event))
            .await
            .expect("consume");
        assert_eq!(events.len(), 2);
        assert!(events[0].data.contains("\"result\""));
        assert_eq!(events[0].id.as_deref(), Some("1"));
    }

    #[test]
    fn transient_errors_retry() {
        assert!(error_is_transient(&McpError::Spawn("connect".into())));
        assert!(error_is_transient(&McpError::Invalid(
            "MCP HTTP request failed with status 503".into()
        )));
        assert!(!error_is_transient(&McpError::Invalid(
            "MCP HTTP request failed with status 404: gone".into()
        )));
        assert!(!error_is_transient(&McpError::AuthRequired {
            body: String::new()
        }));
    }

    /// Header assembly: config headers, session, version and token.
    #[test]
    fn headers_carry_session_version_and_token() {
        let transport = HttpTransport::new(HttpOptions {
            url: "https://example.invalid/mcp".to_string(),
            headers: vec![("X-Custom".to_string(), "yes".to_string())],
            ..Default::default()
        });
        transport.set_protocol_version("2025-06-18");
        transport.set_token(Some("tok".to_string()));
        *transport.session_id.lock().unwrap() = Some("sess".to_string());
        let headers = transport.headers(&[("accept", "text/event-stream")]);
        assert_eq!(headers.get("x-custom").unwrap(), "yes");
        assert_eq!(headers.get("mcp-session-id").unwrap(), "sess");
        assert_eq!(headers.get("mcp-protocol-version").unwrap(), "2025-06-18");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer tok");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
    }
}
