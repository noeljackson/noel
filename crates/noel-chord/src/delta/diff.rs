//! A compact op batch between two JSON revisions, ported from upstream's
//! `diffRevisions` (`chord/src/delta/diff.ts`).
//!
//! Upstream matches array elements by JavaScript object identity where it
//! can. Two separately parsed values share no containers, so here only
//! primitives are ever identical; that is how upstream behaves on values that
//! came off a wire. Only the resulting value is contractual.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{index_key, Op, Path, Seg, RESERVED_SEGMENTS};
use crate::js;

const DEFAULT_OVERLAP_SCAN: usize = 65_536;
const MAX_DELTA_OPERATIONS: usize = 4_096;
const MAX_IDENTITY_CANDIDATES: usize = 200_000;
const MAX_SEMANTIC_CELLS: usize = 65_536;

/// Ops that turn `before` into `after`. A batch that would exceed 4,096 ops,
/// or cost more than the snapshot once it passes 64 KiB, is `[["r", after]]`.
pub fn diff(before: &Value, after: &Value) -> Vec<Op> {
    let mut batch = Batch::default();
    diff_value(before, after, &[], &mut batch);
    if batch.overflowed {
        return vec![Op::Replace(after.clone())];
    }
    let ops = batch.ops;
    if ops.is_empty() || matches!(ops[0], Op::Replace(_)) {
        return ops;
    }
    let delta_cost = 2 + ops.iter().map(|op| operation_cost(op) + 1).sum::<usize>();
    if delta_cost < 65_536 {
        return ops;
    }
    if delta_cost >= json_cost(after) + 6 {
        vec![Op::Replace(after.clone())]
    } else {
        ops
    }
}

#[derive(Default)]
struct Batch {
    ops: Vec<Op>,
    overflowed: bool,
}

impl Batch {
    fn emit(&mut self, op: Op) {
        if self.overflowed {
            return;
        }
        if self.ops.len() >= MAX_DELTA_OPERATIONS {
            self.overflowed = true;
            return;
        }
        self.ops.push(op);
    }

    fn set(&mut self, path: &[Seg], value: &Value) {
        if path.is_empty() {
            self.emit(Op::Replace(value.clone()));
        } else {
            self.emit(Op::Set(path.to_vec(), value.clone()));
        }
    }
}

fn child(path: &[Seg], seg: Seg) -> Path {
    let mut path = path.to_vec();
    path.push(seg);
    path
}

/// JavaScript's `===` between values that share no containers.
fn identical(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => js::number_value(a) == js::number_value(b),
        (Value::String(a), Value::String(b)) => a == b,
        _ => false,
    }
}

/// Upstream's `equalJson`, and so its `sameValue`.
fn equal_json(a: &Value, b: &Value) -> bool {
    if identical(a, b) {
        return true;
    }
    match (a, b) {
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal_json(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| equal_json(a, b)))
        }
        _ => false,
    }
}

/// A primitive as a JavaScript `Map` keys it (SameValueZero). Containers have
/// no key: no container is identical to another value here.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Key<'a> {
    Null,
    Bool(bool),
    Number(u64),
    String(&'a str),
}

fn key_of(value: &Value) -> Option<Key<'_>> {
    match value {
        Value::Null => Some(Key::Null),
        Value::Bool(b) => Some(Key::Bool(*b)),
        Value::Number(n) => {
            let n = js::number_value(n);
            Some(Key::Number(if n == 0.0 { 0.0f64 } else { n }.to_bits()))
        }
        Value::String(s) => Some(Key::String(s)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn permutation(before: &[Value], after: &[Value]) -> Option<Vec<usize>> {
    if before.len() != after.len() {
        return None;
    }
    let mut positions: HashMap<Key<'_>, (Vec<usize>, usize)> = HashMap::new();
    for (index, value) in before.iter().enumerate() {
        if let Some(key) = key_of(value) {
            positions.entry(key).or_default().0.push(index);
        }
    }
    let mut result = Vec::with_capacity(after.len());
    for value in after {
        let (indices, used) = key_of(value).and_then(|key| positions.get_mut(&key))?;
        let index = *indices.get(*used)?;
        *used += 1;
        result.push(index);
    }
    Some(result)
}

/// Longest suffix of `a` (within the last `scan` units) that is a prefix of
/// `b`, probing as upstream's `overlap` does; 0 when it gives up.
fn overlap(a: &[u16], b: &[u16], scan: usize, probe: usize, max_candidates: usize) -> usize {
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail = if a.len() > scan {
        &a[a.len() - scan..]
    } else {
        a
    };
    for h in [probe.min(b.len()), 1] {
        let head = &b[..h];
        let mut tried = 0;
        let mut from = 0;
        while let Some(k) = index_of(tail, head, from) {
            tried += 1;
            if tried > max_candidates {
                break;
            }
            let n = tail.len() - k;
            if n <= b.len() && tail[k..] == b[..n] {
                return n;
            }
            from = k + 1;
        }
        if h == 1 {
            break;
        }
    }
    0
}

/// `String.prototype.indexOf` over UTF-16 code units.
fn index_of(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()] == *needle)
}

