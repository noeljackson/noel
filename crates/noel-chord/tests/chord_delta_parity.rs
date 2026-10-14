//! noel-chord against upstream Chord's goldens
//! (`tests/fixtures/ts-parity/chord-delta.json`, made by
//! `scripts/ts-parity/chord-delta-fixture.ts`): the delta codec, the
//! immutable applier, diff results, canonical JSON, `hash64`, and contract
//! fingerprints, coverage and `fits`.

use noel_chord::delta::{apply_owned, diff, Decoder, DeltaError, Encoder, Op};
use noel_chord::{
    canonical_json, coverage, fingerprint, fits, hash64, Description, MemberDescription,
};
use serde::Deserialize;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/chord-delta.json");
const MAKEFILE: &str = include_str!("../../../Makefile");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the fixture is JSON")
}

fn section<'a>(fixture: &'a Value, name: &str, at_least: usize) -> &'a Vec<Value> {
    let cases = fixture[name]
        .as_array()
        .unwrap_or_else(|| panic!("{name} is a list"));
    assert!(cases.len() >= at_least, "{name} has {} cases", cases.len());
    cases
}

fn parse_ops(ops: &Value) -> Vec<Op> {
    ops.as_array()
        .expect("ops are a list")
        .iter()
        .map(|op| Op::from_json(op).unwrap_or_else(|error| panic!("{op}: {error}")))
        .collect()
}

fn ops_json(ops: &[Op]) -> Value {
    Value::Array(ops.iter().map(Op::to_json).collect())
}

fn error_json(error: &DeltaError) -> Value {
    json!({"name": error.upstream_name(), "message": error.to_string()})
}

/// The descriptions parsed straight from the text, so variants and members
/// keep their order.
#[derive(Deserialize)]
struct Contracts {
    contracts: Vec<ContractCase>,
}

#[derive(Deserialize)]
struct ContractCase {
    label: String,
    description: Description,
}

fn descriptions() -> Vec<(String, Description)> {
    let parsed: Contracts = serde_json::from_str(FIXTURE).expect("contracts parse");
    parsed
        .contracts
        .into_iter()
        .map(|case| (case.label, case.description))
        .collect()
}

#[test]
fn the_fixture_comes_from_the_pinned_commit() {
    let fixture = fixture();
    let pin = MAKEFILE
        .lines()
        .find_map(|line| line.strip_prefix("CHORD_REFERENCE_REF ?= "))
        .expect("the Makefile pins Chord");
    assert_eq!(
        fixture["source"]["ref"], pin,
        "regenerate chord-delta.json after moving the pin"
    );
    let package_commit = fixture["source"]["packageCommit"].as_str().unwrap();
    assert_eq!(package_commit.len(), 40);
}

#[test]
fn codec_streams_match_upstream() {
    let fixture = fixture();
    let mut batches = 0;
    for stream in section(&fixture, "codec", 8) {
        let name = stream["name"].as_str().unwrap();
        let mut encoder = Encoder::new();
        let mut decoder = Decoder::new();
        let mut value = stream["base"].clone();
        for (index, batch) in stream["batches"].as_array().unwrap().iter().enumerate() {
            let ops = parse_ops(&batch["ops"]);
            let wire = encoder.encode(&ops);
            assert_eq!(
                Value::Array(wire),
                batch["wire"],
                "{name}, batch {index}: wire"
            );
            // Decode upstream's wire, not ours.
            let decoded = decoder
                .decode(batch["wire"].as_array().unwrap())
                .unwrap_or_else(|error| panic!("{name}, batch {index}: {error}"));
            assert_eq!(
                ops_json(&decoded),
                batch["decoded"],
                "{name}, batch {index}: decoded"
            );
            value = apply_owned(value, &decoded)
                .unwrap_or_else(|error| panic!("{name}, batch {index}: {error}"));
            assert_eq!(value, batch["value"], "{name}, batch {index}: value");
            batches += 1;
        }
    }
    assert!(batches >= 20, "{batches} batches");
}

#[test]
fn decode_errors_match_upstream() {
    let fixture = fixture();
    for case in section(&fixture, "decodeErrors", 30) {
        let name = case["name"].as_str().unwrap();
        let mut decoder = Decoder::new();
        let batches = case["batches"].as_array().unwrap();
        for (index, batch) in batches.iter().enumerate() {
            let last = index == batches.len() - 1;
            match decoder.decode(batch["wire"].as_array().unwrap()) {
                Ok(ops) => {
                    assert!(
                        !last,
                        "{name}: decoded {:?}, upstream threw {}",
                        ops, batch["error"]
                    );
                    assert_eq!(ops_json(&ops), batch["decoded"], "{name}, batch {index}");
                }
                Err(error) => {
                    assert!(last, "{name}, batch {index}: {error}");
                    assert_eq!(error_json(&error), batch["error"], "{name}");
                }
            }
        }
    }
}

#[test]
fn apply_immutable_matches_upstream() {
    let fixture = fixture();
    for case in section(&fixture, "apply", 40) {
        let name = case["name"].as_str().unwrap();
        // Upstream validates each op as it reaches it.
        let mut outcome = Ok(case["base"].clone());
        for op in case["ops"].as_array().unwrap() {
            outcome = outcome.and_then(|value| apply_owned(value, &[Op::from_json(op)?]));
        }
        match outcome {
            Ok(value) => assert_eq!(value, case["result"], "{name}: {}", case["error"]),
            Err(error) => assert_eq!(error_json(&error), case["error"], "{name}"),
        }
    }
}

