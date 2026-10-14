//! pi-env protocol 1's frames (upstream `packages/env/docs/protocol.md`), in
//! both directions: `u32 length | u8 type | u32 id | u32 jsonLength | json |
//! payload`, integers big-endian. The `noel-env serve` daemon and noel's
//! client share this codec.
//!
//! Ported from upstream pi's `packages/env/daemon/src/frame.rs`
//! (<https://github.com/earendil-works/pi>), Copyright (c) 2025 Mario
//! Zechner, MIT License (see LICENSE at the repository root).

use std::io::{self, Read, Write};

use serde_json::Value;

/// The protocol version `hello` asks for and reports.
pub const PROTOCOL: u64 = 1;

pub const REQUEST: u8 = 1;
pub const RESULT: u8 = 2;
pub const ERROR: u8 = 3;
pub const EVENT: u8 = 4;
pub const CANCEL: u8 = 5;
pub const PING: u8 = 6;

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Largest payload a response carries, leaving room for its JSON.
pub const MAX_PAYLOAD: usize = MAX_FRAME - 64 * 1024;

/// The daemon's first output, before any frame: the client discards
/// whatever a login shell printed before it.
pub fn sync_line(token: &str) -> String {
    format!("PI-ENV {token}\n")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub kind: u8,
    pub id: u32,
    pub json: Value,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: u8, id: u32, json: Value) -> Frame {
        Frame {
            kind,
            id,
            json,
            payload: Vec::new(),
        }
    }

    pub fn with_payload(kind: u8, id: u32, json: Value, payload: Vec<u8>) -> Frame {
        Frame {
            kind,
            id,
            json,
            payload,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// Read one frame; `None` at a clean end of input. JSON that does not parse
/// leaves the framing intact: the frame arrives with `Value::Null`, which the
/// daemon answers with `EINVAL`.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut length = [0u8; 4];
    match input.read_exact(&mut length) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length) as usize;
    if !(9..=MAX_FRAME).contains(&length) {
        return Err(invalid("frame length out of range"));
    }
    let mut body = vec![0u8; length];
    input.read_exact(&mut body)?;
    let kind = body[0];
    let id = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
    let json_length = u32::from_be_bytes([body[5], body[6], body[7], body[8]]) as usize;
    if 9 + json_length > length {
        return Err(invalid("JSON length out of range"));
    }
    let json = if json_length == 0 {
        Value::Object(Default::default())
    } else {
        serde_json::from_slice(&body[9..9 + json_length]).unwrap_or(Value::Null)
    };
    let payload = body[9 + json_length..].to_vec();
    Ok(Some(Frame {
        kind,
        id,
        json,
        payload,
    }))
}

/// One frame as bytes, for a writer that queues frames (noel's client).
pub fn encode_frame(kind: u8, id: u32, json: &Value, payload: &[u8]) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(json).map_err(|_| invalid("unserializable JSON"))?;
    let length = 9 + json.len() + payload.len();
    if length > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut bytes = Vec::with_capacity(4 + length);
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&json);
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

/// Write one frame and flush it.
pub fn write_frame(output: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let json = serde_json::to_vec(&frame.json).map_err(|_| invalid("unserializable JSON"))?;
    let length = 9 + json.len() + frame.payload.len();
    if length > MAX_FRAME {
        return Err(invalid("frame too large"));
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(length as u32).to_be_bytes());
    header.push(frame.kind);
    header.extend_from_slice(&frame.id.to_be_bytes());
    header.extend_from_slice(&(json.len() as u32).to_be_bytes());
    output.write_all(&header)?;
    output.write_all(&json)?;
    output.write_all(&frame.payload)?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_json_and_payload() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            &Frame::with_payload(RESULT, 7, json!({ "a": 1 }), vec![0, 255, 10]),
        )
        .unwrap();
        assert_eq!(
            encode_frame(RESULT, 7, &json!({ "a": 1 }), &[0, 255, 10]).unwrap(),
            buffer
        );
        let frame = read_frame(&mut buffer.as_slice()).unwrap().unwrap();
        assert_eq!((frame.kind, frame.id), (RESULT, 7));
        assert_eq!(frame.json, json!({ "a": 1 }));
        assert_eq!(frame.payload, vec![0, 255, 10]);
        assert!(read_frame(&mut &[][..]).unwrap().is_none());
    }

    #[test]
    fn lays_out_the_header_big_endian() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Frame::new(PING, 0, json!({}))).unwrap();
        assert_eq!(
            buffer,
            b"\x00\x00\x00\x0b\x06\x00\x00\x00\x00\x00\x00\x00\x02{}"
        );
    }

    #[test]
    fn keeps_framing_when_json_does_not_parse_and_rejects_bad_lengths() {
        let mut frame = vec![0, 0, 0, 12, REQUEST, 0, 0, 0, 3, 0, 0, 0, 3];
        frame.extend_from_slice(b"{x}");
        let read = read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!((read.id, read.json), (3, Value::Null));
        assert!(read_frame(&mut &[0u8, 0, 0, 8][..]).is_err());
        assert!(read_frame(&mut &[0u8, 0, 0, 9, 1, 0, 0, 0, 1, 0, 0, 0, 1][..]).is_err());
    }
}
