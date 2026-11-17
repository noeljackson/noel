//! Chord v2's binary codec (upstream `src/link/binary.ts`), the default of
//! every link: a kind byte (its high bit set when an origin follows as a
//! varint, the next when a trace carrier follows as JSON, the next on a call
//! that is old work of a reload), then the kind's fields as unsigned LEB128 varints,
//! length-prefixed UTF-8 strings and JSON, raw incarnations (16 bytes) and
//! fingerprints (8 bytes), parts behind their length doubled (plus one when
//! deflated), and byte segments only beside payloads marked as carrying
//! them. No outer length: links deliver whole messages.
//!
//! Encoding is byte for byte upstream's, including the padded five-byte
//! length of strings of 4096 UTF-16 units or more. JSON fields are written
//! with `serde_json`, whose keys are sorted; upstream writes them in
//! insertion order, and receivers parse them either way.

use serde_json::Value;

use super::message::{
    hex, unhex, Address, BoundEntry, CauseKind, FailedMember, Message, Part, StateEntry,
};

/// Kinds by their byte.
const KINDS: [&str; 18] = [
    "hello", "hold", "release", "call", "abort", "bound", "state", "failed", "result", "error",
    "gone", "retry", "loading", "ack", "closed", "ping", "pong", "revoke",
];

/// The kind byte's flag for an origin.
const ORIGIN: u8 = 0x80;
/// The kind byte's flag for a trace carrier (a hold's or a call's).
const TRACE: u8 = 0x40;
/// The kind byte's flag for a call that is old work of a reload; another
/// kind with it is an unknown kind.
const OLD: u8 = 0x20;
/// A payload's flags: present, and carrying byte segments.
const PRESENT: u64 = 1;
const SEGMENTS: u64 = 2;
/// A hold's optional fields, one bit each.
const PRINCIPAL: u8 = 1;
const INCARNATION: u8 = 2;
const CHAIN: u8 = 4;
const LIMIT: u8 = 8;
/// Strings from this many UTF-16 units get a padded five-byte length.
const LONG: usize = 4096;
const LENGTH_SLOT: usize = 5;

/// Why bytes are no message (a protocol error).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("binary: {0}")]
pub struct DecodeError(pub String);

fn fail<T>(message: &str) -> Result<T, DecodeError> {
    Err(DecodeError(message.to_owned()))
}

/// `message` as bytes.
pub fn encode(message: &Message) -> Vec<u8> {
    let mut writer = Writer::default();
    let kind = KINDS
        .iter()
        .position(|kind| *kind == message.kind())
        .expect("every kind has a byte") as u8;
    let (trace, old) = match message {
        Message::Hold { trace, .. } => (trace.as_ref(), false),
        Message::Call { trace, old, .. } => (trace.as_ref(), *old),
        _ => (None, false),
    };
    let origin = message.origin();
    writer.byte(
        kind | origin.map_or(0, |_| ORIGIN)
            | trace.map_or(0, |_| TRACE)
            | if old { OLD } else { 0 },
    );
    if let Some(origin) = origin {
        writer.varint(origin);
    }
    if let Some(trace) = trace {
        writer.json(trace);
    }
    encode_fields(message, &mut writer);
    writer.bytes
}