fn emit_string(before: &str, after: &str, path: &[Seg], batch: &mut Batch) {
    if before == after {
        return;
    }
    if after.len() > before.len() && after.starts_with(before) {
        batch.emit(Op::Append(path.to_vec(), after[before.len()..].to_owned()));
        return;
    }
    let before16: Vec<u16> = before.encode_utf16().collect();
    let after16: Vec<u16> = after.encode_utf16().collect();
    let shared = overlap(&before16, &after16, DEFAULT_OVERLAP_SCAN, 64, 8);
    // For well-formed strings the overlap starts and ends on character
    // boundaries, so the rest always decodes; a set is never wrong.
    let rest = (shared > 0)
        .then(|| String::from_utf16(&after16[shared..]).ok())
        .flatten();
    let Some(rest) = rest else {
        batch.emit(Op::Set(path.to_vec(), Value::String(after.to_owned())));
        return;
    };
    batch.emit(Op::Trim(path.to_vec(), (before16.len() - shared) as u64));
    if after16.len() > shared {
        batch.emit(Op::Append(path.to_vec(), rest));
    }
}

type ArrayMatch = (usize, usize);

fn lcs_matches(before: &[Value], after: &[Value], max_cells: usize) -> Option<Vec<ArrayMatch>> {
    if before.is_empty() || after.is_empty() {
        return Some(Vec::new());
    }
    if before.len().saturating_mul(after.len()) > max_cells {
        return None;
    }
    // Upstream aligns containers that share a child container by identity;
    // without shared containers that reduces to deep equality.
    let equal = equal_json;
    let width = after.len() + 1;
    let mut lengths = vec![0u32; (before.len() + 1) * width];
    for left in (0..before.len()).rev() {
        for right in (0..after.len()).rev() {
            let at = left * width + right;
            lengths[at] = if equal(&before[left], &after[right]) {
                lengths[(left + 1) * width + right + 1] + 1
            } else {
                lengths[(left + 1) * width + right].max(lengths[left * width + right + 1])
            };
        }
    }
    let mut matches = Vec::new();
    let (mut left, mut right) = (0, 0);
    while left < before.len() && right < after.len() {
        if equal(&before[left], &after[right])
            && lengths[left * width + right] == lengths[(left + 1) * width + right + 1] + 1
        {
            matches.push((left, right));
            left += 1;
            right += 1;
        } else if lengths[(left + 1) * width + right] >= lengths[left * width + right + 1] {
            left += 1;
        } else {
            right += 1;
        }
    }
    Some(matches)
}

fn lower_bound(values: &[usize], value: usize) -> usize {
    values.partition_point(|&v| v < value)
}

fn identity_subsequence(
    before: &[Value],
    (before_start, before_end): (usize, usize),
    after: &[Value],
    (after_start, after_end): (usize, usize),
) -> Option<Vec<ArrayMatch>> {
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    if after_count < before_count {
        let matches = in_order(
            &after[after_start..after_end],
            after_start,
            before,
            (before_start, before_end),
        )?;
        return Some(matches.into_iter().map(|(a, b)| (b, a)).collect());
    }
    if before_count < after_count {
        return in_order(
            &before[before_start..before_end],
            before_start,
            after,
            (after_start, after_end),
        );
    }
    None
}

/// Each item of `short`, in order, paired with the next identical item of
/// `long[long_start..long_end]`; `None` when one has none. Pairs are
/// `(index in short's array, index in long)`.
fn in_order(
    short: &[Value],
    short_start: usize,
    long: &[Value],
    (long_start, long_end): (usize, usize),
) -> Option<Vec<ArrayMatch>> {
    let mut at = long_start;
    short
        .iter()
        .enumerate()
        .map(|(offset, item)| {
            while at < long_end && !identical(item, &long[at]) {
                at += 1;
            }
            if at == long_end {
                return None;
            }
            at += 1;
            Some((short_start + offset, at - 1))
        })
        .collect()
}

