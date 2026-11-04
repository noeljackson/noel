//! Chord v2 transports: links over a WebSocket and over a byte stream.
//!
//! - [`websocket`]: one binary message per Chord message, as upstream's
//!   `webSocketLink`; a text message is a protocol error and ends the link.
//! - [`framed`]: noel's process links (server and worker over a pipe or a
//!   socket): each frame is `[u32 BE length][tag][payload]`, tag 0 a Chord
//!   message and tag 1 a JSON control message beside the link. Upstream's
//!   Node IPC has no counterpart outside Node.

use futures_util::future::{select, Either};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

use super::link::Link;

/// How long a peer has to end the socket after this side closed the link;
/// then the socket is dropped (as `ws`'s `closeTimeout`).
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// A link over an established WebSocket. Spawns its reader and writer on
/// the current tokio runtime.
pub fn websocket<S>(stream: WebSocketStream<S>) -> Link
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut source) = stream.split();
    let (outbound, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();
    let (delivered, inbound) = mpsc::unbounded_channel::<Vec<u8>>();
    let link = Link::new(outbound, inbound);
    tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.send(WsMessage::binary(message)).await.is_err() {
                return;
            }
        }
        // The link closed: a close frame.
        let _ = sink.close().await;
    });
    let closing_link = link.clone();
    tokio::spawn(async move {
        // A peer that never answers the close frame would otherwise hold the
        // socket forever.
        let mut closing = Box::pin(async move {
            closing_link.closed().await;
            tokio::time::sleep(CLOSE_GRACE).await;
        });
        loop {
            let message = match select(source.next(), closing.as_mut()).await {
                Either::Left((Some(message), _)) => message,
                Either::Left((None, _)) | Either::Right(_) => break,
            };
            match message {
                Ok(WsMessage::Binary(bytes)) => {
                    if delivered.send(bytes.to_vec()).is_err() {
                        break;
                    }
                }
                // Text frames are no Chord v2 messages: the link ends.
                Ok(WsMessage::Text(_)) | Ok(WsMessage::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        // Dropping `delivered` ends the link for its host.
    });
    link
}

/// A frame's tag: a Chord message.
const CHORD: u8 = 0;
/// A frame's tag: a JSON control message.
const CONTROL: u8 = 1;
/// The largest frame accepted (a process link is trusted; this only stops a
/// corrupt length from allocating without bound).
const MAX_FRAME: u32 = 1 << 30;

/// Writes control messages beside a framed link.
#[derive(Clone)]
pub struct ControlSender {
    control: mpsc::UnboundedSender<Vec<u8>>,
}

impl ControlSender {
    /// Sends `message`; `false` once the stream ended.
    pub fn send(&self, message: &Value) -> bool {
        self.control.send(message.to_string().into_bytes()).is_ok()
    }
}

/// A link over a byte stream pair: Chord messages as tag-0 frames, control
/// messages as tag-1 frames (handed to `control`). A malformed frame ends
/// the link. Spawns its reader and writer on the current tokio runtime.
pub fn framed<R, W>(
    reader: R,
    writer: W,
    control: Option<mpsc::UnboundedSender<Value>>,
) -> (Link, ControlSender)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outbound, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();
    let (control_out, mut control_queued) = mpsc::unbounded_channel::<Vec<u8>>();
    let (delivered, inbound) = mpsc::unbounded_channel::<Vec<u8>>();
    let link = Link::new(outbound, inbound);
    let mut writer = writer;
    tokio::spawn(async move {
        let mut control_open = true;
        loop {
            let (tag, payload) = tokio::select! {
                biased;
                message = control_queued.recv(), if control_open => match message {
                    Some(message) => (CONTROL, message),
                    None => {
                        control_open = false;
                        continue;
                    }
                },
                message = queued.recv() => match message {
                    Some(message) => (CHORD, message),
                    None => break,
                },
            };
            if write_frame(&mut writer, tag, &payload).await.is_err() {
                return;
            }
        }
        // Control messages sent before the link closed still go.
        while let Ok(message) = control_queued.try_recv() {
            if write_frame(&mut writer, CONTROL, &message).await.is_err() {
                return;
            }
        }
        let _ = writer.shutdown().await;
    });
    let closing_link = link.clone();
    let mut reader = reader;
    tokio::spawn(async move {
        loop {
            let frame = tokio::select! {
                frame = read_frame(&mut reader) => frame,
                _ = closing_link.closed() => break,
            };
            match frame {
                Ok(Some((CHORD, payload))) => {
                    if delivered.send(payload).is_err() {
                        break;
                    }
                }
                Ok(Some((CONTROL, payload))) => {
                    let Ok(message) = serde_json::from_slice::<Value>(&payload) else {
                        break;
                    };
                    if let Some(control) = &control {
                        let _ = control.send(message);
                    }
                }
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }
    });
    (
        link,
        ControlSender {
            control: control_out,
        },
    )
}

async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    tag: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let length = u32::try_from(payload.len() + 1)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(tag);
    frame.extend_from_slice(payload);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// The next frame, or `None` at the end of the stream.
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length);
    if length == 0 || length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a malformed frame length",
        ));
    }
    let mut frame = vec![0u8; length as usize];
    reader.read_exact(&mut frame).await?;
    let tag = frame.remove(0);
    Ok(Some((tag, frame)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn framed_links_carry_messages_and_control_both_ways() {
        let (a_read, b_write) = tokio::io::duplex(1024);
        let (b_read, a_write) = tokio::io::duplex(1024);
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let (a, a_control) = framed(a_read, a_write, None);
        let (b, _) = framed(b_read, b_write, Some(control_tx));
        let mut inbound = b.take_inbound().unwrap();
        a.send(vec![1, 2, 3]);
        assert!(a_control.send(&json!({"kind": "ready"})));
        a.send(vec![]);
        assert_eq!(inbound.recv().await, Some(vec![1, 2, 3]));
        assert_eq!(control_rx.recv().await, Some(json!({"kind": "ready"})));
        assert_eq!(inbound.recv().await, Some(vec![]));
        a.close();
        assert_eq!(inbound.recv().await, None);
    }

    #[tokio::test]
    async fn a_malformed_frame_ends_the_link() {
        let (a_read, mut peer) = tokio::io::duplex(64);
        let (_unused, a_write) = tokio::io::duplex(64);
        let (a, _) = framed(a_read, a_write, None);
        let mut inbound = a.take_inbound().unwrap();
        peer.write_all(&[0, 0, 0, 0]).await.unwrap();
        assert_eq!(inbound.recv().await, None);
    }
}
