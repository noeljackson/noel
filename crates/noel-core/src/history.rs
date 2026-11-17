//! A session's history as clients see it (Durable's conversation entries):
//! every message the session ever saved, in order, each with a stable id,
//! and markers where the model's context was cut or reset. Unlike
//! `SessionState::messages`, which is what the model sees, compaction and
//! undo never remove entries: a compaction summary or a reset is a marker in
//! place (Durable's head markers).
//!
//! The history is a function of the journal: each message record is the
//! next entry, and each compaction or reset record a marker. Ids count
//! entries in journal order, so the live view (fed by the store's observer)
//! and a reload compute the same ids. A compaction checkpoint rewrites the
//! journal but keeps every message record, so ids survive it.

use serde::Serialize;
use serde_json::{json, Value};

use crate::{CompactionKind, CompactionRecord, ConversationMessage, MessageRole};

/// What a journal record adds to the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRecord {
    Message {
        message: ConversationMessage,
        /// Milliseconds since the epoch, for records that saved one.
        timestamp: Option<u64>,
        /// Placed from the queue (follow-ups or steering, joined): how
        /// they were queued, `steer` or `followUp`.
        queued: Option<&'static str>,
    },
    /// The context was compacted: earlier messages are summarized.
    Compaction(CompactionRecord),
    /// The context was replaced by `messages` messages (an undo or a
    /// checkpoint that is not a compaction).
    Reset { messages: usize },
    /// A tool call's outcome, saved just before its result message: no
    /// entry, but whether that result is an error.
    ToolEvent { call_id: String, is_error: bool },
}

/// One entry of the history, in Durable's `EntryRecord` shape:
/// `{id, kind, model?, data?, head?, timestamp?}`. A message is `pi.user`,
/// `pi.assistant`, `pi.tool-result` or `pi.system` with the message as
/// `model`; a marker is `pi.compaction` (`data: {reason, summary,
/// omittedMessages, retainedMessages}`) or `pi.reset` (`data: {messages}`)
/// with `head`, where the model's context now starts.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// 1 for the session's first entry, then one more for each.
    pub id: u64,
    pub timestamp: Option<u64>,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    /// `kind` is `pi.user`, ...; `message` as clients see it.
    Message {
        kind: &'static str,
        message: Value,
        /// A user message placed from the queue (`data: {queued: mode}`).
        queued: Option<&'static str>,
    },
    Compaction {
        reason: &'static str,
        summary: String,
        omitted_messages: usize,
        retained_messages: usize,
    },
    Reset {
        messages: usize,
    },
}

impl Entry {
    pub fn to_json(&self) -> Value {
        let mut entry = match &self.kind {
            EntryKind::Message {
                kind,
                message,
                queued,
            } => {
                let mut entry = json!({"id": self.id, "kind": kind, "model": [message]});
                if let Some(mode) = queued {
                    entry["data"] = json!({"queued": mode});
                }
                entry
            }
            EntryKind::Compaction {
                reason,
                summary,
                omitted_messages,
                retained_messages,
            } => json!({
                "id": self.id,
                "kind": "pi.compaction",
                "data": {
                    "reason": reason,
                    "summary": summary,
                    "omittedMessages": omitted_messages,
                    "retainedMessages": retained_messages,
                },
                "head": self.id,
            }),
            EntryKind::Reset { messages } => json!({
                "id": self.id,
                "kind": "pi.reset",
                "data": {"messages": messages},
                "head": self.id,
            }),
        };
        if let Some(timestamp) = self.timestamp {
            entry["timestamp"] = json!(timestamp);
        }
        entry
    }
}

impl Serialize for Entry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// The history built so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    entries: Vec<Entry>,
    /// Tool calls whose saved outcome was an error.
    failed_calls: std::collections::HashSet<String>,
}

impl History {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The id the next entry gets.
    pub fn next_id(&self) -> u64 {
        self.entries.last().map_or(1, |entry| entry.id + 1)
    }

    /// Adds what `record` adds; `to_json` renders a message as clients see
    /// it, given whether it is a failed tool call's result. Returns the new
    /// entry, if the record made one.
    pub fn push(
        &mut self,
        record: &HistoryRecord,
        to_json: impl Fn(&ConversationMessage, bool) -> Value,
    ) -> Option<&Entry> {
        if let HistoryRecord::ToolEvent { call_id, is_error } = record {
            if *is_error {
                self.failed_calls.insert(call_id.clone());
            } else {
                self.failed_calls.remove(call_id);
            }
            return None;
        }
        let id = self.next_id();
        let (timestamp, kind) = match record {
            HistoryRecord::Message {
                message,
                timestamp,
                queued,
            } => (
                *timestamp,
                EntryKind::Message {
                    kind: match message.role {
                        MessageRole::User => "pi.user",
                        MessageRole::Assistant => "pi.assistant",
                        MessageRole::Tool => "pi.tool-result",
                        MessageRole::System => "pi.system",
                    },
                    message: to_json(
                        message,
                        message
                            .tool_call_id
                            .as_ref()
                            .is_some_and(|id| self.failed_calls.contains(id)),
                    ),
                    queued: *queued,
                },
            ),
            HistoryRecord::Compaction(record) => (
                None,
                EntryKind::Compaction {
                    reason: match record.kind {
                        CompactionKind::Manual => "manual",
                        CompactionKind::Automatic => "threshold",
                        CompactionKind::Retry => "overflow",
                    },
                    summary: record.summary.clone(),
                    omitted_messages: record.omitted_messages,
                    retained_messages: record.retained_messages,
                },
            ),
            HistoryRecord::Reset { messages } => (
                None,
                EntryKind::Reset {
                    messages: *messages,
                },
            ),
            HistoryRecord::ToolEvent { .. } => unreachable!("handled above"),
        };
        self.entries.push(Entry {
            id,
            timestamp,
            kind,
        });
        self.entries.last()
    }