fn greedy_identity_anchors(
    positions: &HashMap<Key<'_>, Vec<usize>>,
    after: &[Value],
    (after_start, after_end): (usize, usize),
) -> Vec<ArrayMatch> {
    let mut matches = Vec::new();
    let mut next = 0;
    for (after_index, value) in after.iter().enumerate().take(after_end).skip(after_start) {
        let Some(candidates) = key_of(value).and_then(|key| positions.get(&key)) else {
            continue;
        };
        let Some(&before_index) = candidates.get(lower_bound(candidates, next)) else {
            continue;
        };
        matches.push((before_index, after_index));
        next = before_index + 1;
    }
    matches
}

/// The longest increasing run of identical anchors (patience style).
fn identity_anchors(
    before: &[Value],
    (before_start, before_end): (usize, usize),
    after: &[Value],
    (after_start, after_end): (usize, usize),
) -> Vec<ArrayMatch> {
    let mut positions: HashMap<Key<'_>, Vec<usize>> = HashMap::new();
    for (index, value) in before
        .iter()
        .enumerate()
        .take(before_end)
        .skip(before_start)
    {
        if let Some(key) = key_of(value) {
            positions.entry(key).or_default().push(index);
        }
    }
    let mut candidate_count = 0;
    for value in &after[after_start..after_end] {
        candidate_count += key_of(value)
            .and_then(|key| positions.get(&key))
            .map_or(0, Vec::len);
        if candidate_count > MAX_IDENTITY_CANDIDATES {
            return greedy_identity_anchors(&positions, after, (after_start, after_end));
        }
    }
    if candidate_count == 0 {
        return Vec::new();
    }

    // (before, after, previous candidate)
    let mut candidates: Vec<(usize, usize, Option<usize>)> = Vec::new();
    let mut tails: Vec<usize> = Vec::new();
    let mut tail_values: Vec<usize> = Vec::new();
    for (after_index, value) in after.iter().enumerate().take(after_end).skip(after_start) {
        let Some(before_positions) = key_of(value).and_then(|key| positions.get(&key)) else {
            continue;
        };
        for &before_index in before_positions.iter().rev() {
            let at = lower_bound(&tail_values, before_index);
            let candidate_index = candidates.len();
            candidates.push((
                before_index,
                after_index,
                at.checked_sub(1).map(|at| tails[at]),
            ));
            if at == tails.len() {
                tails.push(candidate_index);
                tail_values.push(before_index);
            } else {
                tails[at] = candidate_index;
                tail_values[at] = before_index;
            }
        }
    }
    let mut matches = Vec::new();
    let mut candidate_index = tails.last().copied();
    while let Some(index) = candidate_index {
        let (before_index, after_index, previous) = candidates[index];
        matches.push((before_index, after_index));
        candidate_index = previous;
    }
    matches.reverse();
    matches
}

/// One region of an array diff: `before[before_start..before_end]` becomes
/// `after[after_start..after_end]`, at `output_start` in the array as edited
/// so far.
struct Region {
    before: (usize, usize),
    after: (usize, usize),
    output: usize,
}

fn process_array_matches(
    before: &[Value],
    after: &[Value],
    path: &[Seg],
    batch: &mut Batch,
    region: Region,
    matches: &[ArrayMatch],
) {
    let mut before_at = region.before.0;
    let mut after_at = region.after.0;
    let mut output_at = region.output;
    for &(before_match, after_match) in matches {
        if batch.overflowed {
            return;
        }
        diff_array_region(
            before,
            after,
            path,
            batch,
            Region {
                before: (before_at, before_match),
                after: (after_at, after_match),
                output: output_at,
            },
        );
        output_at += after_match - after_at;
        if !equal_json(&before[before_match], &after[after_match]) {
            diff_value(
                &before[before_match],
                &after[after_match],
                &child(path, Seg::Index(output_at as u64)),
                batch,
            );
        }
        output_at += 1;
        before_at = before_match + 1;
        after_at = after_match + 1;
    }
    if !batch.overflowed {
        diff_array_region(
            before,
            after,
            path,
            batch,
            Region {
                before: (before_at, region.before.1),
                after: (after_at, region.after.1),
                output: output_at,
            },
        );
    }
}

