//! Links: ordered, bidirectional frame channels, as upstream's
//! `chord/src/link.ts`.
//!
//! - [`Link::memory_pair`]: two linked ends in one process (tests).
//! - [`websocket`]: one JSON text message per frame. Packed binary messages
//!   (`[u32 BE envelope length][envelope JSON with z][raw-deflated payload]`)
//!   are decoded when accepted, and never sent.
//! - [`ndjson`]: Chord's Node IPC encoding as lines, `{"chord2": frame}`, for
//!   process links; other lines carry control messages. Unwindowed.
//!
//! A link reports the size of every frame it sends (the JSON text's length in
//! UTF-16 code units), except NDJSON, which reports none and so is never
//! windowed. Closing a link sends `bye` with the reason; a `bye` received ends
//! it with the peer's reason.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::object::BoxFuture;
use crate::wire::{text_size, Frame};

/// How a link puts frames on its medium. Implement it to build a link over
/// another medium, or to observe one (tests tap links this way).
pub trait Transport: Send + Sync + 'static {
    /// Sends one frame. Returns its size when the medium counts sizes;
    /// `None` keeps the link out of flow control.
    fn transmit(&self, frame: &Frame) -> Option<usize>;
    /// The link ended: release the medium. Frames sent before must still
    /// reach the peer, in order.
    fn shutdown(&self);
    /// Resolves once what was sent before `shutdown` has been written.
    fn flushed(&self) -> BoxFuture<()> {
        Box::pin(async {})
    }
}

/// What a link hands its connection.
#[derive(Debug)]
pub(crate) enum Inbound {
    Frame(Value),
    Ended(String),
}

enum Inbox {
    Buffer(Vec<Inbound>),
    Attached(Box<dyn Fn(Inbound) + Send + Sync>),
}

struct Core {
    id: u64,
    closed: AtomicBool,
    transport: Box<dyn Transport>,
    inbox: Mutex<Inbox>,
}

impl Core {
    fn push(&self, inbound: Inbound) {
        let mut inbox = self.inbox.lock().expect("link inbox");
        match &mut *inbox {
            Inbox::Buffer(buffer) => buffer.push(inbound),
            Inbox::Attached(sink) => sink(inbound),
        }
    }

    /// Marks the link ended and tells its connection once.
    fn end(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.transport.shutdown();
        self.push(Inbound::Ended(reason.to_owned()));
    }

    fn close(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.transport.transmit(&Frame::Bye {
            reason: Some(reason.to_owned()),
        });
        self.transport.shutdown();
        self.push(Inbound::Ended(reason.to_owned()));
    }
}

static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

/// One end of a link. Cloning shares the end: a route may hand the same link
/// out again, and the host finds the connection it already runs on it.
#[derive(Clone)]
pub struct Link {
    core: Arc<Core>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("id", &self.core.id)
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// The receiving side of a link, for its transport: frames and the end.
#[derive(Clone)]
pub struct Delivery {
    core: Weak<Core>,
}

impl Link {
    /// A link over `transport`, which gets the [`Delivery`] it reports what
    /// arrives to.
    pub fn new<T: Transport>(transport: impl FnOnce(Delivery) -> T) -> Link {
        let core = Arc::new_cyclic(|weak: &Weak<Core>| {
            let delivery = Delivery { core: weak.clone() };
            Core {
                id: NEXT_LINK.fetch_add(1, Ordering::Relaxed),
                closed: AtomicBool::new(false),
                transport: Box::new(transport(delivery)),
                inbox: Mutex::new(Inbox::Buffer(Vec::new())),
            }
        });
        Link { core }
    }

    /// Two connected in-memory ends. Each frame crosses as JSON text, so it is
    /// detached from the sender, and its size is that text's length.
    pub fn memory_pair() -> (Link, Link) {
        let left_peer = Arc::new(OnceLock::<Delivery>::new());
        let right_peer = Arc::new(OnceLock::<Delivery>::new());
        let left = Link::new(|_| MemoryTransport {
            peer: left_peer.clone(),
        });
        let right = Link::new(|delivery| {
            let _ = left_peer.set(delivery);
            MemoryTransport {
                peer: right_peer.clone(),
            }
        });
        let _ = right_peer.set(Delivery {
            core: Arc::downgrade(&left.core),
        });
        (left, right)
    }

