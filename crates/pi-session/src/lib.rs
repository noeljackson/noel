//! Versioned, append-only JSONL journals shared by interactive agents.
//!
//! Every append writes its record and newline in one `write_all`, so a crash
//! leaves at most a torn final segment. Readers ignore such a segment, and
//! writers repair it ([`repair_jsonl_tail`]) before appending, so no record is
//! ever glued onto a partial line.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pi_ai::{ChatMessage, ModelRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub version: u32,
    pub sequence: u64,
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub event: SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum SessionEvent {
    Started {
        session_id: String,
        #[serde(default)]
        subject_id: Option<String>,
    },
    Metadata {
        values: BTreeMap<String, String>,
    },
    TurnStarted {
        turn_id: String,
    },
    TurnCompleted {
        turn_id: String,
    },
    TurnFailed {
        turn_id: String,
        error: String,
    },
    TurnCancelled {
        turn_id: String,
    },
    Message {
        turn_id: Option<String>,
        message: ChatMessage,
    },
    ToolStarted {
        turn_id: String,
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolFinished {
        turn_id: String,
        call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    ActiveModelChanged {
        previous: Option<ModelRef>,
        active: ModelRef,
        reason: String,
    },
    Compaction {
        reason: String,
        removed_messages: usize,
        summary: String,
    },
    Usage {
        turn_id: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    PromptSnapshot {
        turn_id: String,
        sha256: String,
        #[serde(default)]
        prompt: Option<String>,
    },
    RemediationDrafted {
        proposal_id: String,
        draft: Value,
    },
    /// Unversioned Pi JSONL remains readable during migration. The original
    /// value is retained exactly so applications can perform their own replay.
    Legacy {
        record: Value,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadReport {
    pub events: Vec<EventEnvelope>,
    pub ignored_trailing_bytes: usize,
    pub missing_final_newline: bool,
    pub interrupted_turns: BTreeSet<String>,
}

#[derive(Debug)]
pub struct JsonlJournal {
    path: PathBuf,
    next_sequence: u64,
}

impl JsonlJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<(Self, LoadReport), JournalError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| JournalError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let report = if path.exists() {
            load(&path)?
        } else {
            File::create(&path).map_err(|source| JournalError::Open {
                path: path.clone(),
                source,
            })?;
            LoadReport::default()
        };
        let next_sequence = report
            .events
            .iter()
            .map(|event| event.sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok((
            Self {
                path,
                next_sequence,
            },
            report,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, event: SessionEvent) -> Result<EventEnvelope, JournalError> {
        let envelope = EventEnvelope {
            version: JOURNAL_VERSION,
            sequence: self.next_sequence,
            timestamp_ms: unix_timestamp_ms(),
            event,
        };
        append_jsonl_record(&self.path, &envelope)?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(envelope)
    }
}

/// Durably appends an application-specific legacy record. This lets existing
/// Pi sessions adopt the shared write guarantees without changing their wire
/// format in place.
///
/// The record and its newline reach the file in one `write_all` followed by
/// `sync_data`. A failed write is truncated back to the previous length (best
/// effort), and a torn tail left by an earlier failure is repaired first, so
/// the new record always starts on its own line. Callers serialize appends to
/// one journal.
pub fn append_jsonl_record<T: Serialize>(path: &Path, record: &T) -> Result<(), JournalError> {
    let mut line = serde_json::to_vec(record).map_err(|source| JournalError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    line.push(b'\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| JournalError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|source| JournalError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let write_error = |source| JournalError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut length = file.metadata().map_err(write_error)?.len();
    if repair_tail(&mut file, length).map_err(write_error)? != TailRepair::Clean {
        length = file.metadata().map_err(write_error)?.len();
    }
    if let Err(source) = file.write_all(&line).and_then(|()| file.sync_data()) {
        // A partial record must not survive: the next append would glue onto it.
        let _ = file.set_len(length).and_then(|()| file.sync_data());
        return Err(write_error(source));
    }
    Ok(())
}

/// What [`repair_jsonl_tail`] changed so a journal ends on a record boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailRepair {
    /// Empty, or already newline-terminated; nothing changed.
    Clean,
    /// A complete final record only lacked its newline, which was appended.
    Terminated,
    /// A torn final segment of this many bytes was truncated away.
    Truncated(u64),
}

/// Prepares a journal for appending: a complete final record that lacks its
/// newline is terminated, and a torn final segment (an interrupted write) is
/// truncated back to the last newline. A missing journal needs no repair.
/// Only a journal's writer may call this; readers ignore torn tails instead.
pub fn repair_jsonl_tail(path: &Path) -> Result<TailRepair, JournalError> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(TailRepair::Clean),
        Err(source) => {
            return Err(JournalError::Open {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    file.metadata()
        .and_then(|metadata| repair_tail(&mut file, metadata.len()))
        .map_err(|source| JournalError::Write {
            path: path.to_path_buf(),
            source,
        })
}

/// In-memory form of [`repair_jsonl_tail`] for callers that rewrite a whole
/// journal: the torn final segment is dropped, never patched with a newline.
pub fn repair_jsonl_tail_bytes(content: &mut Vec<u8>) -> TailRepair {
    let (records, tail) = split_jsonl_tail(content);
    if tail.is_empty() {
        TailRepair::Clean
    } else if is_complete_jsonl_record(tail) {
        content.push(b'\n');
        TailRepair::Terminated
    } else {
        let torn = tail.len() as u64;
        content.truncate(records.len());
        TailRepair::Truncated(torn)
    }
}

/// Splits a journal into its newline-terminated part and the bytes after its
/// last newline, which are empty when the journal ends with a newline.
pub fn split_jsonl_tail(content: &[u8]) -> (&[u8], &[u8]) {
    let start = content
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    content.split_at(start)
}

/// Whether an unterminated final segment is a whole record that only lost its
/// newline. Records are JSON objects and every proper prefix of a JSON object
/// is invalid JSON, so this never mistakes a torn write for a record and never
/// discards a complete one.
pub fn is_complete_jsonl_record(segment: &[u8]) -> bool {
    serde_json::from_slice::<serde::de::IgnoredAny>(segment).is_ok()
}

fn repair_tail(file: &mut File, length: u64) -> io::Result<TailRepair> {
    if length == 0 {
        return Ok(TailRepair::Clean);
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::Start(length - 1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(TailRepair::Clean);
    }
    let start = final_segment_start(file, length)?;
    let mut segment = vec![0u8; usize::try_from(length - start).map_err(io::Error::other)?];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut segment)?;
    let repair = if is_complete_jsonl_record(&segment) {
        file.seek(SeekFrom::Start(length))?;
        file.write_all(b"\n")?;
        TailRepair::Terminated
    } else {
        file.set_len(start)?;
        TailRepair::Truncated(length - start)
    };
    file.sync_data()?;
    Ok(repair)
}

/// Offset just past the last newline, scanning backwards so a large journal is
/// not read in full.
fn final_segment_start(file: &mut File, length: u64) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut end = length;
    let mut buffer = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        buffer.resize((end - start) as usize, 0);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer)?;
        if let Some(index) = buffer.iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + index as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

pub fn load(path: &Path) -> Result<LoadReport, JournalError> {
    let file = File::open(path).map_err(|source| JournalError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let file_len = file
        .metadata()
        .map_err(|source| JournalError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .len() as usize;
    let mut reader = BufReader::new(file);
    let mut report = LoadReport::default();
    let mut consumed = 0usize;
    let mut line_number = 0usize;
    loop {
        let mut bytes = Vec::new();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|source| JournalError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let has_newline = bytes.last() == Some(&b'\n');
        let line = if has_newline {
            &bytes[..bytes.len() - 1]
        } else {
            bytes.as_slice()
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            consumed += read;
            continue;
        }
        match decode_line(line, report.events.len() as u64 + 1) {
            Ok(event) => {
                report.events.push(event);
                consumed += read;
                if !has_newline {
                    report.missing_final_newline = true;
                }
            }
            Err(_source) if !has_newline => {
                report.ignored_trailing_bytes = file_len.saturating_sub(consumed);
                break;
            }
            Err(source) => {
                return Err(JournalError::Parse {
                    path: path.to_path_buf(),
                    line: line_number,
                    source,
                })
            }
        }
    }
    report.interrupted_turns = interrupted_turns(&report.events);
    Ok(report)
}

fn decode_line(bytes: &[u8], legacy_sequence: u64) -> Result<EventEnvelope, serde_json::Error> {
    let value: Value = serde_json::from_slice(bytes)?;
    if value.get("version").is_some() && value.get("event").is_some() {
        serde_json::from_value(value)
    } else {
        Ok(EventEnvelope {
            version: 0,
            sequence: legacy_sequence,
            timestamp_ms: 0,
            event: SessionEvent::Legacy { record: value },
        })
    }
}

fn interrupted_turns(events: &[EventEnvelope]) -> BTreeSet<String> {
    let mut open = BTreeSet::new();
    for envelope in events {
        match &envelope.event {
            SessionEvent::TurnStarted { turn_id } => {
                open.insert(turn_id.clone());
            }
            SessionEvent::TurnCompleted { turn_id }
            | SessionEvent::TurnFailed { turn_id, .. }
            | SessionEvent::TurnCancelled { turn_id } => {
                open.remove(turn_id);
            }
            _ => {}
        }
    }
    open
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("failed to create journal directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open journal {path}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read journal {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write journal {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize journal {path}: {source}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to parse journal {path} at line {line}: {source}")]
    Parse {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pi-session-{name}-{}.jsonl", std::process::id()))
    }

    #[test]
    fn round_trips_and_detects_interrupted_turn() {
        let path = temp_path("round-trip");
        let _ = fs::remove_file(&path);
        let (mut journal, _) = JsonlJournal::open(&path).unwrap();
        journal
            .append(SessionEvent::TurnStarted {
                turn_id: "turn-1".to_string(),
            })
            .unwrap();
        let report = load(&path).unwrap();
        assert_eq!(report.events.len(), 1);
        assert!(report.interrupted_turns.contains("turn-1"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn ignores_only_a_truncated_final_record() {
        let path = temp_path("truncated");
        let _ = fs::remove_file(&path);
        let (mut journal, _) = JsonlJournal::open(&path).unwrap();
        journal
            .append(SessionEvent::Metadata {
                values: BTreeMap::new(),
            })
            .unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"version":1,"sequence"#).unwrap();
        let report = load(&path).unwrap();
        assert_eq!(report.events.len(), 1);
        assert!(report.ignored_trailing_bytes > 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_legacy_pi_jsonl() {
        let path = temp_path("legacy");
        fs::write(&path, "{\"type\":\"started\",\"session_id\":\"old\"}\n").unwrap();
        let report = load(&path).unwrap();
        assert_eq!(report.events[0].version, 0);
        assert!(matches!(
            report.events[0].event,
            SessionEvent::Legacy { .. }
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn appends_land_as_whole_lines_even_after_a_torn_tail() {
        let path = temp_path("append-lines");
        let _ = fs::remove_file(&path);
        append_jsonl_record(&path, &serde_json::json!({ "type": "a" })).unwrap();
        append_jsonl_record(&path, &serde_json::json!({ "type": "b" })).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"type\":\"a\"}\n{\"type\":\"b\"}\n"
        );

        // An interrupted earlier write is dropped instead of glued onto.
        fs::write(&path, "{\"type\":\"a\"}\n{\"type\":\"b\",\"par").unwrap();
        append_jsonl_record(&path, &serde_json::json!({ "type": "c" })).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"type\":\"a\"}\n{\"type\":\"c\"}\n"
        );

        // A complete record that only lost its newline is kept.
        fs::write(&path, "{\"type\":\"a\"}").unwrap();
        append_jsonl_record(&path, &serde_json::json!({ "type": "c" })).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"type\":\"a\"}\n{\"type\":\"c\"}\n"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn repair_terminates_complete_records_and_truncates_torn_ones() {
        let path = temp_path("repair");
        let _ = fs::remove_file(&path);
        assert_eq!(repair_jsonl_tail(&path).unwrap(), TailRepair::Clean);
        assert!(
            !path.exists(),
            "repairing a missing journal must not create it"
        );

        let cases: [(&str, TailRepair, &str); 6] = [
            ("", TailRepair::Clean, ""),
            ("{\"a\":1}\n", TailRepair::Clean, "{\"a\":1}\n"),
            (
                "{\"a\":1}\n{\"b\":2}",
                TailRepair::Terminated,
                "{\"a\":1}\n{\"b\":2}\n",
            ),
            (
                "{\"a\":1}\n{\"b\":",
                TailRepair::Truncated(5),
                "{\"a\":1}\n",
            ),
            ("{\"b\":", TailRepair::Truncated(5), ""),
            // Two records glued without a newline are corrupt, not complete.
            (
                "{\"a\":1}\n{\"b\":2}{\"c\"",
                TailRepair::Truncated(11),
                "{\"a\":1}\n",
            ),
        ];
        for (content, repair, repaired) in cases {
            fs::write(&path, content).unwrap();
            assert_eq!(repair_jsonl_tail(&path).unwrap(), repair, "{content:?}");
            assert_eq!(fs::read_to_string(&path).unwrap(), repaired, "{content:?}");
        }

        // The backwards newline scan crosses its 64 KiB chunk boundary.
        let long = "x".repeat(200_000);
        let torn = format!("{{\"a\":1}}\n{{\"b\":\"{long}");
        fs::write(&path, &torn).unwrap();
        assert_eq!(
            repair_jsonl_tail(&path).unwrap(),
            TailRepair::Truncated(torn.len() as u64 - 8)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\":1}\n");
        let complete = format!("{{\"a\":1}}\n{{\"b\":\"{long}\"}}");
        fs::write(&path, &complete).unwrap();
        assert_eq!(repair_jsonl_tail(&path).unwrap(), TailRepair::Terminated);
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{complete}\n"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn in_memory_repair_drops_torn_segments_instead_of_patching_them() {
        let mut torn = b"{\"a\":1}\n{\"b\":".to_vec();
        assert_eq!(repair_jsonl_tail_bytes(&mut torn), TailRepair::Truncated(5));
        assert_eq!(torn, b"{\"a\":1}\n");
        let mut unterminated = b"{\"a\":1}\n{\"b\":2}".to_vec();
        assert_eq!(
            repair_jsonl_tail_bytes(&mut unterminated),
            TailRepair::Terminated
        );
        assert_eq!(unterminated, b"{\"a\":1}\n{\"b\":2}\n");
        let mut clean = b"{\"a\":1}\n".to_vec();
        assert_eq!(repair_jsonl_tail_bytes(&mut clean), TailRepair::Clean);
        assert_eq!(clean, b"{\"a\":1}\n");
    }
}