fn diff_array_region(
    before: &[Value],
    after: &[Value],
    path: &[Seg],
    batch: &mut Batch,
    region: Region,
) {
    if batch.overflowed {
        return;
    }
    let Region {
        before: (mut before_start, mut before_end),
        after: (mut after_start, mut after_end),
        output: mut output_start,
    } = region;
    while before_start < before_end
        && after_start < after_end
        && equal_json(&before[before_start], &after[after_start])
    {
        before_start += 1;
        after_start += 1;
        output_start += 1;
    }
    while before_start < before_end
        && after_start < after_end
        && equal_json(&before[before_end - 1], &after[after_end - 1])
    {
        before_end -= 1;
        after_end -= 1;
    }
    let before_count = before_end - before_start;
    let after_count = after_end - after_start;
    if before_count == 0 && after_count == 0 {
        return;
    }
    let splice = |batch: &mut Batch| {
        batch.emit(Op::Splice(
            path.to_vec(),
            output_start as u64,
            before_count as u64,
            after[after_start..after_end].to_vec(),
        ));
    };
    if before_count == 0 || after_count == 0 {
        splice(batch);
        return;
    }
    let region = || Region {
        before: (before_start, before_end),
        after: (after_start, after_end),
        output: output_start,
    };

    if before_count == after_count {
        let positional: Vec<ArrayMatch> = (0..before_count)
            .filter(|offset| {
                equal_json(&before[before_start + offset], &after[after_start + offset])
            })
            .map(|offset| (before_start + offset, after_start + offset))
            .collect();
        if !positional.is_empty() {
            process_array_matches(before, after, path, batch, region(), &positional);
            return;
        }
    }

    if let Some(subsequence) = identity_subsequence(
        before,
        (before_start, before_end),
        after,
        (after_start, after_end),
    ) {
        if !subsequence.is_empty() {
            process_array_matches(before, after, path, batch, region(), &subsequence);
            return;
        }
    }

    let identity = identity_anchors(
        before,
        (before_start, before_end),
        after,
        (after_start, after_end),
    );
    if !identity.is_empty() {
        process_array_matches(before, after, path, batch, region(), &identity);
        return;
    }

    if let Some(semantic) = lcs_matches(
        &before[before_start..before_end],
        &after[after_start..after_end],
        MAX_SEMANTIC_CELLS,
    ) {
        if !semantic.is_empty() {
            let absolute: Vec<ArrayMatch> = semantic
                .into_iter()
                .map(|(b, a)| (before_start + b, after_start + a))
                .collect();
            process_array_matches(before, after, path, batch, region(), &absolute);
            return;
        }
    }

    if before_count == 1 && after_count == 1 {
        diff_value(
            &before[before_start],
            &after[after_start],
            &child(path, Seg::Index(output_start as u64)),
            batch,
        );
        return;
    }
    splice(batch);
}

fn diff_array(before: &[Value], after: &[Value], path: &[Seg], batch: &mut Batch) {
    if before.len() == after.len() && before.iter().zip(after).all(|(a, b)| equal_json(a, b)) {
        return;
    }
    if before.len() == after.len()
        && before.len() > 1
        && !equal_json(&before[0], &after[0])
        && !equal_json(&before[before.len() - 1], &after[after.len() - 1])
    {
        if let Some(order) = permutation(before, after) {
            batch.emit(Op::Permute(path.to_vec(), order));
            return;
        }
    }
    diff_array_region(
        before,
        after,
        path,
        batch,
        Region {
            before: (0, before.len()),
            after: (0, after.len()),
            output: 0,
        },
    );
}

fn diff_object(
    before: &Map<String, Value>,
    after: &Map<String, Value>,
    path: &[Seg],
    batch: &mut Batch,
) {
    // A change at or below a reserved key folds into a set of this object.
    if before
        .keys()
        .chain(after.keys())
        .any(|key| RESERVED_SEGMENTS.contains(&key.as_str()))
    {
        let (before, after) = (Value::Object(before.clone()), Value::Object(after.clone()));
        if !equal_json(&before, &after) {
            batch.set(path, &after);
        }
        return;
    }
    for (key, value) in after {
        if batch.overflowed {
            return;
        }
        let path = child(path, Seg::Key(key.clone()));
        match before.get(key) {
            Some(previous) => diff_value(previous, value, &path, batch),
            None => batch.set(&path, value),
        }
    }
    for key in before.keys() {
        if batch.overflowed {
            return;
        }
        if !after.contains_key(key) {
            batch.emit(Op::Delete(child(path, Seg::Key(key.clone()))));
        }
    }
}

fn diff_value(before: &Value, after: &Value, path: &[Seg], batch: &mut Batch) {
    if identical(before, after) || batch.overflowed {
        return;
    }
    match (before, after) {
        (Value::String(before), Value::String(after)) if !path.is_empty() => {
            emit_string(before, after, path, batch);
        }
        (Value::Array(before), Value::Array(after)) => diff_array(before, after, path, batch),
        (Value::Object(before), Value::Object(after)) => diff_object(before, after, path, batch),
        _ => batch.set(path, after),
    }
}