fn encode_fields(message: &Message, writer: &mut Writer) {
    match message {
        Message::Hello {
            protocol,
            incarnation,
            calls,
        } => {
            writer.varint(*protocol);
            writer.uuid(incarnation);
            writer.varint(*calls);
        }
        Message::Hold {
            hold,
            type_name,
            version,
            fingerprint,
            params,
            address,
            principal,
            incarnation,
            chain,
            limit,
            ..
        } => {
            writer.varint(*hold);
            writer.string(type_name);
            writer.varint(*version);
            writer.raw(&unhex(fingerprint).unwrap_or_else(|_| vec![0; 8]));
            writer.part(params);
            writer.string(&address.kind);
            writer.json(&address.params);
            writer.byte(
                principal.as_ref().map_or(0, |_| PRINCIPAL)
                    | incarnation.as_ref().map_or(0, |_| INCARNATION)
                    | chain.as_ref().map_or(0, |_| CHAIN)
                    | limit.map_or(0, |_| LIMIT),
            );
            if let Some(principal) = principal {
                writer.json(principal);
            }
            if let Some(incarnation) = incarnation {
                writer.uuid(incarnation);
            }
            if let Some(chain) = chain {
                writer.varint(chain.len() as u64);
                for instance in chain {
                    writer.uuid(instance);
                }
            }
            if let Some(limit) = limit {
                writer.varint(*limit);
            }
        }
        Message::Release { hold, .. } | Message::Loading { hold, .. } => writer.varint(*hold),
        Message::Retry { hold, hops, .. } => {
            writer.varint(*hold);
            writer.varint(*hops);
        }
        Message::Call {
            call,
            hold,
            method,
            args,
            segments,
            ..
        } => {
            writer.varint(*call);
            writer.varint(*hold);
            writer.varint(*method);
            writer.payload(args.as_ref(), segments.as_deref());
        }
        Message::Abort { call, .. } => writer.varint(*call),
        Message::Bound { holds, .. } => {
            writer.varint(holds.len() as u64);
            for entry in holds {
                writer.varint(entry.hold);
                writer.varint(entry.variant);
                writer.varint(entry.members.len() as u64);
                for (member, value) in &entry.members {
                    writer.varint(*member);
                    writer.part(value);
                }
                writer.varint(entry.failed.len() as u64);
                writer.failed_entries(&entry.failed);
            }
        }
        Message::State { holds, .. } => {
            writer.varint(holds.len() as u64);
            for entry in holds {
                writer.varint(entry.hold);
                writer.varint(entry.members.len() as u64);
                for (member, ops) in &entry.members {
                    writer.varint(*member);
                    writer.part(ops);
                }
                // The failure count doubled, plus one when retirements follow.
                writer.varint(entry.failed.len() as u64 * 2 + u64::from(!entry.retired.is_empty()));
                writer.failed_entries(&entry.failed);
                if !entry.retired.is_empty() {
                    writer.varint(entry.retired.len() as u64);
                    for member in &entry.retired {
                        writer.varint(*member);
                    }
                }
            }
        }
        Message::Failed {
            hold,
            cause,
            error,
            retry_in,
            ..
        } => {
            writer.varint(*hold);
            writer.byte(cause.code());
            writer.json(error);
            // 0 when absent, else one more than the delay.
            writer.varint(retry_in.map_or(0, |delay| delay + 1));
        }
        Message::Result {
            call,
            value,
            segments,
            ..
        } => {
            writer.varint(*call);
            writer.payload(value.as_ref(), segments.as_deref());
        }
        Message::Error { call, error, .. } => {
            writer.varint(*call);
            writer.json(error);
        }
        Message::Gone { hold, error, .. } => {
            writer.varint(*hold);
            writer.json(error);
        }
        Message::Revoke { revocation } => writer.json(revocation),
        Message::Ack { .. } | Message::Closed { .. } | Message::Ping | Message::Pong => {}
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn byte(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    fn varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push((value % 0x80) as u8 | 0x80);
            value /= 0x80;
        }
        self.bytes.push(value as u8);
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Text behind its UTF-8 length times `scale`: a padded five-byte
    /// varint for long text, as upstream writes it in place.
    fn text(&mut self, text: &str, scale: u64) {
        let length = text.len() as u64 * scale;
        if text.encode_utf16().count() >= LONG {
            let mut rest = length;
            for index in 0..LENGTH_SLOT {
                self.bytes
                    .push((rest % 0x80) as u8 | if index < LENGTH_SLOT - 1 { 0x80 } else { 0 });
                rest /= 0x80;
            }
        } else {
            self.varint(length);
        }
        self.raw(text.as_bytes());
    }

    fn string(&mut self, text: &str) {
        self.text(text, 1);
    }

    fn json(&mut self, value: &Value) {
        self.string(&value.to_string());
    }

    fn uuid(&mut self, text: &str) {
        self.raw(&unhex(&text.replace('-', "")).unwrap_or_else(|_| vec![0; 16]));
    }

    fn part(&mut self, part: &Part) {
        match part {
            Part::Text(text) => self.text(text, 2),
            Part::Bytes(bytes) => {
                self.varint(bytes.len() as u64 * 2);
                self.raw(bytes);
            }
            Part::Deflated(bytes) => {
                self.varint(bytes.len() as u64 * 2 + 1);
                self.raw(bytes);
            }
        }
    }

    fn payload(&mut self, part: Option<&Part>, segments: Option<&[Vec<u8>]>) {
        let Some(part) = part else {
            self.varint(0);
            return;
        };
        self.varint(if segments.is_some() {
            PRESENT | SEGMENTS
        } else {
            PRESENT
        });
        self.part(part);
        if let Some(segments) = segments {
            self.varint(segments.len() as u64);
            for segment in segments {
                self.varint(segment.len() as u64);
            }
            for segment in segments {
                self.raw(segment);
            }
        }
    }

    fn failed_entries(&mut self, failed: &[FailedMember]) {
        for failed in failed {
            self.varint(failed.member);
            self.json(&failed.error);
        }
    }
}

/// The message in `bytes`: every read is bounds-checked, and the bytes are
/// consumed exactly. Parts are not parsed or inflated.
pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    let mut reader = Reader { bytes, offset: 0 };
    let byte = reader.byte()?;
    let Some(kind) = KINDS.get(usize::from(byte & !(ORIGIN | TRACE | OLD))) else {
        return fail("an unknown kind");
    };
    if byte & OLD != 0 && *kind != "call" {
        return fail("an unknown kind");
    }
    let origin = if byte & ORIGIN != 0 {
        Some(reader.varint()?)
    } else {
        None
    };
    let trace = if byte & TRACE != 0 {
        Some(reader.json()?)
    } else {
        None
    };
    let mut message = decode_fields(kind, origin, &mut reader)?;
    if reader.offset != bytes.len() {
        return fail("bytes after the message");
    }
    // Kinds without an origin refuse one (upstream's schema check).
    if origin.is_some()
        && matches!(
            message,
            Message::Hello { .. } | Message::Revoke { .. } | Message::Ping | Message::Pong
        )
    {
        return fail("an origin on a kind without one");
    }
    // Only holds and calls carry a trace (upstream's schema check).
    match &mut message {
        Message::Hold { trace: slot, .. } => *slot = trace,
        Message::Call {
            trace: slot, old, ..
        } => {
            *slot = trace;
            *old = byte & OLD != 0;
        }
        _ if trace.is_some() => return fail("a trace on a kind without one"),
        _ => {}
    }
    Ok(message)
}

