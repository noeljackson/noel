//! A host's end of a link (upstream `src/link/channel.ts`): the handshake,
//! the link's codec, the check of every message (skipped on a trusted link)
//! and the heartbeat. A protocol error closes the link.
//!
//! The channel does no I/O: its owner feeds it the bytes that arrived, takes
//! the bytes to send from [`Channel::outgoing`], drives the heartbeat with
//! [`Channel::tick`] and tells it when the link closed. Time is passed in as
//! milliseconds, so tests drive it without a clock.
//!
//! Deflated parts are inflated synchronously when their owner opens them
//! (`super::part`), so no message ever waits for an inflate: upstream's
//! holding of later messages behind an asynchronous one has nothing to hold.

use super::binary;
use super::error::PROTOCOL_VERSION;
use super::message::Message;

/// A silent open link sends a `ping` after this many milliseconds.
pub const PING_AFTER_MS: u64 = 15_000;

/// A link silent this many milliseconds (the handshake included) closes.
pub const CLOSE_AFTER_MS: u64 = 30_000;

/// How many concurrent calls a side accepts by default (the call window).
pub const CALLS: u64 = 256;

/// How a link turns messages into bytes and back; both ends of a link use
/// the same one.
pub trait Codec: Send + Sync {
    fn name(&self) -> &'static str;
    fn encode(&self, message: &Message) -> Vec<u8>;
    /// An error for bytes that are no message of this codec.
    fn decode(&self, bytes: &[u8]) -> Result<Message, DecodeError>;
}

/// Why received bytes are no message (#178: a type, not a string).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The codec could not read them.
    #[error("{0}")]
    Codec(String),
    /// They read as a message that breaks the protocol's rules (checked on
    /// links that are not trusted).
    #[error("{0}")]
    Invalid(String),
}

/// The binary codec, the default.
#[derive(Debug, Clone, Copy, Default)]
pub struct BinaryCodec;

impl Codec for BinaryCodec {
    fn name(&self) -> &'static str {
        "binary"
    }

    fn encode(&self, message: &Message) -> Vec<u8> {
        binary::encode(message)
    }

    fn decode(&self, bytes: &[u8]) -> Result<Message, DecodeError> {
        binary::decode(bytes).map_err(|error| DecodeError::Codec(error.to_string()))
    }
}

/// What a channel is made with besides its codec.
#[derive(Debug, Clone)]
pub struct ChannelOptions {
    /// This host's incarnation, sent in `hello`.
    pub incarnation: String,
    /// How many concurrent calls this side accepts, sent in `hello`.
    pub calls: u64,
    /// Skips the message check (the codec's decode still runs).
    pub trusted: bool,
}

/// A protocol error: the link must close, and the error is reported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProtocolError(pub String);

/// What a received message gave the channel's owner.
#[derive(Debug, Clone, PartialEq)]
pub enum Received {
    /// The peer's `hello` arrived: the link is open.
    Opened,
    /// A checked message after the handshake (never `hello`, `ping` or
    /// `pong`).
    Message(Box<Message>),
    /// Nothing for the owner: a `ping` (answered) or `pong`, or bytes after
    /// the close.
    Nothing,
}

/// What the heartbeat asks of the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Nothing,
    /// The link was silent too long: close it.
    Close,
}

#[derive(Debug)]
enum State {
    /// Until the peer's `hello`, with what this end sent meanwhile.
    Greeting {
        queued: Vec<Vec<u8>>,
    },
    Open {
        peer: String,
        calls: u64,
    },
    Closed,
}

/// One end of a link. It sends `hello { protocol, incarnation, calls }`
/// first and nothing else until the peer's arrived. Every message is
/// decoded by the link's codec and, unless the link is trusted, checked; a
/// first message that is no `hello` of this codec, a second `hello`,
/// another protocol version, or a message failing a check is a protocol
/// error. Every received message stamps the time; [`Channel::tick`] pings a
/// silent open link and closes a link silent for too long. A `ping` is
/// answered with `pong` at once.
pub struct Channel {
    codec: Box<dyn Codec>,
    trusted: bool,
    state: State,
    last_received: u64,
    pinged: bool,
    outgoing: Vec<Vec<u8>>,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("codec", &self.codec.name())
            .field("state", &self.state)
            .finish()
    }
}

impl Channel {
    /// A channel made at `now` (milliseconds): its `hello` is the first of
    /// its outgoing bytes.
    pub fn new(codec: impl Codec + 'static, options: ChannelOptions, now: u64) -> Channel {
        let hello = codec.encode(&Message::Hello {
            protocol: PROTOCOL_VERSION,
            incarnation: options.incarnation,
            calls: options.calls,
        });
        Channel {
            codec: Box::new(codec),
            trusted: options.trusted,
            state: State::Greeting { queued: Vec::new() },
            last_received: now,
            pinged: false,
            outgoing: vec![hello],
        }
    }