    /// The newest entries: at most `min` of them, or every entry from
    /// `from` on when that is at most `max` (Durable's `ConversationWindow`).
    /// Returns the window and the id of the entry just before it.
    pub fn window(&self, from: Option<u64>, min: usize, max: usize) -> (&[Entry], Option<u64>) {
        let len = self.entries.len();
        let start = match from {
            Some(from) => {
                let at = self.entries.partition_point(|entry| entry.id < from);
                if len - at <= max {
                    at
                } else {
                    len.saturating_sub(min)
                }
            }
            None => len.saturating_sub(min),
        };
        let before = start.checked_sub(1).map(|index| self.entries[index].id);
        (&self.entries[start..], before)
    }

    /// Up to `limit` entries just before the entry `before`, oldest first,
    /// and the id of the entry before them (paging back).
    pub fn page(&self, before: u64, limit: usize) -> (&[Entry], Option<u64>) {
        let end = self.entries.partition_point(|entry| entry.id < before);
        let start = end.saturating_sub(limit);
        let earlier = start.checked_sub(1).map(|index| self.entries[index].id);
        (&self.entries[start..end], earlier)
    }
}

/// Milliseconds since the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> HistoryRecord {
        HistoryRecord::Message {
            message: ConversationMessage {
                thinking: String::new(),
                role: MessageRole::User,
                content: text.to_owned(),
                media: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                tool_calls: Vec::new(),
                aborted: false,
            },
            timestamp: Some(7),
            queued: None,
        }
    }

    fn text(message: &ConversationMessage, _failed: bool) -> Value {
        Value::String(message.content.clone())
    }

    fn history(count: usize) -> History {
        let mut history = History::default();
        for index in 0..count {
            history.push(&message(&index.to_string()), text);
        }
        history
    }

    #[test]
    fn markers_keep_their_place_and_ids_count_entries() {
        let mut history = History::default();
        history.push(&message("a"), text);
        history.push(
            &HistoryRecord::Compaction(CompactionRecord {
                kind: CompactionKind::Automatic,
                omitted_messages: 1,
                retained_messages: 0,
                summary: "s".to_owned(),
            }),
            text,
        );
        history.push(&message("b"), text);
        let json = serde_json::to_value(history.entries()).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"id": 1, "timestamp": 7, "kind": "pi.user", "model": ["a"]},
                {"id": 2, "kind": "pi.compaction", "head": 2, "data": {
                    "reason": "threshold", "summary": "s", "omittedMessages": 1, "retainedMessages": 0}},
                {"id": 3, "timestamp": 7, "kind": "pi.user", "model": ["b"]},
            ])
        );
    }

    #[test]
    fn the_window_is_the_newest_entries_or_everything_from_an_entry() {
        let history = history(120);
        let (window, before) = history.window(None, 50, 100);
        assert_eq!((window.len(), window[0].id, before), (50, 71, Some(70)));
        // From an entry with at most `max` after it: from there.
        let (window, before) = history.window(Some(30), 50, 100);
        assert_eq!((window.len(), window[0].id, before), (91, 30, Some(29)));
        // Too far back: the newest `min`.
        let (window, _) = history.window(Some(5), 50, 100);
        assert_eq!(window[0].id, 71);
        // Past the newest: empty.
        let (window, before) = history.window(Some(500), 50, 100);
        assert_eq!((window.len(), before), (0, Some(120)));
        let small = super::tests::history(3);
        let (window, before) = small.window(None, 50, 100);
        assert_eq!((window.len(), before), (3, None));
    }

    #[test]
    fn pages_go_back_from_an_entry() {
        let history = history(120);
        let (page, earlier) = history.page(71, 50);
        assert_eq!(
            (page.len(), page[0].id, page[49].id, earlier),
            (50, 21, 70, Some(20))
        );
        let (page, earlier) = history.page(21, 50);
        assert_eq!((page.len(), page[0].id, earlier), (20, 1, None));
        let (page, _) = history.page(1, 50);
        assert!(page.is_empty());
    }
}
