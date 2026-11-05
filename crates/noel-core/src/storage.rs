//! Where a session journal lives (roadmap phase 6): a sequence of JSON
//! records, one per line, that [`crate::SessionStore`] appends to, reads
//! back and rewrites. The store owns what the records mean (recovery,
//! checkpoints, history); a storage only keeps the bytes, durably and with
//! one writer at a time.
//!
//! - [`JsonlFile`]: the journal file under the session lock, as noel has
//!   always kept it.
//! - [`MemoryJournal`]: the same contract in memory, for tests and for
//!   embedders without a file system.

use std::any::Any;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::session_lock::{self, SessionLock};
use crate::SessionError;

/// A rewrite of a journal's bytes: given the current journal (its torn
/// tail, if any, already dropped), the journal that replaces it.
pub type Rewrite<'a> = &'a mut dyn FnMut(Vec<u8>) -> Result<Vec<u8>, SessionError>;

/// The bytes of one session journal.
pub trait JournalStorage: Send + Sync + fmt::Debug + Any {
    /// Where the journal is, for errors and the session's identity.
    fn path(&self) -> &Path;

    /// Appends one record (its JSON, without the newline). The record and its
    /// newline are durable when this returns, and a failed append leaves no
    /// partial record behind.
    fn append(&self, record: &[u8]) -> Result<(), SessionError>;

    /// The journal as written; a torn final segment is left for the reader
    /// to ignore.
    fn read(&self) -> Result<Vec<u8>, SessionError>;

    /// Replaces the journal with what `rewrite` makes of it, atomically: no
    /// append lands between the read and the replacement, and a failure
    /// leaves the previous journal.
    fn rewrite(&self, rewrite: Rewrite<'_>) -> Result<(), SessionError>;

    fn as_any(&self) -> &dyn Any;
}

/// A shared storage, as a store and its clones hold it.
pub type Storage = Arc<dyn JournalStorage>;

/// The journal file, written only while this process holds the session lock
/// (see [`SessionError::Locked`]); handles of one process share the lock and
/// serialize their writes.
#[derive(Debug)]
pub struct JsonlFile {
    path: PathBuf,
    lock: Arc<SessionLock>,
}

impl JsonlFile {
    /// Takes the session lock and repairs a torn tail before the first
    /// append.
    pub fn open(path: PathBuf) -> Result<JsonlFile, SessionError> {
        let file = JsonlFile {
            lock: session_lock::acquire(&path)?,
            path,
        };
        {
            let _writer = file.lock.writer();
            noel_session::repair_jsonl_tail(&file.path)?;
        }
        Ok(file)
    }

    /// The lock without the repair (a test points it at a directory).
    #[cfg(test)]
    pub(crate) fn unrepaired(path: PathBuf) -> Result<JsonlFile, SessionError> {
        Ok(JsonlFile {
            lock: session_lock::acquire(&path)?,
            path,
        })
    }

    #[cfg(test)]
    pub(crate) fn lock(&self) -> &Arc<SessionLock> {
        &self.lock
    }
}

impl JournalStorage for JsonlFile {
    fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, record: &[u8]) -> Result<(), SessionError> {
        let _writer = self.lock.writer();
        noel_session::append_jsonl_line(&self.path, record)?;
        Ok(())
    }

    fn read(&self) -> Result<Vec<u8>, SessionError> {
        std::fs::read(&self.path).map_err(|source| SessionError::Read {
            path: self.path.clone(),
            source,
        })
    }

    fn rewrite(&self, rewrite: Rewrite<'_>) -> Result<(), SessionError> {
        let _writer = self.lock.writer();
        let mut journal = match std::fs::read(&self.path) {
            Ok(journal) => journal,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => {
                return Err(SessionError::Read {
                    path: self.path.clone(),
                    source,
                })
            }
        };
        noel_session::repair_jsonl_tail_bytes(&mut journal);
        let journal = rewrite(journal)?;
        // A synced temporary file renamed over the journal: a failed write
        // leaves the old one. The lock lives in its own file.
        noel_config::write_file_atomic(&self.path, &journal)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A journal in memory, named by `path`.
#[derive(Debug)]
pub struct MemoryJournal {
    path: PathBuf,
    content: Mutex<Vec<u8>>,
}

impl MemoryJournal {
    /// An empty journal.
    pub fn new(path: impl Into<PathBuf>) -> MemoryJournal {
        MemoryJournal::with_bytes(path, Vec::new())
    }

    /// A journal holding `content`, its torn tail repaired as a writer
    /// repairs one.
    pub fn with_bytes(path: impl Into<PathBuf>, mut content: Vec<u8>) -> MemoryJournal {
        noel_session::repair_jsonl_tail_bytes(&mut content);
        MemoryJournal {
            path: path.into(),
            content: Mutex::new(content),
        }
    }

    /// The journal's bytes now.
    pub fn bytes(&self) -> Vec<u8> {
        self.content
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl JournalStorage for MemoryJournal {
    fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, record: &[u8]) -> Result<(), SessionError> {
        let mut content = self.content.lock().unwrap_or_else(PoisonError::into_inner);
        noel_session::repair_jsonl_tail_bytes(&mut content);
        content.extend_from_slice(record);
        content.push(b'\n');
        Ok(())
    }

    fn read(&self) -> Result<Vec<u8>, SessionError> {
        Ok(self.bytes())
    }

    fn rewrite(&self, rewrite: Rewrite<'_>) -> Result<(), SessionError> {
        let mut content = self.content.lock().unwrap_or_else(PoisonError::into_inner);
        let mut journal = content.clone();
        noel_session::repair_jsonl_tail_bytes(&mut journal);
        *content = rewrite(journal)?;
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
