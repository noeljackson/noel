//! A session's history as a list that builds only the rows in view.
//!
//! A column of every entry lays out and shapes the whole history on each
//! frame, which stalls the window on a long session. The list measures a row
//! when it is first drawn and keeps its height.

use crate::entry_element;
use gpui::{colors::Colors, div, list, prelude::*, px, AnyElement, List, ListAlignment, ListState};
use serde_json::Value;
use std::sync::Arc;

/// How far past the viewport rows are built, so a scroll has them ready.
const OVERDRAW: f32 = 600.;

/// The history's entries, oldest first, and one row after them for what is
/// still streaming.
pub struct Transcript {
    entries: Arc<Vec<Value>>,
    list: ListState,
}

impl Default for Transcript {
    fn default() -> Self {
        Transcript {
            entries: Arc::default(),
            // The tail row is always there: rows are the entries, then it.
            list: ListState::new(1, ListAlignment::Bottom, px(OVERDRAW)),
        }
    }
}

impl Transcript {
    pub fn entries(&self) -> &[Value] {
        &self.entries
    }

    /// Another session, or none: no rows but the tail, at the end.
    pub fn clear(&mut self) {
        self.entries = Arc::default();
        self.list.reset(1);
    }

    /// Takes the history as it now stands. Entries added before or after
    /// the ones held keep the measured rows and the scroll position; any
    /// other change starts the list again. The tail row is measured again
    /// either way.
    pub fn set(&mut self, entries: Arc<Vec<Value>>) {
        if !Arc::ptr_eq(&self.entries, &entries) {
            let held = self.entries.len();
            match placed(&self.entries, &entries) {
                Some((before, after)) => {
                    self.list.splice(held..held, after);
                    self.list.splice(0..0, before);
                }
                None => self.list.reset(entries.len() + 1),
            }
            self.entries = entries;
        }
        let tail = self.entries.len();
        self.list.remeasure_items(tail..tail + 1);
    }

    /// The list, `tail` drawing the row after the last entry.
    pub fn element(&self, colors: &Colors, tail: impl Fn() -> AnyElement + 'static) -> List {
        let entries = self.entries.clone();
        let colors = colors.clone();
        list(self.list.clone(), move |index, _, _| {
            let row = match entries.get(index) {
                Some(entry) => entry_element(entry, &colors),
                None => tail(),
            };
            div()
                .px_6()
                .pb_4()
                .when(index == 0, |row| row.pt_4())
                .child(row)
                .into_any_element()
        })
    }
}

/// How many entries `new` has before and after the ones `held`, if it
/// holds them all in a row (by id: an entry of the history does not change).
fn placed(held: &[Value], new: &[Value]) -> Option<(usize, usize)> {
    let id = |entry: &Value| entry["id"].as_u64();
    let Some(first) = held.first() else {
        return Some((0, new.len()));
    };
    let before = new
        .iter()
        .position(|entry| id(entry).is_some() && id(entry) == id(first))?;
    let kept = new.get(before..before + held.len())?;
    held.iter()
        .zip(kept)
        .all(|(held, kept)| id(held).is_some() && id(held) == id(kept))
        .then(|| (before, new.len() - before - held.len()))
}

#[cfg(test)]
mod tests {
    use super::placed;
    use serde_json::{json, Value};

    fn entries(ids: &[u64]) -> Vec<Value> {
        ids.iter().map(|id| json!({ "id": id })).collect()
    }

    #[test]
    fn placed_counts_the_entries_around_the_ones_held() {
        assert_eq!(placed(&[], &entries(&[1, 2])), Some((0, 2)));
        assert_eq!(placed(&entries(&[1, 2]), &entries(&[1, 2])), Some((0, 0)));
        assert_eq!(
            placed(&entries(&[1, 2]), &entries(&[1, 2, 3])),
            Some((0, 1))
        );
        // The history before the first window arrived, and a new entry.
        assert_eq!(
            placed(&entries(&[3, 4]), &entries(&[1, 2, 3, 4, 5])),
            Some((2, 1))
        );
    }

    #[test]
    fn placed_is_none_when_an_entry_held_is_gone_or_moved() {
        assert_eq!(placed(&entries(&[1, 2]), &entries(&[1])), None);
        assert_eq!(placed(&entries(&[1, 2]), &entries(&[1, 4])), None);
        assert_eq!(placed(&entries(&[1, 2]), &entries(&[3, 4])), None);
        assert_eq!(placed(&[json!({})], &[json!({})]), None);
    }
}
