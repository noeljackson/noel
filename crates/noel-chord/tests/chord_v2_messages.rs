//! Chord v2's binary codec against upstream at `CHORD2_REFERENCE_REF`
//! (`tests/fixtures/ts-parity/chord-v2-messages.json`, written by
//! `scripts/ts-parity/chord-v2-messages-fixture.ts` with upstream's
//! `binaryCodec`): every kind, origins, optional hold fields, deflated parts,
//! byte segments, non-ASCII text and a long part. Both directions: noel's
//! bytes are upstream's, and upstream's bytes decode to the same message.
//! The fixture's `checks` are upstream's message-check verdicts.

use std::path::Path;

use noel_chord::v2::binary::{decode, encode};
use noel_chord::v2::{ChordError, Message};
use serde_json::Value;

fn cases() -> Vec<(Value, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/ts-parity/chord-v2-messages.json");
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture")).unwrap();
    fixture["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            (
                case["message"].clone(),
                case["hex"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

#[test]
fn every_kind_is_covered() {
    let kinds: std::collections::BTreeSet<String> = cases()
        .iter()
        .map(|(message, _)| message["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds.len(), 18, "{kinds:?}");
}

#[test]
fn noel_encodes_as_upstream_does() {
    for (json, expected) in cases() {
        let message = Message::from_json(&json).expect("a message");
        assert_eq!(hex(&encode(&message)), expected, "{json}");
    }
}

#[test]
fn upstreams_bytes_decode_to_the_same_message() {
    for (json, bytes) in cases() {
        let decoded = decode(&unhex(&bytes)).unwrap_or_else(|error| panic!("{json}: {error}"));
        let expected = Message::from_json(&json).unwrap();
        let decoded = decoded.with_text_parts();
        assert_eq!(decoded, expected, "{json}");
        // As text again, a part encodes as its origin wrote it. (Received
        // bytes are written behind a plain length, as upstream writes them:
        // only text of 4096 units or more gets the padded one.)
        assert_eq!(hex(&encode(&decoded)), bytes, "re-encoding {json}");
    }
}

#[test]
fn truncated_or_padded_bytes_are_refused() {
    for (json, bytes) in cases() {
        let bytes = unhex(&bytes);
        if bytes.len() > 1 {
            assert!(decode(&bytes[..bytes.len() - 1]).is_err(), "short {json}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_err(), "long {json}");
    }
    assert!(decode(&[0x7f]).is_err(), "an unknown kind");
    assert!(decode(&[0x8f, 0x01]).is_err(), "an origin on ping");
}

/// The message check against upstream's `checkMessage` on what the binary
/// codec decodes: origins, call windows, hop budgets, versions, principals,
/// activation chains and error shapes.
#[test]
fn the_check_gives_upstreams_verdicts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/ts-parity/chord-v2-messages.json");
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture")).unwrap();
    let checks = fixture["checks"].as_array().unwrap();
    assert!(checks.len() >= 20);
    for case in checks {
        let message = Message::from_json(&case["message"]).expect("a message");
        let decoded = decode(&encode(&message)).expect("decodes");
        assert_eq!(
            decoded.check().is_ok(),
            case["ok"].as_bool().unwrap(),
            "{}",
            case["message"]
        );
    }
}

/// ChordError envelopes against upstream's `chordErrorFromJson`: the
/// error's JSON again, or upstream's reason for refusing it.
#[test]
fn chord_error_envelopes_are_checked_as_upstream_does() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/ts-parity/chord-v2-messages.json");
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture")).unwrap();
    for case in fixture["envelopes"].as_array().unwrap() {
        let rebuilt = ChordError::from_json(&case["input"]);
        match case.get("json") {
            Some(json) => assert_eq!(&rebuilt.unwrap().to_json(), json),
            None => assert_eq!(rebuilt.unwrap_err(), case["error"].as_str().unwrap()),
        }
    }
}

/// The kind byte's old-work flag (0x20) belongs to `call` only, and a trace
/// carrier (0x40) to `hold` and `call`: on another kind upstream's decoder
/// calls it an unknown kind, and its schema refuses the trace.
#[test]
fn old_and_trace_flags_on_other_kinds_are_refused() {
    // `release` (kind 2) of hold 7.
    assert!(decode(&[0x02, 0x07]).is_ok());
    assert!(decode(&[0x02 | 0x20, 0x07]).is_err());
    // A trace `{}` (length-prefixed JSON) on a release.
    assert!(decode(&[0x02 | 0x40, 0x02, b'{', b'}', 0x07]).is_err());
}
