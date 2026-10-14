//! Chord delta: operations over plain JSON, applied immutably, as in
//! upstream's `chord/src/delta`.
//!
//! | Op | Tuple | Meaning |
//! | --- | --- | --- |
//! | [`Op::Replace`] | `["r", value]` | Replace the whole value. |
//! | [`Op::Set`] | `["s", path, value]` | Set an object property or array element. |
//! | [`Op::Delete`] | `["d", path]` | Delete an object property or remove an array element. |
//! | [`Op::Append`] | `["a", path, text]` | Append to a string. |
//! | [`Op::Trim`] | `["t", path, count]` | Remove `count` UTF-16 code units from a string's front. |
//! | [`Op::Splice`] | `["p", path, index, remove, items]` | Splice an array. |
//! | [`Op::Permute`] | `["m", path, permutation]` | Reorder an array: `new[i] = old[permutation[i]]`. |
//!
//! Batches are exact but not canonical: only the value they produce is
//! contractual. [`Encoder`] and [`Decoder`] add the wire compression; use one
//! pair per ordered state stream, and after an error discard both the decoder
//! and the replica and recover from a later `r`.

mod codec;
mod diff;

pub use codec::{Decoder, Encoder};
pub use diff::diff;

use serde_json::Value;

use crate::js;
use crate::schema::canonical_json;

/// Segments that reach JavaScript's prototype chain. Upstream rejects them in
/// every path, so noel does too.
pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

/// One step of a path: an object key or a non-negative array index.
///
/// A numeric segment on an object addresses the key JavaScript would make of
/// the number, so `["o", 0]` and `["o", "0"]` reach the same property.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Seg {
    Key(String),
    Index(u64),
}

impl Seg {
    pub fn to_json(&self) -> Value {
        match self {
            Seg::Key(key) => Value::String(key.clone()),
            Seg::Index(index) => Value::from(*index),
        }
    }
}

impl From<&str> for Seg {
    fn from(key: &str) -> Self {
        Seg::Key(key.to_owned())
    }
}

impl From<String> for Seg {
    fn from(key: String) -> Self {
        Seg::Key(key)
    }
}

impl From<u64> for Seg {
    fn from(index: u64) -> Self {
        Seg::Index(index)
    }
}

impl From<usize> for Seg {
    fn from(index: usize) -> Self {
        Seg::Index(index as u64)
    }
}

pub type Path = Vec<Seg>;

pub fn path_to_json(path: &[Seg]) -> Value {
    Value::Array(path.iter().map(Seg::to_json).collect())
}

/// A decoded operation. `Set`, `Delete`, `Append` and `Trim` need a nonempty
/// path; `Splice` and `Permute` may target the root.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Replace(Value),
    Set(Path, Value),
    Delete(Path),
    Append(Path, String),
    /// Counts UTF-16 code units, as upstream does.
    Trim(Path, u64),
    /// `(path, index, remove, items)`.
    Splice(Path, u64, u64, Vec<Value>),
    Permute(Path, Vec<usize>),
}