fn number_cost(value: f64) -> usize {
    js::number_to_string(value).len()
}

/// The length of a value's JSON, roughly, as upstream estimates it.
fn json_cost(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::String(s) => js::utf16_len(s) + 2,
        Value::Number(n) => number_cost(js::number_value(n)),
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Array(items) => items_cost(items.iter().map(json_cost)),
        Value::Object(map) => {
            2 + map
                .iter()
                .enumerate()
                .map(|(index, (key, value))| {
                    js::utf16_len(key) + 3 + json_cost(value) + usize::from(index > 0)
                })
                .sum::<usize>()
        }
    }
}

fn items_cost(costs: impl Iterator<Item = usize>) -> usize {
    2 + costs
        .enumerate()
        .map(|(index, cost)| cost + usize::from(index > 0))
        .sum::<usize>()
}

fn path_cost(path: &[Seg]) -> usize {
    items_cost(path.iter().map(|seg| match seg {
        Seg::Key(key) => js::utf16_len(key) + 2,
        Seg::Index(index) => index_key(*index).len(),
    }))
}

fn operation_cost(op: &Op) -> usize {
    match op {
        Op::Replace(value) => 6 + json_cost(value),
        Op::Set(path, value) => 7 + path_cost(path) + json_cost(value),
        Op::Delete(path) => 6 + path_cost(path),
        Op::Append(path, text) => 7 + path_cost(path) + js::utf16_len(text) + 2,
        Op::Trim(path, count) => 7 + path_cost(path) + number_cost(*count as f64),
        Op::Splice(path, index, remove, items) => {
            10 + path_cost(path)
                + number_cost(*index as f64)
                + number_cost(*remove as f64)
                + items_cost(items.iter().map(json_cost))
        }
        Op::Permute(path, permutation) => {
            7 + path_cost(path)
                + items_cost(permutation.iter().map(|&index| number_cost(index as f64)))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::apply_immutable;
    use super::*;
    use crate::schema::canonical_json;

    /// Diffs, and checks the ops produce `after`; numbers compare as
    /// JavaScript compares them.
    fn check(before: Value, after: Value) -> Vec<Op> {
        let ops = diff(&before, &after);
        let result = apply_immutable(&before, &ops).unwrap();
        assert_eq!(canonical_json(&result), canonical_json(&after), "{ops:?}");
        ops
    }

    #[test]
    fn strings_append_trim_or_set() {
        let ops = check(json!({"t": "hello"}), json!({"t": "hello world"}));
        assert!(matches!(ops.as_slice(), [Op::Append(..)]));
        let ops = check(json!({"t": "abcdef"}), json!({"t": "defghi"}));
        assert!(matches!(ops.as_slice(), [Op::Trim(_, 3), Op::Append(..)]));
        let ops = check(json!({"t": "abc"}), json!({"t": "xyz"}));
        assert!(matches!(ops.as_slice(), [Op::Set(..)]));
    }

    #[test]
    fn primitive_arrays_permute() {
        let ops = check(json!([1, 2, 3, 4]), json!([4, 3, 2, 1]));
        assert!(matches!(ops.as_slice(), [Op::Permute(..)]));
    }

    #[test]
    fn too_many_ops_become_a_replacement() {
        let before: Map<String, Value> = (0..5000).map(|i| (format!("k{i}"), json!(0))).collect();
        let after: Map<String, Value> = (0..5000).map(|i| (format!("k{i}"), json!(1))).collect();
        let ops = check(Value::Object(before), Value::Object(after));
        assert!(matches!(ops.as_slice(), [Op::Replace(_)]));
    }

    #[test]
    fn an_op_dearer_than_the_snapshot_becomes_a_replacement() {
        let ops = check(
            json!({"a": "x".repeat(70_000)}),
            json!({"a": "y".repeat(70_000)}),
        );
        assert!(matches!(ops.as_slice(), [Op::Replace(_)]));
    }

    #[test]
    fn equal_values_need_no_ops() {
        assert!(check(json!({"a": [1, {"b": 2.0}]}), json!({"a": [1, {"b": 2}]})).is_empty());
    }

    #[test]
    fn reserved_keys_fold_into_their_object() {
        let ops = check(
            json!({"o": {"__proto__": 1, "a": 1}}),
            json!({"o": {"__proto__": 1, "a": 2}}),
        );
        assert!(matches!(ops.as_slice(), [Op::Set(path, _)] if path == &vec![Seg::from("o")]));
    }
}
