//! The wire form of op batches, as upstream's `encoder()` and `decoder()`.
//!
//! It adds two compressions to [`Op`] tuples and nothing else:
//!
//! - `["#", id, path]` defines an id on a path's second use, and later ops
//!   name the path by that number;
//! - a short tuple without a path reuses the previous op's path in the same
//!   batch; the tuple's length tells the forms apart.
//!
//! Ids span batches; short forms do not. An `r` clears the dictionary on both
//! sides, so every base batch is a recovery point.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::{parse_path, parse_permutation, path_error, path_to_json, shape, DeltaError, Op, Path};
use crate::js;

/// Interns repeated paths. One encoder per ordered state stream.
#[derive(Debug, Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, u64>,
    next_id: u64,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The wire tuples for one batch.
    pub fn encode(&mut self, ops: &[Op]) -> Vec<Value> {
        let mut out = Vec::with_capacity(ops.len());
        // The previous op's path, within this batch only.
        let mut previous: Option<&Path> = None;
        for op in ops {
            let Some(path) = op.path() else {
                out.push(op.to_json());
                // Everything after a replacement must decode without what came
                // before it.
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            };
            let mut tuple = vec![Value::from(op.verb())];
            if previous != Some(path) {
                let reference = if let Some(id) = self.ids.get(path) {
                    Value::from(*id)
                } else if self.seen.contains(path) {
                    // Second use: define, then reference.
                    let id = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(path.clone(), id);
                    out.push(Value::Array(vec![
                        Value::from("#"),
                        Value::from(id),
                        path_to_json(path),
                    ]));
                    Value::from(id)
                } else {
                    // First use: inline.
                    self.seen.insert(path.clone());
                    path_to_json(path)
                };
                tuple.push(reference);
                previous = Some(path);
            }
            tuple.extend(op.arguments());
            out.push(Value::Array(tuple));
        }
        out
    }
}

/// Restores [`Op`]s from wire tuples, validating each as upstream's
/// `assertValidWireOp` does. One decoder per ordered state stream, fed exactly
/// its encoder's batches from that state's base on.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Keyed by the id's f64 bits, as a JavaScript `Map` keys numbers.
    paths: HashMap<u64, Path>,
}

/// A path reference: an id defined by `#`, or a path inline.
enum Reference {
    Id(u64, f64),
    Path(Path),
}

/// `Number.isInteger(value) && value >= 0`, as a map key.
fn id_key(value: &Value) -> Option<(u64, f64)> {
    let Value::Number(number) = value else {
        return None;
    };
    let value = js::number_value(number);
    if !(value.is_finite() && value.fract() == 0.0 && value >= 0.0) {
        return None;
    }
    // -0 and 0 are one key.
    let value = if value == 0.0 { 0.0 } else { value };
    Some((value.to_bits(), value))
}