impl Op {
    pub fn verb(&self) -> &'static str {
        match self {
            Op::Replace(_) => "r",
            Op::Set(..) => "s",
            Op::Delete(_) => "d",
            Op::Append(..) => "a",
            Op::Trim(..) => "t",
            Op::Splice(..) => "p",
            Op::Permute(..) => "m",
        }
    }

    /// The op's path; `None` for `Replace`.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Op::Replace(_) => None,
            Op::Set(path, _)
            | Op::Delete(path)
            | Op::Append(path, _)
            | Op::Trim(path, _)
            | Op::Splice(path, ..)
            | Op::Permute(path, _) => Some(path),
        }
    }

    /// The tuple form, with its path inline.
    pub fn to_json(&self) -> Value {
        let mut tuple = vec![Value::from(self.verb())];
        if let Some(path) = self.path() {
            tuple.push(path_to_json(path));
        }
        tuple.extend(self.arguments());
        Value::Array(tuple)
    }

    /// What follows the path in the tuple.
    pub(crate) fn arguments(&self) -> Vec<Value> {
        match self {
            Op::Replace(value) | Op::Set(_, value) => vec![value.clone()],
            Op::Delete(_) => Vec::new(),
            Op::Append(_, text) => vec![Value::String(text.clone())],
            Op::Trim(_, count) => vec![Value::from(*count)],
            Op::Splice(_, index, remove, items) => vec![
                Value::from(*index),
                Value::from(*remove),
                Value::Array(items.clone()),
            ],
            Op::Permute(_, permutation) => vec![Value::from(permutation.clone())],
        }
    }

    /// Parses and validates a decoded tuple, as upstream's `assertValidOp`:
    /// paths inline, no `#`, no short forms. Errors carry upstream's messages.
    pub fn from_json(op: &Value) -> Result<Op, DeltaError> {
        let tuple = match op {
            Value::Array(tuple) if !tuple.is_empty() => tuple,
            _ => return Err(shape("op is not a tuple")),
        };
        let verb = match &tuple[0] {
            Value::String(verb) => verb.as_str(),
            other => return Err(unknown_verb(other)),
        };
        match verb {
            "r" => {
                arity(tuple, 2, "r arity")?;
                Ok(Op::Replace(tuple[1].clone()))
            }
            "s" => {
                arity(tuple, 3, "s arity")?;
                Ok(Op::Set(parse_path(&tuple[1], true)?, tuple[2].clone()))
            }
            "d" => {
                arity(tuple, 2, "d arity")?;
                Ok(Op::Delete(parse_path(&tuple[1], true)?))
            }
            "a" => {
                let text = match tuple.get(2) {
                    Some(Value::String(text)) if tuple.len() == 3 => text.clone(),
                    _ => return Err(shape("a shape")),
                };
                Ok(Op::Append(parse_path(&tuple[1], true)?, text))
            }
            "t" => {
                let count = match tuple.get(2).and_then(js::non_negative_integer) {
                    Some(count) if tuple.len() == 3 => count,
                    _ => return Err(shape("t shape")),
                };
                Ok(Op::Trim(parse_path(&tuple[1], true)?, count))
            }
            "p" => {
                arity(tuple, 5, "p arity")?;
                let path = parse_path(&tuple[1], false)?;
                let index = js::non_negative_integer(&tuple[2]).ok_or_else(|| shape("p index"))?;
                let remove =
                    js::non_negative_integer(&tuple[3]).ok_or_else(|| shape("p remove"))?;
                let Value::Array(items) = &tuple[4] else {
                    return Err(shape("p items"));
                };
                Ok(Op::Splice(path, index, remove, items.clone()))
            }
            "m" => {
                arity(tuple, 3, "m arity")?;
                let path = parse_path(&tuple[1], false)?;
                Ok(Op::Permute(path, parse_permutation(&tuple[2])?))
            }
            _ => Err(unknown_verb(&tuple[0])),
        }
    }
}

/// A batch that begins with a replacement: a recovery point for a reader.
pub fn is_base(ops: &[Op]) -> bool {
    matches!(ops.first(), Some(Op::Replace(_)))
}

/// Why an op could not be decoded or applied. [`DeltaError::upstream_name`]
/// names the error upstream throws in the same place, and the message is
/// upstream's.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeltaError {
    /// A malformed op: upstream's `TypeError`.
    #[error("{0}")]
    Shape(String),
    /// A path that does not resolve: upstream's `PathError`. Holds the path,
    /// or the path id, as JSON.
    #[error("unresolvable path: {0}")]
    Path(String),
    /// A reserved segment, a segment that is not a non-negative integer, or an
    /// array index past the end: upstream's `UnsafePathError`.
    #[error("unsafe path segment: {0}")]
    UnsafePath(String),
    /// noel only: a `t` that would leave half of a surrogate pair, which a
    /// Rust string cannot hold. Upstream keeps the lone surrogate.
    #[error("trim splits a surrogate pair at {0}")]
    SplitSurrogate(String),
}