#[test]
fn diffs_produce_upstream_results() {
    let fixture = fixture();
    for case in section(&fixture, "diff", 25) {
        let name = case["name"].as_str().unwrap();
        let (before, after) = (&case["before"], &case["after"]);
        let ops = diff(before, after);
        let value =
            apply_owned(before.clone(), &ops).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(value, case["result"], "{name}: {ops:?}");
        assert_eq!(canonical_json(&value), canonical_json(after), "{name}");

        // And across the wire, as text.
        let text = serde_json::to_string(&Encoder::new().encode(&ops)).unwrap();
        let wire: Vec<Value> = serde_json::from_str(&text).unwrap();
        let decoded = Decoder::new().decode(&wire).unwrap();
        assert_eq!(
            apply_owned(before.clone(), &decoded).unwrap(),
            case["result"],
            "{name}"
        );

        assert!(
            diff(after, after).is_empty(),
            "{name}: no ops between equal values"
        );
    }
}

#[test]
fn canonical_json_and_hash64_match_upstream() {
    let fixture = fixture();
    for case in section(&fixture, "canonicalJson", 20) {
        let value: Value = serde_json::from_str(case["json"].as_str().unwrap()).unwrap();
        let canonical = canonical_json(&value);
        assert_eq!(canonical, case["canonical"], "{}", case["name"]);
        assert_eq!(hash64(&canonical), case["hash64"], "{}", case["name"]);
    }
    for case in section(&fixture, "numbers", 30) {
        let literal = case["literal"].as_str().unwrap();
        // serde_json's default float parser is not correctly rounded; the
        // standard library's is, as JSON.parse is.
        let number: f64 = literal.parse().unwrap();
        let canonical = canonical_json(&Value::from(number));
        assert_eq!(canonical, case["canonical"], "{literal}");
        assert_eq!(hash64(&canonical), case["hash64"], "{literal}");
    }
    for case in section(&fixture, "hash64", 15) {
        assert_eq!(
            hash64(case["text"].as_str().unwrap()),
            case["hash64"],
            "{}",
            case["text"]
        );
    }
}

#[test]
fn contract_fingerprints_match_upstream() {
    let fixture = fixture();
    let cases = section(&fixture, "contracts", 25);
    let typed = descriptions();
    assert_eq!(typed.len(), cases.len());
    for (case, (label, description)) in cases.iter().zip(&typed) {
        assert_eq!(case["label"], label.as_str());
        assert_eq!(
            fingerprint(&case["description"]),
            case["fingerprint"],
            "{label}"
        );
        assert_eq!(description.fingerprint(), case["fingerprint"], "{label}");
        assert_eq!(
            description.to_value(),
            case["description"],
            "{label}: lossless"
        );
    }
}

#[test]
fn coverage_matches_upstream() {
    let fixture = fixture();
    let typed = descriptions();
    let find = |label: &Value| {
        &typed
            .iter()
            .find(|(name, _)| label == name.as_str())
            .unwrap_or_else(|| panic!("no contract {label}"))
            .1
    };
    for case in section(&fixture, "coverage", 30) {
        let reason = coverage(find(&case["provider"]), find(&case["consumer"]));
        assert_eq!(
            reason.map_or(Value::Null, Value::String),
            case["reason"],
            "{} covering {}",
            case["provider"],
            case["consumer"]
        );
    }
}

#[test]
fn fits_matches_upstream() {
    let fixture = fixture();
    for case in section(&fixture, "fits", 50) {
        assert_eq!(
            Value::Bool(fits(&case["sub"], &case["sup"])),
            case["fits"],
            "{}",
            case["name"]
        );
    }
}

#[test]
fn builders_reproduce_upstream_fingerprints() {
    let fixture = fixture();
    let expected = |label: &str| {
        section(&fixture, "contracts", 1)
            .iter()
            .find(|case| case["label"] == label)
            .map(|case| case["fingerprint"].clone())
            .unwrap()
    };
    let input = json!({"type": "object", "properties": {}});
    let number = json!({"type": "number"});
    let counter = Description::remote(
        "fixture.counter",
        1,
        input.clone(),
        [(
            "owner",
            vec![
                ("count", MemberDescription::state(number.clone())),
                (
                    "add",
                    MemberDescription::method(
                        Some(json!({"type": "object", "required": ["n"], "properties": {"n": number}})),
                        Some(number.clone()),
                    ),
                ),
                ("reset", MemberDescription::method(None, None)),
            ],
        )],
    )
    .unwrap();
    assert_eq!(counter.fingerprint(), expected("counter"));

    let native = Description::local(
        "fixture.native",
        1,
        input,
        [
            ("handle", MemberDescription::opaque("fixture.handle@1")),
            ("size", MemberDescription::state(number)),
            ("ping", MemberDescription::method(None, None)),
        ],
    )
    .unwrap();
    assert_eq!(native.fingerprint(), expected("native"));
}