    /// A process-unique id.
    pub fn id(&self) -> u64 {
        self.core.id
    }

    pub fn is_closed(&self) -> bool {
        self.core.closed.load(Ordering::SeqCst)
    }

    /// Sends a frame; `None` once closed, or when the medium counts no sizes.
    pub fn send(&self, frame: &Frame) -> Option<usize> {
        if self.is_closed() {
            return None;
        }
        self.core.transport.transmit(frame)
    }

    /// Closes both ends: the peer gets a `bye` with `reason`.
    pub fn close(&self, reason: &str) {
        self.core.close(reason);
    }

    /// Resolves once a closed link has written everything it sent.
    pub async fn flushed(&self) {
        self.core.transport.flushed().await;
    }

    /// Hands everything that arrives, buffered or later, to `sink`.
    pub(crate) fn attach(&self, sink: impl Fn(Inbound) + Send + Sync + 'static) {
        let mut inbox = self.core.inbox.lock().expect("link inbox");
        if let Inbox::Buffer(buffer) = &mut *inbox {
            for inbound in buffer.drain(..) {
                sink(inbound);
            }
        }
        *inbox = Inbox::Attached(Box::new(sink));
    }

    pub(crate) fn same(&self, other: &Link) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }
}

impl Delivery {
    fn core(&self) -> Option<Arc<Core>> {
        self.core.upgrade()
    }

    /// A parsed frame from the medium. A `bye` ends the link with its reason;
    /// anything after the end is dropped.
    pub fn deliver(&self, frame: Value) {
        let Some(core) = self.core() else { return };
        if core.closed.load(Ordering::SeqCst) {
            return;
        }
        if frame.get("t").and_then(Value::as_str) == Some("bye") {
            let reason = frame
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("peer closed");
            core.end(reason);
            return;
        }
        core.push(Inbound::Frame(frame));
    }

    /// A frame as JSON text, maybe with a packed payload (`z: [field,
    /// base64]`). Anything that is not a frame closes the link with
    /// `protocol error`.
    pub fn deliver_text(&self, text: &str) {
        match parse_frame(text) {
            Ok(frame) => self.deliver(frame),
            Err(()) => self.close("protocol error"),
        }
    }

    /// The medium ended.
    pub fn end(&self, reason: &str) {
        if let Some(core) = self.core() {
            core.end(reason);
        }
    }

    /// Closes the link from the medium's side (a protocol error).
    pub fn close(&self, reason: &str) {
        if let Some(core) = self.core() {
            core.close(reason);
        }
    }

    pub fn is_closed(&self) -> bool {
        self.core()
            .map(|core| core.closed.load(Ordering::SeqCst))
            .unwrap_or(true)
    }
}

struct MemoryTransport {
    peer: Arc<OnceLock<Delivery>>,
}

impl Transport for MemoryTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        let text = frame.to_text();
        if let Some(peer) = self.peer.get() {
            peer.deliver(serde_json::from_str(&text).expect("a frame's own text parses"));
        }
        Some(text_size(&text))
    }

    fn shutdown(&self) {
        if let Some(peer) = self.peer.get() {
            peer.end("peer closed");
        }
    }
}

// ─── payload packing ────────────────────────────────────────────────────────

const PAYLOAD_FIELDS: [&str; 4] = ["args", "value", "snap", "reset"];

fn inflate(bytes: &[u8]) -> Result<Value, ()> {
    let mut text = String::new();
    flate2::read::DeflateDecoder::new(bytes)
        .read_to_string(&mut text)
        .map_err(|_| ())?;
    serde_json::from_str(&text).map_err(|_| ())
}

/// For object transports: a packed payload travels as `z: [field, base64]`.
fn unpack_object(mut value: Value) -> Result<Value, ()> {
    let Some(object) = value.as_object_mut() else {
        return Ok(value);
    };
    let Some(z) = object.remove("z") else {
        return Ok(value);
    };
    let field = z.get(0).and_then(Value::as_str).ok_or(())?;
    let encoded = z.get(1).and_then(Value::as_str).ok_or(())?;
    if !PAYLOAD_FIELDS.contains(&field) {
        return Err(());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ())?;
    object.insert(field.to_owned(), inflate(&bytes)?);
    Ok(value)
}