impl DeltaError {
    /// The class of the error upstream throws here.
    pub fn upstream_name(&self) -> &'static str {
        match self {
            DeltaError::Shape(_) => "TypeError",
            DeltaError::Path(_) => "PathError",
            DeltaError::UnsafePath(_) => "UnsafePathError",
            DeltaError::SplitSurrogate(_) => "SplitSurrogate",
        }
    }
}

pub(crate) fn shape(message: impl Into<String>) -> DeltaError {
    DeltaError::Shape(message.into())
}

pub(crate) fn path_error(path: &[Seg]) -> DeltaError {
    DeltaError::Path(canonical_json(&path_to_json(path)))
}

fn unknown_verb(verb: &Value) -> DeltaError {
    shape(format!("unknown op verb: {}", js::display(verb)))
}

fn arity(tuple: &[Value], length: usize, message: &str) -> Result<(), DeltaError> {
    if tuple.len() == length {
        Ok(())
    } else {
        Err(shape(message))
    }
}

/// The property key JavaScript makes of a numeric segment.
pub(crate) fn index_key(index: u64) -> String {
    js::number_to_string(index as f64)
}

/// `assertSafePath` for one segment.
pub(crate) fn parse_seg(seg: &Value) -> Result<Seg, DeltaError> {
    match seg {
        Value::String(key) if RESERVED_SEGMENTS.contains(&key.as_str()) => {
            Err(DeltaError::UnsafePath(key.clone()))
        }
        Value::String(key) => Ok(Seg::Key(key.clone())),
        Value::Number(number) => {
            let value = js::number_value(number);
            if !(value.is_finite() && value.fract() == 0.0 && value >= 0.0) {
                return Err(DeltaError::UnsafePath(js::display(seg)));
            }
            // noel holds indices as u64; JavaScript would accept larger
            // integers as object keys.
            if value >= 18_446_744_073_709_551_616.0 {
                return Err(DeltaError::UnsafePath(js::number_to_string(value)));
            }
            Ok(Seg::Index(value as u64))
        }
        other => Err(DeltaError::UnsafePath(js::display(other))),
    }
}

/// `assertPathArg`: an array of safe segments, nonempty when required.
pub(crate) fn parse_path(path: &Value, nonempty: bool) -> Result<Path, DeltaError> {
    let Value::Array(segs) = path else {
        return Err(shape("path is not an array"));
    };
    if nonempty && segs.is_empty() {
        return Err(shape("path is empty"));
    }
    segs.iter().map(parse_seg).collect()
}

pub(crate) fn parse_permutation(value: &Value) -> Result<Vec<usize>, DeltaError> {
    let Value::Array(items) = value else {
        return Err(shape("m permutation is not an array"));
    };
    let mut permutation = Vec::with_capacity(items.len());
    for item in items {
        match js::non_negative_integer(item) {
            Some(index) if index < items.len() as u64 => permutation.push(index as usize),
            _ => return Err(shape("m permutation is not a bijection")),
        }
    }
    check_bijection(&permutation)?;
    Ok(permutation)
}

fn check_bijection(permutation: &[usize]) -> Result<(), DeltaError> {
    let mut seen = vec![false; permutation.len()];
    for &index in permutation {
        if index >= seen.len() || seen[index] {
            return Err(shape("m permutation is not a bijection"));
        }
        seen[index] = true;
    }
    Ok(())
}

/// `assertValidOp` for an op built in Rust: the type already fixes its shape.
fn validate(op: &Op) -> Result<(), DeltaError> {
    let check_safe = |path: &Path| {
        for seg in path {
            if let Seg::Key(key) = seg {
                if RESERVED_SEGMENTS.contains(&key.as_str()) {
                    return Err(DeltaError::UnsafePath(key.clone()));
                }
            }
        }
        Ok(())
    };
    match op {
        Op::Replace(_) => Ok(()),
        Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Trim(path, _) => {
            if path.is_empty() {
                return Err(shape("path is empty"));
            }
            check_safe(path)
        }
        Op::Splice(path, ..) => check_safe(path),
        Op::Permute(path, permutation) => {
            check_safe(path)?;
            check_bijection(permutation)
        }
    }
}