fn parse_reference(reference: &Value) -> Result<Reference, DeltaError> {
    if let Value::Number(_) = reference {
        let (key, id) = id_key(reference).ok_or_else(|| shape("bad path id"))?;
        return Ok(Reference::Id(key, id));
    }
    // A string is not a path: unchecked, it would resolve to the root.
    Ok(Reference::Path(parse_path(reference, false)?))
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decodes one batch. After an error, discard this decoder.
    pub fn decode(&mut self, wire: &[Value]) -> Result<Vec<Op>, DeltaError> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::with_capacity(wire.len());
        for op in wire {
            let tuple = match op {
                Value::Array(tuple) if !tuple.is_empty() => tuple,
                _ => return Err(shape("op is not a tuple")),
            };
            let verb = match &tuple[0] {
                Value::String(verb) => verb.as_str(),
                other => return Err(shape(format!("unknown op verb: {}", js::display(other)))),
            };
            match verb {
                "#" => {
                    let definition =
                        match (tuple.len(), tuple.get(1).and_then(id_key), tuple.get(2)) {
                            (3, Some((key, _)), Some(path @ Value::Array(_))) => (key, path),
                            _ => return Err(shape("# shape")),
                        };
                    let path = parse_path(definition.1, false)?;
                    self.paths.insert(definition.0, path);
                    continue;
                }
                "r" => {
                    if tuple.len() != 2 {
                        return Err(shape("r arity"));
                    }
                    out.push(Op::Replace(tuple[1].clone()));
                    self.paths.clear();
                    previous = None;
                    continue;
                }
                _ => {}
            }

            // The full form carries a path reference; the short form, one
            // element fewer, reuses the previous path.
            let full = match (verb, tuple.len()) {
                ("s", 3) | ("d", 2) | ("a", 3) | ("t", 3) | ("p", 5) | ("m", 3) => true,
                ("s", 2) | ("d", 1) | ("a", 2) | ("t", 2) | ("p", 4) | ("m", 2) => false,
                ("s" | "d" | "a" | "t" | "p" | "m", _) => {
                    return Err(shape(format!("{verb} arity")))
                }
                _ => return Err(shape(format!("unknown op verb: {verb}"))),
            };
            let reference = if full {
                Some(parse_reference(&tuple[1])?)
            } else {
                None
            };
            let args = &tuple[if full { 2 } else { 1 }..];
            match verb {
                "a" if !args[0].is_string() => return Err(shape("a value")),
                "t" if js::non_negative_integer(&args[0]).is_none() => {
                    return Err(shape("t count"));
                }
                "p" => {
                    if js::non_negative_integer(&args[0]).is_none() {
                        return Err(shape("p index"));
                    }
                    if js::non_negative_integer(&args[1]).is_none() {
                        return Err(shape("p remove"));
                    }
                    if !args[2].is_array() {
                        return Err(shape("p items"));
                    }
                }
                "m" => {
                    parse_permutation(&args[0])?;
                }
                _ => {}
            }

            let path = match reference {
                None => previous.clone().ok_or_else(|| path_error(&[]))?,
                Some(Reference::Id(key, id)) => {
                    let path = self
                        .paths
                        .get(&key)
                        .cloned()
                        .ok_or_else(|| DeltaError::Path(js::number_to_string(id)))?;
                    previous = Some(path.clone());
                    path
                }
                Some(Reference::Path(path)) => {
                    previous = Some(path.clone());
                    path
                }
            };
            if verb != "p" && verb != "m" && path.is_empty() {
                return Err(path_error(&path));
            }
            let integer = |value: &Value| js::non_negative_integer(value).expect("validated above");
            out.push(match verb {
                "s" => Op::Set(path, args[0].clone()),
                "d" => Op::Delete(path),
                "a" => Op::Append(path, args[0].as_str().expect("validated above").to_owned()),
                "t" => Op::Trim(path, integer(&args[0])),
                "p" => Op::Splice(
                    path,
                    integer(&args[0]),
                    integer(&args[1]),
                    args[2].as_array().expect("validated above").clone(),
                ),
                "m" => Op::Permute(path, parse_permutation(&args[0])?),
                _ => unreachable!("verbs checked above"),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ops(values: Value) -> Vec<Op> {
        values
            .as_array()
            .unwrap()
            .iter()
            .map(|op| Op::from_json(op).unwrap())
            .collect()
    }

    #[test]
    fn a_path_is_interned_on_its_second_use() {
        let mut encoder = Encoder::new();
        let batch = ops(json!([
            ["s", ["a"], 1],
            ["s", ["a"], 2],
            ["s", ["b"], 3],
            ["s", ["a"], 4]
        ]));
        let wire = encoder.encode(&batch);
        assert_eq!(
            Value::Array(wire.clone()),
            json!([
                ["s", ["a"], 1],
                ["s", 2],
                ["s", ["b"], 3],
                ["#", 0, ["a"]],
                ["s", 0, 4]
            ])
        );
        let mut decoder = Decoder::new();
        assert_eq!(decoder.decode(&wire).unwrap(), batch);
    }
}
