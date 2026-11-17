//! A SQLite-backed [`JournalStorage`] (plan slice 2, docs/durable-sdk-2026-11.md).
//!
//! The journal contract stays noel's (audit item 10): a sequence of JSON
//! records, appended atomically one at a time, read back whole, and
//! rewritten by atomic replacement. Durable's table store is not adopted;
//! what its storage conformance checks that applies to a journal —
//! durability of each append, atomic rewrites that a crash cannot tear,
//! one writer at a time — is the conformance suite in `tests/storage.rs`.
//!
//! The layout is one table, one row per record, in append order:
//!
//! ```sql
//! CREATE TABLE journal (
//!   seq     INTEGER PRIMARY KEY AUTOINCREMENT,
//!   record  BLOB NOT NULL
//! );
//! ```
//!
//! A rewrite is one transaction that deletes and re-inserts; SQLite's
//! rollback journal makes it atomic, and `synchronous=FULL` makes a
//! committed record durable. One connection is one writer: the session
//! lock guards the file (`noel-core`'s `JsonlFile` opens the same
//! `<journal>.lock`); this storage takes the same lock through
//! [`SqliteJournal::open`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use noel_core::storage::{JournalStorage, Rewrite};
use noel_core::{SessionError, SessionLock};
use rusqlite::Connection;

/// The SQLite journal: `<path>` is the database file.
#[derive(Debug)]
pub struct SqliteJournal {
    path: PathBuf,
    lock: Arc<SessionLock>,
    connection: Mutex<Connection>,
}

impl SqliteJournal {
    /// Opens (creating if needed) the journal at `path`, taking the session
    /// lock so a second process fails with [`SessionError::Locked`].
    pub fn open(path: impl Into<PathBuf>) -> Result<SqliteJournal, SessionError> {
        let path = path.into();
        let lock = noel_core::acquire_session_lock(&path)?;
        let connection = Connection::open(&path).map_err(|source| SessionError::Open {
            path: path.clone(),
            source: std::io::Error::other(source),
        })?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS journal (
                     seq    INTEGER PRIMARY KEY AUTOINCREMENT,
                     record BLOB NOT NULL
                 );",
            )
            .map_err(|source| SessionError::Write {
                path: path.clone(),
                source: std::io::Error::other(source),
            })?;
        Ok(SqliteJournal {
            path,
            lock,
            connection: std::sync::Mutex::new(connection),
        })
    }

    /// An in-memory journal (SQLite `:memory:`), for tests and embedders
    /// without a file system. Named by `path` for errors and identity.
    pub fn in_memory(path: impl Into<PathBuf>) -> Result<SqliteJournal, SessionError> {
        let path = path.into();
        let connection = Connection::open_in_memory().map_err(|source| SessionError::Open {
            path: path.clone(),
            source: std::io::Error::other(source),
        })?;
        connection
            .execute_batch(
                "PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS journal (
                     seq    INTEGER PRIMARY KEY AUTOINCREMENT,
                     record BLOB NOT NULL
                 );",
            )
            .map_err(|source| SessionError::Write {
                path: path.clone(),
                source: std::io::Error::other(source),
            })?;
        Ok(SqliteJournal {
            path,
            lock: Arc::new(SessionLock::memory()),
            connection: std::sync::Mutex::new(connection),
        })
    }
}

impl JournalStorage for SqliteJournal {
    fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, record: &[u8]) -> Result<(), SessionError> {
        let _writer = self.lock.writer();
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        connection
            .execute("INSERT INTO journal (record) VALUES (?1)", [record])
            .map_err(|source| SessionError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        Ok(())
    }

    fn read(&self) -> Result<Vec<u8>, SessionError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut statement = connection
            .prepare("SELECT record FROM journal ORDER BY seq")
            .map_err(|source| SessionError::Read {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        let records = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|source| SessionError::Read {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        let mut journal = Vec::new();
        for record in records {
            let record = record.map_err(|source| SessionError::Read {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
            journal.extend_from_slice(&record);
            journal.push(b'\n');
        }
        Ok(journal)
    }

    fn rewrite(&self, rewrite: Rewrite<'_>) -> Result<(), SessionError> {
        let _writer = self.lock.writer();
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut current = Vec::new();
        {
            let mut statement = connection
                .prepare("SELECT record FROM journal ORDER BY seq")
                .map_err(|source| SessionError::Read {
                    path: self.path.clone(),
                    source: std::io::Error::other(source),
                })?;
            let records = statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .map_err(|source| SessionError::Read {
                    path: self.path.clone(),
                    source: std::io::Error::other(source),
                })?;
            for record in records {
                let record = record.map_err(|source| SessionError::Read {
                    path: self.path.clone(),
                    source: std::io::Error::other(source),
                })?;
                current.extend_from_slice(&record);
                current.push(b'\n');
            }
        }
        let rewritten = rewrite(current)?;
        let mut connection = connection;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|source| SessionError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        transaction
            .execute("DELETE FROM journal", [])
            .map_err(|source| SessionError::Write {
                path: self.path.clone(),
                source: std::io::Error::other(source),
            })?;
        for line in rewritten.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            transaction
                .execute("INSERT INTO journal (record) VALUES (?1)", [line])
                .map_err(|source| SessionError::Write {
                    path: self.path.clone(),
                    source: std::io::Error::other(source),
                })?;
        }
        transaction.commit().map_err(|source| SessionError::Write {
            path: self.path.clone(),
            source: std::io::Error::other(source),
        })?;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
