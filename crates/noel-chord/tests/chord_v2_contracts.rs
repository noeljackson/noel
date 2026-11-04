//! Chord v2 contracts against upstream at `CHORD2_REFERENCE_REF`
//! (`tests/fixtures/ts-parity/chord-v2-contracts.json`, written by
//! `scripts/ts-parity/chord-v2-contracts-fixture.ts`): `hash64`, and the
//! fingerprint of each contract as upstream's `remote()` declares it from the
//! same JSON, including noel's own (`noel-client/contracts/v2`) and
//! upstream's pinned `b3eda86070a7e319`.

use std::path::Path;

use noel_chord::v2::{hash64, Contract, Declaration};
use serde_json::Value;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/ts-parity/chord-v2-contracts.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read fixture"))
        .expect("parse fixture")
}

#[test]
fn hash64_matches_upstream() {
    for case in fixture()["hash64"].as_array().unwrap() {
        let text = case[0].as_str().unwrap();
        assert_eq!(hash64(text), case[1].as_str().unwrap(), "{text:?}");
    }
}

#[test]
fn fingerprints_match_upstream() {
    let fixture = fixture();
    let contracts = fixture["contracts"].as_array().unwrap();
    assert!(contracts.len() >= 5);
    for case in contracts {
        let declaration: Declaration =
            serde_json::from_value(case["declaration"].clone()).expect("a declaration");
        let name = declaration.name.clone();
        let contract = Contract::declare(declaration).expect("a valid contract");
        assert_eq!(
            contract.fingerprint,
            case["fingerprint"].as_str().unwrap(),
            "{name}: normal form {}",
            contract.normal
        );
    }
}

#[test]
fn noels_contracts_are_the_ones_the_fixture_declared() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../noel-client/contracts/v2");
    let fixture = fixture();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        let contract = Contract::from_json(&text).expect("noel's contract declares");
        let recorded = fixture["contracts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["declaration"]["name"] == contract.declaration.name.as_str())
            .unwrap_or_else(|| panic!("{} is in the fixture", path.display()));
        assert_eq!(
            contract.fingerprint,
            recorded["fingerprint"].as_str().unwrap()
        );
    }
}