fn decode_fields(
    kind: &str,
    origin: Option<u64>,
    reader: &mut Reader<'_>,
) -> Result<Message, DecodeError> {
    Ok(match kind {
        "hello" => Message::Hello {
            protocol: reader.varint()?,
            incarnation: reader.uuid()?,
            calls: reader.varint()?,
        },
        "hold" => {
            let hold = reader.varint()?;
            let type_name = reader.string()?;
            let version = reader.varint()?;
            let fingerprint = hex(reader.raw(8)?);
            let params = reader.part()?;
            let address = Address {
                kind: reader.string()?,
                params: reader.json()?,
            };
            let flags = reader.byte()?;
            if flags > (PRINCIPAL | INCARNATION | CHAIN | LIMIT) {
                return fail("unknown hold flags");
            }
            let principal = if flags & PRINCIPAL != 0 {
                Some(reader.json()?)
            } else {
                None
            };
            let incarnation = if flags & INCARNATION != 0 {
                Some(reader.uuid()?)
            } else {
                None
            };
            let chain = if flags & CHAIN != 0 {
                let count = reader.count()?;
                Some(
                    (0..count)
                        .map(|_| reader.uuid())
                        .collect::<Result<_, _>>()?,
                )
            } else {
                None
            };
            let limit = if flags & LIMIT != 0 {
                Some(reader.varint()?)
            } else {
                None
            };
            Message::Hold {
                origin,
                hold,
                type_name,
                version,
                fingerprint,
                params,
                address,
                principal,
                incarnation,
                chain,
                limit,
                trace: None,
            }
        }
        "release" => Message::Release {
            origin,
            hold: reader.varint()?,
        },
        "loading" => Message::Loading {
            origin,
            hold: reader.varint()?,
        },
        "retry" => Message::Retry {
            origin,
            hold: reader.varint()?,
            hops: reader.varint()?,
        },
        "call" => {
            let (call, hold, method) = (reader.varint()?, reader.varint()?, reader.varint()?);
            let (args, segments) = reader.payload()?;
            Message::Call {
                origin,
                call,
                hold,
                method,
                args,
                segments,
                trace: None,
                old: false,
            }
        }
        "abort" => Message::Abort {
            origin,
            call: reader.varint()?,
        },
        "bound" => {
            let count = reader.count()?;
            let mut holds = Vec::with_capacity(count);
            for _ in 0..count {
                let (hold, variant) = (reader.varint()?, reader.varint()?);
                let members = reader.members()?;
                let failures = reader.count()?;
                holds.push(BoundEntry {
                    hold,
                    variant,
                    members,
                    failed: reader.failed_entries(failures)?,
                });
            }
            Message::Bound { origin, holds }
        }
        "state" => {
            let count = reader.count()?;
            let mut holds = Vec::with_capacity(count);
            for _ in 0..count {
                let hold = reader.varint()?;
                let members = reader.members()?;
                let flags = reader.varint()?;
                let failed =
                    reader.failed_entries(usize::try_from(flags / 2).unwrap_or(usize::MAX))?;
                let retired = if flags % 2 == 0 {
                    Vec::new()
                } else {
                    let count = reader.count()?;
                    (0..count)
                        .map(|_| reader.varint())
                        .collect::<Result<_, _>>()?
                };
                holds.push(StateEntry {
                    hold,
                    members,
                    failed,
                    retired,
                });
            }
            Message::State { origin, holds }
        }
        "failed" => {
            let hold = reader.varint()?;
            let Some(cause) = CauseKind::ALL.get(usize::from(reader.byte()?)).copied() else {
                return fail("an unknown failure kind");
            };
            let error = reader.json()?;
            let retry_in = reader.varint()?;
            Message::Failed {
                origin,
                hold,
                cause,
                error,
                retry_in: retry_in.checked_sub(1),
            }
        }
        "result" => {
            let call = reader.varint()?;
            let (value, segments) = reader.payload()?;
            Message::Result {
                origin,
                call,
                value,
                segments,
            }
        }
        "error" => Message::Error {
            origin,
            call: reader.varint()?,
            error: reader.json()?,
        },
        "gone" => Message::Gone {
            origin,
            hold: reader.varint()?,
            error: reader.json()?,
        },
        "revoke" => Message::Revoke {
            revocation: reader.json()?,
        },
        "ack" => Message::Ack { origin },
        // Its origin is the kind byte's; 0 without one, which the message
        // check refuses (as upstream).
        "closed" => Message::Closed {
            origin: origin.unwrap_or(0),
        },
        "ping" => Message::Ping,
        "pong" => Message::Pong,
        _ => return fail("an unknown kind"),
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let Some(byte) = self.bytes.get(self.offset) else {
            return fail("the message ends early");
        };
        self.offset += 1;
        Ok(*byte)
    }

    /// An unsigned LEB128 varint of at most 8 bytes, at most a safe
    /// integer (2^53 - 1).
    fn varint(&mut self) -> Result<u64, DecodeError> {
        const MAX_SAFE: u64 = (1 << 53) - 1;
        let mut value: u64 = 0;
        let mut scale: u64 = 1;
        for read in 1.. {
            if read > 8 {
                return fail("a varint does not fit");
            }
            let byte = self.byte()?;
            value = value.saturating_add(u64::from(byte & 0x7f).saturating_mul(scale));
            if value > MAX_SAFE {
                return fail("a varint does not fit");
            }
            if byte < 0x80 {
                return Ok(value);
            }
            scale = scale.saturating_mul(0x80);
        }
        unreachable!()
    }

    fn raw(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        if length > self.bytes.len() - self.offset {
            return fail("the message ends early");
        }
        let bytes = &self.bytes[self.offset..self.offset + length];
        self.offset += length;
        Ok(bytes)
    }

    fn length(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.varint()?).or_else(|_| fail("the message ends early"))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let length = self.length()?;
        let bytes = self.raw(length)?;
        String::from_utf8(bytes.to_vec()).or_else(|_| fail("invalid UTF-8"))
    }

    fn json(&mut self) -> Result<Value, DecodeError> {
        let text = self.string()?;
        serde_json::from_str(&text).or_else(|_| fail("invalid JSON"))
    }

    fn uuid(&mut self) -> Result<String, DecodeError> {
        let text = hex(self.raw(16)?);
        Ok(format!(
            "{}-{}-{}-{}-{}",
            &text[..8],
            &text[8..12],
            &text[12..16],
            &text[16..20],
            &text[20..]
        ))
    }

    /// A part as its received bytes, unparsed and not inflated.
    fn part(&mut self) -> Result<Part, DecodeError> {
        let prefix = self.varint()?;
        let bytes = self
            .raw(usize::try_from(prefix / 2).or_else(|_| fail("the message ends early"))?)?
            .to_vec();
        Ok(if prefix % 2 == 1 {
            Part::Deflated(bytes)
        } else {
            Part::Bytes(bytes)
        })
    }

    /// A count of elements that take at least a byte each.
    fn count(&mut self) -> Result<usize, DecodeError> {
        let count = self.length()?;
        if count > self.bytes.len() - self.offset {
            return fail("a count does not fit");
        }
        Ok(count)
    }

    fn members(&mut self) -> Result<Vec<(u64, Part)>, DecodeError> {
        let count = self.count()?;
        (0..count)
            .map(|_| Ok((self.varint()?, self.part()?)))
            .collect()
    }

    fn payload(&mut self) -> Result<Payload, DecodeError> {
        let flags = self.varint()?;
        if flags == 0 {
            return Ok((None, None));
        }
        if flags != PRESENT && flags != (PRESENT | SEGMENTS) {
            return fail("unknown payload flags");
        }
        let part = self.part()?;
        if flags == PRESENT {
            return Ok((Some(part), None));
        }
        let count = self.count()?;
        let lengths: Vec<usize> = (0..count)
            .map(|_| self.length())
            .collect::<Result<_, _>>()?;
        let segments = lengths
            .into_iter()
            .map(|length| self.raw(length).map(<[u8]>::to_vec))
            .collect::<Result<_, _>>()?;
        Ok((Some(part), Some(segments)))
    }

    fn failed_entries(&mut self, count: usize) -> Result<Vec<FailedMember>, DecodeError> {
        if count > self.bytes.len() - self.offset {
            return fail("a count does not fit");
        }
        (0..count)
            .map(|_| {
                Ok(FailedMember {
                    member: self.varint()?,
                    error: self.json()?,
                })
            })
            .collect()
    }
}

/// A decoded payload: the part and its segment boundaries, if any.
type Payload = (Option<Part>, Option<Vec<Vec<u8>>>);