    /// The peer's incarnation from its `hello`; empty before it arrived.
    pub fn peer(&self) -> &str {
        match &self.state {
            State::Open { peer, .. } => peer,
            _ => "",
        }
    }

    /// How many concurrent calls the peer accepts; 0 before its `hello`.
    pub fn peer_calls(&self) -> u64 {
        match &self.state {
            State::Open { calls, .. } => *calls,
            _ => 0,
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, State::Open { .. })
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }

    /// The bytes to send, in order, taken from the channel.
    pub fn outgoing(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outgoing)
    }

    /// Sends one message: before the handshake it waits, after the close it
    /// is dropped.
    pub fn send(&mut self, message: &Message) {
        match &mut self.state {
            State::Closed => {}
            State::Greeting { queued } => queued.push(self.codec.encode(message)),
            State::Open { .. } => self.outgoing.push(self.codec.encode(message)),
        }
    }

    /// The heartbeat at `now`: a link silent for `close` ms (the handshake
    /// included) closes; an open one silent for `ping` ms sends one `ping`.
    pub fn tick(&mut self, now: u64, ping: u64, close: u64) -> Tick {
        if self.is_closed() {
            return Tick::Nothing;
        }
        let silent = now.saturating_sub(self.last_received);
        if silent >= close {
            return Tick::Close;
        }
        if silent >= ping && !self.pinged && self.is_open() {
            self.pinged = true;
            self.outgoing.push(self.codec.encode(&Message::Ping));
        }
        Tick::Nothing
    }

    /// Bytes that arrived at `now`. An error is a protocol error: the
    /// channel is closed, and the owner closes the link and reports it.
    pub fn receive(&mut self, bytes: &[u8], now: u64) -> Result<Received, ProtocolError> {
        if self.is_closed() {
            return Ok(Received::Nothing);
        }
        self.last_received = now;
        self.pinged = false;
        let handled = self.handle(bytes);
        if handled.is_err() {
            self.closed();
        }
        handled
    }

    /// The link closed, from either end. Returns whether the channel was
    /// still live (so the owner handles the close once).
    pub fn closed(&mut self) -> bool {
        if self.is_closed() {
            return false;
        }
        self.state = State::Closed;
        self.outgoing.clear();
        true
    }

    fn handle(&mut self, bytes: &[u8]) -> Result<Received, ProtocolError> {
        if matches!(self.state, State::Greeting { .. }) {
            return self.greet(bytes);
        }
        let message = self
            .decode(bytes)
            .map_err(|error| ProtocolError(error.to_string()))?;
        match message {
            Message::Hello { .. } => Err(ProtocolError("handshake: a second hello".to_owned())),
            Message::Ping => {
                self.outgoing.push(self.codec.encode(&Message::Pong));
                Ok(Received::Nothing)
            }
            Message::Pong => Ok(Received::Nothing),
            message => Ok(Received::Message(Box::new(message))),
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Message, DecodeError> {
        let message = self.codec.decode(bytes)?;
        if !self.trusted {
            message.check().map_err(DecodeError::Invalid)?;
        }
        Ok(message)
    }

    /// The first message: the peer's `hello` in this link's codec and
    /// protocol version.
    fn greet(&mut self, bytes: &[u8]) -> Result<Received, ProtocolError> {
        let message = self.decode(bytes).map_err(|error| {
            ProtocolError(format!(
                "handshake: the first message is no {} message ({error})",
                self.codec.name()
            ))
        })?;
        let Message::Hello {
            protocol,
            incarnation,
            calls,
        } = message
        else {
            return Err(ProtocolError(format!(
                "handshake: {} before hello",
                message.kind()
            )));
        };
        if protocol != PROTOCOL_VERSION {
            return Err(ProtocolError(format!(
                "handshake: protocol {protocol}, this host speaks {PROTOCOL_VERSION}"
            )));
        }
        let State::Greeting { queued } = std::mem::replace(
            &mut self.state,
            State::Open {
                peer: incarnation,
                calls,
            },
        ) else {
            unreachable!("greeting");
        };
        self.outgoing.extend(queued);
        Ok(Received::Opened)
    }
}

#[cfg(test)]
mod tests {
    use super::super::check::uuidv7;
    use super::*;

    fn channel(now: u64) -> Channel {
        Channel::new(
            BinaryCodec,
            ChannelOptions {
                incarnation: uuidv7(),
                calls: CALLS,
                trusted: false,
            },
            now,
        )
    }

    /// Moves what `from` has to send into `to`.
    fn pump(from: &mut Channel, to: &mut Channel, now: u64) -> Vec<Received> {
        from.outgoing()
            .iter()
            .map(|bytes| to.receive(bytes, now).unwrap())
            .collect()
    }

    #[test]
    fn messages_wait_for_the_peers_hello() {
        let (mut a, mut b) = (channel(0), channel(0));
        let release = Message::Release {
            origin: None,
            hold: 3,
        };
        a.send(&release);
        let hello = a.outgoing();
        assert_eq!(hello.len(), 1, "only hello before the handshake");
        assert_eq!(b.receive(&hello[0], 0).unwrap(), Received::Opened);
        assert_eq!(pump(&mut b, &mut a, 1), [Received::Opened]);
        assert!(a.is_open());
        assert_eq!(a.peer_calls(), CALLS);
        assert_eq!(
            a.outgoing()
                .iter()
                .map(|bytes| binary::decode(bytes).unwrap())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&release)
        );
        a.send(&release);
        assert_eq!(
            pump(&mut a, &mut b, 1),
            [Received::Message(Box::new(release))]
        );
    }

    #[test]
    fn protocol_errors_close_the_channel() {
        let (mut a, mut b) = (channel(0), channel(0));
        let pong = binary::encode(&Message::Pong);
        assert_eq!(
            a.receive(&pong, 0).unwrap_err().0,
            "handshake: pong before hello"
        );
        assert!(a.is_closed());
        assert!(!a.closed());

        let mut a = channel(0);
        assert!(a
            .receive(&[0xff], 0)
            .unwrap_err()
            .0
            .starts_with("handshake: the first message is no binary message"));

        let mut a = channel(0);
        let old = binary::encode(&Message::Hello {
            protocol: 4,
            incarnation: uuidv7(),
            calls: 1,
        });
        assert_eq!(
            a.receive(&old, 0).unwrap_err().0,
            "handshake: protocol 4, this host speaks 1"
        );

        let mut a = channel(0);
        let hello = b.outgoing().remove(0);
        a.receive(&hello, 0).unwrap();
        assert_eq!(
            a.receive(&hello, 0).unwrap_err().0,
            "handshake: a second hello"
        );

        let mut a = channel(0);
        a.receive(&hello, 0).unwrap();
        let bad = binary::encode(&Message::Retry {
            origin: None,
            hold: 0,
            hops: 17,
        });
        assert_eq!(
            a.receive(&bad, 0).unwrap_err().0,
            "message: fails the message schema"
        );
    }

    #[test]
    fn a_trusted_channel_skips_the_check() {
        let mut a = Channel::new(
            BinaryCodec,
            ChannelOptions {
                incarnation: uuidv7(),
                calls: 1,
                trusted: true,
            },
            0,
        );
        let mut b = channel(0);
        pump(&mut b, &mut a, 0);
        let bad = Message::Retry {
            origin: None,
            hold: 0,
            hops: 17,
        };
        assert_eq!(
            a.receive(&binary::encode(&bad), 0).unwrap(),
            Received::Message(Box::new(bad))
        );
    }

    #[test]
    fn the_heartbeat_pings_once_then_closes() {
        let (mut a, mut b) = (channel(0), channel(0));
        pump(&mut b, &mut a, 0);
        pump(&mut a, &mut b, 0);
        assert_eq!(a.tick(14_999, PING_AFTER_MS, CLOSE_AFTER_MS), Tick::Nothing);
        assert!(a.outgoing().is_empty());
        a.tick(15_000, PING_AFTER_MS, CLOSE_AFTER_MS);
        a.tick(16_000, PING_AFTER_MS, CLOSE_AFTER_MS);
        // One ping; the peer answers it, which stamps the link again.
        assert_eq!(pump(&mut a, &mut b, 16_000), [Received::Nothing]);
        assert_eq!(pump(&mut b, &mut a, 16_000), [Received::Nothing]);
        assert_eq!(a.tick(31_000, PING_AFTER_MS, CLOSE_AFTER_MS), Tick::Nothing);
        assert_eq!(a.tick(46_000, PING_AFTER_MS, CLOSE_AFTER_MS), Tick::Close);
        // The handshake counts too.
        let mut c = channel(0);
        assert_eq!(c.tick(30_000, PING_AFTER_MS, CLOSE_AFTER_MS), Tick::Close);
    }

    #[test]
    fn nothing_goes_out_after_the_close() {
        let mut a = channel(0);
        assert!(a.closed());
        a.send(&Message::Ack { origin: None });
        assert!(a.outgoing().is_empty());
        assert_eq!(a.receive(&[0xff], 0).unwrap(), Received::Nothing);
    }
}