/// Applies one batch and returns the new value; `base` is not changed, as
/// upstream's `applyImmutable`.
pub fn apply_immutable(base: &Value, ops: &[Op]) -> Result<Value, DeltaError> {
    apply_owned(base.clone(), ops)
}

/// Applies batches in order as one replay of the final result, as upstream's
/// `applyImmutableBatches`.
pub fn apply_immutable_batches<I>(base: &Value, batches: I) -> Result<Value, DeltaError>
where
    I: IntoIterator,
    I::Item: AsRef<[Op]>,
{
    let mut value = base.clone();
    for batch in batches {
        value = apply_owned(value, batch.as_ref())?;
    }
    Ok(value)
}

/// Applies one batch to a value the caller gives up, without copying it. On
/// an error the value is dropped, so no half-applied state survives; the
/// checks and errors are `applyImmutable`'s.
pub fn apply_owned(mut value: Value, ops: &[Op]) -> Result<Value, DeltaError> {
    for op in ops {
        apply_op(&mut value, op)?;
    }
    Ok(value)
}

fn is_container(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

/// Walks `path` to a container, failing as upstream's `copyContainers` does:
/// `PathError` for a missing step or a primitive, `UnsafePathError` for a key
/// on an array that the array has as its own property (`"length"`, `"0"`).
fn containers<'v>(root: &'v mut Value, path: &[Seg]) -> Result<&'v mut Value, DeltaError> {
    if !is_container(root) {
        return Err(path_error(path));
    }
    let mut node = root;
    for seg in path {
        let own = match (&*node, seg) {
            (Value::Object(map), Seg::Key(key)) => map.contains_key(key),
            (Value::Object(map), Seg::Index(index)) => map.contains_key(&index_key(*index)),
            (Value::Array(items), Seg::Index(index)) => *index < items.len() as u64,
            (Value::Array(items), Seg::Key(key)) => {
                key == "length" || js::array_index(key).is_some_and(|i| (i as usize) < items.len())
            }
            _ => false,
        };
        if !own {
            return Err(path_error(path));
        }
        let child = match (node, seg) {
            (Value::Object(map), Seg::Key(key)) => map.get_mut(key),
            (Value::Object(map), Seg::Index(index)) => map.get_mut(&index_key(*index)),
            (Value::Array(items), Seg::Index(index)) => items.get_mut(*index as usize),
            (Value::Array(_), Seg::Key(key)) => return Err(DeltaError::UnsafePath(key.clone())),
            _ => None,
        };
        let child = child.expect("checked above");
        if !is_container(child) {
            return Err(path_error(path));
        }
        node = child;
    }
    Ok(node)
}

fn clamp(value: u64, limit: usize) -> usize {
    usize::try_from(value).map_or(limit, |value| value.min(limit))
}