/// A binary WebSocket message: `[u32 BE envelope length][envelope JSON, with
/// z: field][raw-deflated payload JSON]`.
pub(crate) fn unpack_binary(data: &[u8]) -> Result<Value, ()> {
    if data.len() < 4 {
        return Err(());
    }
    let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let envelope = data.get(4..4 + length).ok_or(())?;
    let mut envelope: Value = serde_json::from_slice(envelope).map_err(|_| ())?;
    let object = envelope.as_object_mut().ok_or(())?;
    let field = match object.remove("z") {
        Some(Value::String(field)) if PAYLOAD_FIELDS.contains(&field.as_str()) => field,
        _ => return Err(()),
    };
    object.insert(field, inflate(&data[4 + length..])?);
    Ok(envelope)
}

/// Upstream's `parseFrame`: JSON, unpacked, an object with a string `t`.
fn parse_frame(text: &str) -> Result<Value, ()> {
    let value: Value = serde_json::from_str(text).map_err(|_| ())?;
    let frame = unpack_object(value)?;
    match frame.get("t") {
        Some(Value::String(_)) => Ok(frame),
        _ => Err(()),
    }
}

// ─── WebSocket ──────────────────────────────────────────────────────────────

/// How a WebSocket link treats packed (binary) messages.
#[derive(Clone, Copy, Debug)]
pub struct WebSocketOptions {
    /// Accept packed frames from the peer (default true; false for a
    /// server's untrusted clients, as upstream's `acceptWebSockets`).
    pub accept_packed: bool,
}

impl Default for WebSocketOptions {
    fn default() -> Self {
        WebSocketOptions {
            accept_packed: true,
        }
    }
}

struct WebSocketTransport {
    outbox: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    written: watch::Receiver<bool>,
}

impl Transport for WebSocketTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        let text = frame.to_text();
        let size = text_size(&text);
        if let Some(outbox) = &*self.outbox.lock().expect("websocket outbox") {
            let _ = outbox.send(Message::text(text));
        }
        Some(size)
    }

    fn shutdown(&self) {
        // The writer sends what is queued, then a close frame.
        self.outbox.lock().expect("websocket outbox").take();
    }

    fn flushed(&self) -> BoxFuture<()> {
        let mut written = self.written.clone();
        Box::pin(async move {
            let _ = written.wait_for(|done| *done).await;
        })
    }
}

