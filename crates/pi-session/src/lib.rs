//! Versioned, append-only JSONL journals shared by interactive agents.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| JournalError::Open {
                path: self.path.clone(),
                source,
            })?;
        serde_json::to_writer(&mut file, &envelope).map_err(|source| JournalError::Serialize {
            path: self.path.clone(),
            source,
        })?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_data())
            .map_err(|source| JournalError::Write {
                path: self.path.clone(),
                source,
            })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(envelope)
    }
}

/// Durably appends an application-specific legacy record. This lets existing
/// Pi sessions adopt the shared write guarantees without changing their wire
/// format in place.
pub fn append_jsonl_record<T: Serialize>(path: &Path, record: &T) -> Result<(), JournalError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| JournalError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| JournalError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::to_writer(&mut file, record).map_err(|source| JournalError::Serialize {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_data())
        .map_err(|source| JournalError::Write {
            path: path.to_path_buf(),
            source,
        })
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
}