fn apply_op(root: &mut Value, op: &Op) -> Result<(), DeltaError> {
    validate(op)?;
    match op {
        Op::Replace(value) => {
            *root = value.clone();
            Ok(())
        }
        Op::Splice(path, index, remove, items) => {
            let Value::Array(array) = containers(root, path)? else {
                return Err(path_error(path));
            };
            // Array.prototype.splice clamps both the start and the count.
            let start = clamp(*index, array.len());
            let end = start + clamp(*remove, array.len() - start);
            array.splice(start..end, items.iter().cloned());
            Ok(())
        }
        Op::Permute(path, permutation) => match containers(root, path)? {
            Value::Array(array) if array.len() == permutation.len() => {
                let mut previous: Vec<Option<Value>> =
                    std::mem::take(array).into_iter().map(Some).collect();
                *array = permutation
                    .iter()
                    .map(|&index| previous[index].take().expect("a bijection"))
                    .collect();
                Ok(())
            }
            _ => Err(path_error(path)),
        },
        Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Trim(path, _) => {
            let (last, parent_path) = path.split_last().expect("validated nonempty");
            match containers(root, parent_path)? {
                Value::Array(array) => {
                    let index = match last {
                        Seg::Index(index) => *index,
                        Seg::Key(key) => return Err(DeltaError::UnsafePath(key.clone())),
                    };
                    // An index may address an element or append one past the end.
                    if index > array.len() as u64 {
                        return Err(DeltaError::UnsafePath(index_key(index)));
                    }
                    let index = index as usize;
                    match op {
                        Op::Set(_, value) if index == array.len() => array.push(value.clone()),
                        Op::Set(_, value) => array[index] = value.clone(),
                        Op::Delete(_) if index < array.len() => {
                            array.remove(index);
                        }
                        Op::Delete(_) => return Err(path_error(path)),
                        Op::Append(_, text) => match array.get_mut(index) {
                            Some(Value::String(current)) => current.push_str(text),
                            _ => return Err(path_error(path)),
                        },
                        Op::Trim(_, count) => match array.get_mut(index) {
                            Some(Value::String(current)) => trim_front(current, *count, path)?,
                            _ => return Err(path_error(path)),
                        },
                        _ => unreachable!("matched above"),
                    }
                }
                Value::Object(map) => {
                    let key = match last {
                        Seg::Key(key) => key.clone(),
                        Seg::Index(index) => index_key(*index),
                    };
                    match op {
                        Op::Set(_, value) => {
                            map.insert(key, value.clone());
                        }
                        Op::Delete(_) => {
                            map.remove(&key);
                        }
                        Op::Append(_, text) => match map.get_mut(&key) {
                            Some(Value::String(current)) => current.push_str(text),
                            _ => return Err(path_error(path)),
                        },
                        Op::Trim(_, count) => match map.get_mut(&key) {
                            Some(Value::String(current)) => trim_front(current, *count, path)?,
                            _ => return Err(path_error(path)),
                        },
                        _ => unreachable!("matched above"),
                    }
                }
                _ => unreachable!("containers() returns a container"),
            }
            Ok(())
        }
    }
}

/// `text.slice(count)`: drops `count` UTF-16 code units from the front.
fn trim_front(text: &mut String, count: u64, path: &[Seg]) -> Result<(), DeltaError> {
    let mut units = 0u64;
    for (byte, c) in text.char_indices() {
        if units == count {
            text.drain(..byte);
            return Ok(());
        }
        units += c.len_utf16() as u64;
        if units > count {
            return Err(DeltaError::SplitSurrogate(canonical_json(&path_to_json(
                path,
            ))));
        }
    }
    text.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn op(value: Value) -> Op {
        Op::from_json(&value).unwrap()
    }

    #[test]
    fn trim_counts_utf16_units() {
        let base = json!({"t": "a😀b"});
        let value = apply_immutable(&base, &[op(json!(["t", ["t"], 3]))]).unwrap();
        assert_eq!(value, json!({"t": "b"}));
        let value = apply_immutable(&base, &[op(json!(["t", ["t"], 9]))]).unwrap();
        assert_eq!(value, json!({"t": ""}));
        assert_eq!(base, json!({"t": "a😀b"}));
    }

    #[test]
    fn a_trim_inside_a_surrogate_pair_is_refused() {
        let base = json!({"t": "a😀b"});
        let error = apply_immutable(&base, &[op(json!(["t", ["t"], 2]))]).unwrap_err();
        assert_eq!(error.upstream_name(), "SplitSurrogate");
    }

    #[test]
    fn an_error_leaves_the_base_alone() {
        let base = json!({"a": [1]});
        let ops = [op(json!(["s", ["a", 0], 2])), op(json!(["d", ["a", 5]]))];
        assert!(apply_immutable(&base, &ops).is_err());
        assert_eq!(base, json!({"a": [1]}));
    }

    #[test]
    fn batches_replay_in_order() {
        let batches = vec![
            vec![op(json!(["s", ["a"], 1]))],
            vec![op(json!(["s", ["b"], 2]))],
        ];
        let value = apply_immutable_batches(&json!({}), &batches).unwrap();
        assert_eq!(value, json!({"a": 1, "b": 2}));
    }
}