/// A link over an established WebSocket: one JSON text message per frame.
/// Spawns its reader and writer on the current tokio runtime.
pub fn websocket<S>(stream: WebSocketStream<S>, options: WebSocketOptions) -> Link
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut source) = stream.split();
    let (outbox, mut queued) = mpsc::unbounded_channel::<Message>();
    let (done, written) = watch::channel(false);
    tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.send(message).await.is_err() {
                let _ = done.send(true);
                return;
            }
        }
        let _ = sink.close().await;
        let _ = done.send(true);
    });
    let mut reader_delivery = None;
    let link = Link::new(|delivery| {
        reader_delivery = Some(delivery);
        WebSocketTransport {
            outbox: Mutex::new(Some(outbox)),
            written,
        }
    });
    let delivery = reader_delivery.expect("the transport was built");
    tokio::spawn(async move {
        while let Some(message) = source.next().await {
            match message {
                Ok(Message::Text(text)) => delivery.deliver_text(text.as_str()),
                Ok(Message::Binary(bytes)) => {
                    if !options.accept_packed {
                        delivery.close("protocol error");
                        continue;
                    }
                    match unpack_binary(&bytes) {
                        Ok(frame) => delivery.deliver(frame),
                        Err(()) => delivery.close("protocol error"),
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
            if delivery.core().is_none() {
                break;
            }
        }
        delivery.end("socket closed");
    });
    link
}

// ─── NDJSON ─────────────────────────────────────────────────────────────────

/// Writes control messages on an NDJSON link: `{"chord2control": message}`.
#[derive(Clone)]
pub struct ControlSender {
    lines: mpsc::UnboundedSender<String>,
}

impl ControlSender {
    pub fn send(&self, message: &Value) -> bool {
        let line = serde_json::json!({ "chord2control": message }).to_string();
        self.lines.send(line).is_ok()
    }
}

struct NdjsonTransport {
    lines: Mutex<Option<mpsc::UnboundedSender<String>>>,
    written: watch::Receiver<bool>,
}

impl Transport for NdjsonTransport {
    fn transmit(&self, frame: &Frame) -> Option<usize> {
        let line = format!("{{\"chord2\":{}}}", frame.to_text());
        if let Some(lines) = &*self.lines.lock().expect("ndjson lines") {
            let _ = lines.send(line);
        }
        // Unwindowed: both ends are local processes.
        None
    }

    fn shutdown(&self) {
        self.lines.lock().expect("ndjson lines").take();
    }

    fn flushed(&self) -> BoxFuture<()> {
        let mut written = self.written.clone();
        Box::pin(async move {
            let _ = written.wait_for(|done| *done).await;
        })
    }
}

/// A link over a byte stream pair, one JSON object per line: frames as
/// `{"chord2": frame}`, control messages as `{"chord2control": message}`
/// (handed to `control`), anything else ignored. Spawns its reader and writer
/// on the current tokio runtime.
pub fn ndjson<R, W>(
    reader: R,
    writer: W,
    control: Option<mpsc::UnboundedSender<Value>>,
) -> (Link, ControlSender)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (lines, mut queued) = mpsc::unbounded_channel::<String>();
    let control_lines = lines.clone();
    let mut writer = writer;
    let (done, written) = watch::channel(false);
    tokio::spawn(async move {
        while let Some(line) = queued.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                let _ = done.send(true);
                return;
            }
        }
        let _ = writer.shutdown().await;
        let _ = done.send(true);
    });
    let mut reader_delivery = None;
    let link = Link::new(|delivery| {
        reader_delivery = Some(delivery);
        NdjsonTransport {
            lines: Mutex::new(Some(lines)),
            written,
        }
    });
    let delivery = reader_delivery.expect("the transport was built");
    tokio::spawn(async move {
        let mut source = BufReader::new(reader).lines();
        loop {
            let line = match source.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) | Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(&line) else {
                delivery.close("protocol error");
                continue;
            };
            if let Some(frame) = message.remove("chord2") {
                match unpack_object(frame) {
                    Ok(frame) if frame.get("t").is_some_and(Value::is_string) => {
                        delivery.deliver(frame)
                    }
                    _ => delivery.close("protocol error"),
                }
            } else if let Some(message) = message.remove("chord2control") {
                if let Some(control) = &control {
                    let _ = control.send(message);
                }
            }
        }
        delivery.end("ipc disconnected");
    });
    (
        link,
        ControlSender {
            lines: control_lines,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use serde_json::json;
    use std::io::Write;

    fn deflate(text: &str) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn packed_binary_messages_and_z_fields_unpack() {
        let envelope = br#"{"t":"state","id":2,"z":"snap"}"#;
        let mut message = (envelope.len() as u32).to_be_bytes().to_vec();
        message.extend_from_slice(envelope);
        message.extend(deflate(r#"{"n":41}"#));
        assert_eq!(
            unpack_binary(&message).unwrap(),
            json!({"t": "state", "id": 2, "snap": {"n": 41}})
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(deflate("[1,2]"));
        let text = json!({"t": "call", "id": 3, "hold": 1, "member": "m", "z": ["args", encoded]})
            .to_string();
        assert_eq!(
            parse_frame(&text).unwrap(),
            json!({"t": "call", "id": 3, "hold": 1, "member": "m", "args": [1, 2]})
        );
        assert!(parse_frame(r#"{"t":"call","z":["nope","AA=="]}"#).is_err());
        assert!(parse_frame("[1]").is_err());
        let mut bad = 100u32.to_be_bytes().to_vec();
        bad.extend_from_slice(b"{}");
        assert!(unpack_binary(&bad).is_err());
    }

    #[test]
    fn a_memory_pair_delivers_in_order_and_closes_both_ends_with_the_reason() {
        let (a, b) = Link::memory_pair();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        b.attach(move |inbound| sink.lock().unwrap().push(format!("{inbound:?}")));
        assert_eq!(
            a.send(&Frame::Ping { n: 1 }),
            Some(r#"{"t":"ping","n":1}"#.len())
        );
        a.close("done");
        assert!(a.is_closed() && b.is_closed());
        assert_eq!(a.send(&Frame::Ping { n: 2 }), None);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].contains("ping"));
        assert_eq!(seen[1], r#"Ended("done")"#);
    }
}
