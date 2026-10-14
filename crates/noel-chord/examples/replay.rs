//! Replays the upstream fixture's wire batches through noel-chord's decoder
//! and immutable applier, and prints each value beside upstream's.
//!
//! ```text
//! cargo run -p noel-chord --example replay [-- path/to/chord-delta.json]
//! ```
//!
//! Exits with status 1 if any value differs from upstream's.

use std::process::ExitCode;

use noel_chord::canonical_json;
use noel_chord::delta::{apply_owned, Decoder};
use serde_json::Value;

fn main() -> ExitCode {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/ts-parity/chord-delta.json"
        )
        .to_owned()
    });
    let fixture: Value = match std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
    {
        Ok(fixture) => fixture,
        Err(error) => {
            eprintln!("replay: {path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "upstream {} at {}",
        fixture["source"]["package"].as_str().unwrap_or("?"),
        fixture["source"]["ref"].as_str().unwrap_or("?")
    );

    let (mut batches, mut mismatches) = (0, 0);
    for stream in fixture["codec"].as_array().into_iter().flatten() {
        println!("\n{}", stream["name"].as_str().unwrap_or("?"));
        let mut decoder = Decoder::new();
        let mut value = Some(stream["base"].clone());
        println!("  base      {}", canonical_json(&stream["base"]));
        for (index, batch) in stream["batches"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            batches += 1;
            let wire = batch["wire"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let outcome = decoder
                .decode(wire)
                .and_then(|ops| apply_owned(value.take().unwrap_or_default(), &ops));
            let upstream = canonical_json(&batch["value"]);
            match outcome {
                Ok(next) => {
                    let ours = canonical_json(&next);
                    let verdict = if ours == upstream { "same" } else { "DIFFERS" };
                    if ours != upstream {
                        mismatches += 1;
                        println!("  upstream  {upstream}");
                    }
                    println!("  batch {index:<3} {ours}  [{verdict}]");
                    value = Some(next);
                }
                Err(error) => {
                    mismatches += 1;
                    println!("  batch {index:<3} error: {error}  [DIFFERS: upstream {upstream}]");
                    break;
                }
            }
        }
    }
    println!("\n{batches} batches, {mismatches} differing from upstream");
    if mismatches == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
