//! Stored-commit observation for embedding hosts.

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use noel_core::storage::{JournalStorage, Rewrite, Storage};

/// One journal commit after it was stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitPublication {
    /// Monotonic within this journal, including commits made before open.
    pub sequence: u64,
    /// The journal record's stable serde tag, or `rewrite`.
    pub kind: String,
}

pub(crate) struct CommitFeed {
    next: Mutex<u64>,
    subscribers: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<CommitPublication>>>,
}

impl CommitFeed {
    pub(crate) fn new(existing_records: u64) -> Arc<Self> {
        Arc::new(Self {
            next: Mutex::new(existing_records.saturating_add(1)),
            subscribers: Mutex::new(Vec::new()),
        })
    }

    fn publish(&self, kind: String) {
        let sequence = {
            let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
            let sequence = *next;
            *next = next.saturating_add(1);
            sequence
        };
        let publication = CommitPublication { sequence, kind };
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|sender| sender.send(publication.clone()).is_ok());
    }

    pub(crate) fn subscribe(&self) -> CommitSubscription {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(sender);
        CommitSubscription { receiver }
    }

    pub(crate) fn close(&self) {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// A live feed of commits stored after attachment.
pub struct CommitSubscription {
    receiver: tokio::sync::mpsc::UnboundedReceiver<CommitPublication>,
}

impl CommitSubscription {
    pub fn try_next(&mut self) -> Option<CommitPublication> {
        self.receiver.try_recv().ok()
    }

    pub async fn next(&mut self) -> Option<CommitPublication> {
        self.receiver.recv().await
    }
}

/// Observes successful storage mutations without changing the journal
/// format or asking every runtime call site to publish separately.
pub(crate) struct ObservedStorage {
    inner: Storage,
    commits: Arc<CommitFeed>,
}

impl ObservedStorage {
    pub(crate) fn wrap(inner: Storage, commits: Arc<CommitFeed>) -> Storage {
        Arc::new(Self { inner, commits })
    }
}

impl fmt::Debug for ObservedStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservedStorage")
            .field("path", &self.inner.path())
            .finish()
    }
}

impl JournalStorage for ObservedStorage {
    fn path(&self) -> &Path {
        self.inner.path()
    }

    fn append(&self, record: &[u8]) -> Result<(), noel_core::SessionError> {
        self.inner.append(record)?;
        let kind = serde_json::from_slice::<serde_json::Value>(record)
            .ok()
            .and_then(|record| {
                record
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".to_owned());
        self.commits.publish(kind);
        Ok(())
    }

    fn read(&self) -> Result<Vec<u8>, noel_core::SessionError> {
        self.inner.read()
    }

    fn rewrite(&self, rewrite: Rewrite<'_>) -> Result<(), noel_core::SessionError> {
        self.inner.rewrite(rewrite)?;
        self.commits.publish("rewrite".to_owned());
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
