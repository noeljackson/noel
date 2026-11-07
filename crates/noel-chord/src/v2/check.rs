//! The message schema check (upstream `checkMessage` against `MESSAGE`):
//! what the types of [`Message`] do not hold already. Ids and indexes are
//! non-negative by type and within 2^53 by the codecs; this checks origins,
//! versions, call windows and hop budgets, fingerprints and incarnations,
//! activation chains, principals and error shapes. Parts are not checked:
//! their owners parse them.

use serde_json::Value;

use super::error::check_wire_error;
use super::message::{FailedMember, Message};

/// The longest activation chain a hold carries.
pub const CHAIN: usize = 32;

/// The longest name a message carries (a type's, a host kind's).
pub const NAME_LENGTH: usize = 256;

/// The most keys a trace carrier has, and the longest key or value.
pub const CARRIER_KEYS: usize = 8;
pub const CARRIER_LENGTH: usize = 256;

/// A `retry`'s hop budget as the first host sends it.
pub const RETRY_HOPS: u64 = 16;

/// What a failed check reports, as upstream does.
pub const SCHEMA_FAILURE: &str = "message: fails the message schema";

impl Message {
    /// Checks the envelope after a decode (the wire is a trust boundary).
    pub fn check(&self) -> Result<(), String> {
        if valid(self) {
            Ok(())
        } else {
            Err(SCHEMA_FAILURE.to_owned())
        }
    }
}

fn valid(message: &Message) -> bool {
    if message.origin() == Some(0) {
        return false;
    }
    match message {
        Message::Hello {
            incarnation, calls, ..
        } => *calls >= 1 && is_incarnation(incarnation),
        Message::Hold {
            type_name,
            address,
            version,
            fingerprint,
            principal,
            incarnation,
            chain,
            trace,
            ..
        } => {
            is_name(type_name)
                && is_name(&address.kind)
                && trace.as_ref().is_none_or(is_carrier)
                && *version >= 1
                && fingerprint.len() == 16
                && fingerprint
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
                && principal.as_ref().is_none_or(|principal| {
                    principal.get("kind").is_some_and(Value::is_string) && principal.is_object()
                })
                && incarnation.as_deref().is_none_or(is_incarnation)
                && chain.as_ref().is_none_or(|chain| {
                    chain.len() <= CHAIN && chain.iter().all(|entry| is_incarnation(entry))
                })
        }
        Message::Retry { hops, .. } => (1..=RETRY_HOPS).contains(hops),
        Message::Bound { holds, .. } => holds.iter().all(|entry| failed_ok(&entry.failed)),
        Message::State { holds, .. } => holds.iter().all(|entry| failed_ok(&entry.failed)),
        Message::Failed { error, .. }
        | Message::Error { error, .. }
        | Message::Gone { error, .. } => check_wire_error(error).is_ok(),
        Message::Call { trace, .. } => trace.as_ref().is_none_or(is_carrier),
        Message::Release { .. }
        | Message::Abort { .. }
        | Message::Ack { .. }
        | Message::Closed { .. }
        | Message::Revoke { .. }
        | Message::Ping
        | Message::Pong
        | Message::Loading { .. }
        | Message::Result { .. } => true,
    }
}

fn is_name(name: &str) -> bool {
    name.chars().count() <= NAME_LENGTH
}

/// A trace carrier: an object of at most 8 strings, keys of printable ASCII
/// without spaces, values of printable ASCII, each at most 256 long.
fn is_carrier(trace: &Value) -> bool {
    let Some(fields) = trace.as_object() else {
        return false;
    };
    fields.len() <= CARRIER_KEYS
        && fields.iter().all(|(key, value)| {
            !key.is_empty()
                && key.len() <= CARRIER_LENGTH
                && key.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
                && value.as_str().is_some_and(|value| {
                    value.len() <= CARRIER_LENGTH
                        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                })
        })
}

fn failed_ok(failed: &[FailedMember]) -> bool {
    failed
        .iter()
        .all(|member| check_wire_error(&member.error).is_ok())
}

/// A uuid as text, lowercase hex in its five groups.
pub fn is_incarnation(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// A new uuidv7: 48 bits of milliseconds since the epoch, then random bits
/// with the version and variant set.
pub fn uuidv7() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes[6..]).expect("random bytes");
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn incarnations_are_lowercase_uuids() {
        let id = uuidv7();
        assert!(is_incarnation(&id), "{id}");
        assert_eq!(&id[14..15], "7");
        assert!(!is_incarnation(&id.to_uppercase()));
        assert!(!is_incarnation("0190a0b0-c0d0-7e0f-8a0b-0c0d0e0f101"));
    }

    #[test]
    fn the_check_refuses_what_the_schema_refuses() {
        let hello = |calls| Message::Hello {
            protocol: 1,
            incarnation: uuidv7(),
            calls,
        };
        assert!(hello(1).check().is_ok());
        assert_eq!(hello(0).check().unwrap_err(), SCHEMA_FAILURE);
        assert!(Message::Ack { origin: Some(0) }.check().is_err());
        assert!(Message::Closed { origin: 0 }.check().is_err());
        for (hops, ok) in [(0, false), (1, true), (16, true), (17, false)] {
            let retry = Message::Retry {
                origin: None,
                hold: 0,
                hops,
            };
            assert_eq!(retry.check().is_ok(), ok, "hops {hops}");
        }
        let error = |error| Message::Error {
            origin: None,
            call: 0,
            error,
        };
        assert!(
            error(json!({"kind": "plugin", "code": "c", "message": "m"}))
                .check()
                .is_ok()
        );
        assert!(
            error(json!({"kind": "plugin", "code": "c", "message": "m", "details": [1]}))
                .check()
                .is_ok()
        );
        assert!(error(json!({"kind": "chord", "error": 1})).check().is_ok());
        assert!(error(json!({"kind": "plugin", "code": "c"}))
            .check()
            .is_err());
        assert!(error(json!({"kind": "chord", "error": {}, "x": 1}))
            .check()
            .is_err());
        assert!(error(json!({"kind": "other"})).check().is_err());
    }
}
