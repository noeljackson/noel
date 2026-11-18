//! A Harness of many conversations (plan slice 2): one process, one
//! scheduler of driver tasks, many journals.
//!
//! Upstream Durable's Harness holds every conversation in one storage and
//! runs one scheduler across them. noel's session model is one journal per
//! session (`docs/spec.md` §10), so the SDK's Harness of many is a
//! manager over per-conversation stores: each conversation is one
//! [`ConversationHandle`] with its own driver (slice 1's), created here,
//! found again by id, and forked from another conversation's saved state.
//!
//! The journal layout: conversations of one harness live under one
//! directory as `<dir>/<id>.jsonl` (noel's own), or `<dir>/<id>.sqlite`
//! for the SQLite journal — each conversation its own file, its own
//! session lock, and its own crash domain: a corrupted or locked journal
//! strands one conversation, never the harness (`docs/spec.md`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use noel_core::storage::{JsonlFile, Storage};
use tokio::sync::Mutex as AsyncMutex;

use crate::driver::Handle;
use crate::{Harness, HarnessError, HarnessOptions};

/// What a harness of many conversations is opened with: slice 1's options
/// plus where conversations live.
#[derive(Clone)]
pub struct MultiOptions {
    pub harness: HarnessOptions,
    /// The directory conversations' journals live in.
    pub dir: PathBuf,
    /// `sqlite` keeps each conversation in `<dir>/<id>.sqlite`
    /// ([`crate::SqliteJournal``]); the default is noel's own
    /// `<dir>/<id>.jsonl`.
    pub backend: Backend,
}

/// How conversations' journals are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Jsonl,
    Sqlite,
}

impl MultiOptions {
    pub fn new(harness: HarnessOptions, dir: impl Into<PathBuf>) -> MultiOptions {
        MultiOptions {
            harness,
            dir: dir.into(),
            backend: Backend::default(),
        }
    }

    pub fn sqlite(harness: HarnessOptions, dir: impl Into<PathBuf>) -> MultiOptions {
        MultiOptions {
            harness,
            dir: dir.into(),
            backend: Backend::Sqlite,
        }
    }
}

/// One harness, many conversations: created, found, forked. All of them
/// share the options (models, tools, providers); each is its own session
/// with its own journal.
pub struct MultiHarness {
    options: MultiOptions,
    conversations: AsyncMutex<BTreeMap<String, Handle>>,
}

impl MultiHarness {
    /// Opens the harness on `dir`, taking every conversation's journal it
    /// finds there (a journal with an interrupted run resumes at its
    /// conversation's first use, slice 1's rule).
    pub async fn open(options: MultiOptions) -> Result<MultiHarness, HarnessError> {
        std::fs::create_dir_all(&options.dir).map_err(|source| {
            HarnessError::Storage(noel_core::SessionError::CreateDir {
                path: options.dir.clone(),
                source,
            })
        })?;
        Ok(MultiHarness {
            options,
            conversations: AsyncMutex::new(BTreeMap::new()),
        })
    }

    /// The conversations this harness knows: the journals in its
    /// directory, by id.
    pub fn list(&self) -> Vec<String> {
        let suffix = match self.options.backend {
            Backend::Jsonl => "jsonl",
            Backend::Sqlite => "sqlite",
        };
        let mut ids = std::fs::read_dir(&self.options.dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        name.strip_suffix(&format!(".{suffix}")).map(str::to_owned)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    fn journal(&self, id: &str) -> PathBuf {
        match self.options.backend {
            Backend::Jsonl => self.options.dir.join(format!("{id}.jsonl")),
            Backend::Sqlite => self.options.dir.join(format!("{id}.sqlite")),
        }
    }

    fn storage(&self, id: &str) -> Result<Storage, HarnessError> {
        Ok(match self.options.backend {
            Backend::Jsonl => Arc::new(JsonlFile::open(self.journal(id))?),
            Backend::Sqlite => Arc::new(crate::SqliteJournal::open(self.journal(id))?),
        })
    }

    /// Opens the conversation `id` (creating its journal when it does not
    /// exist) and returns its harness handle.
    pub async fn conversation(&self, id: &str) -> Result<Harness, HarnessError> {
        {
            let conversations = self.conversations.lock().await;
            if let Some(handle) = conversations.get(id) {
                return Ok(Harness::from_handle(
                    handle.clone(),
                    id.to_owned(),
                    self.journal(id),
                ));
            }
        }
        let storage = self.storage(id)?;
        let options = self.options.harness.clone();
        let harness = Harness::open_named(storage, id.to_owned(), options).await?;
        self.conversations
            .lock()
            .await
            .insert(id.to_owned(), harness.handle());
        Ok(harness)
    }

    /// Creates a conversation with a fresh id.
    pub async fn create_conversation(&self) -> Result<Harness, HarnessError> {
        let id = noel_core::new_session_id();
        self.conversation(&id).await
    }

    /// Forks `source` at its end (its saved session): a new conversation
    /// whose journal starts as a copy of the source's state, the way
    /// noel's sessions fork (`SessionStore::fork`).
    ///
    /// Upstream's fork is at an entry; noel's sessions fork at their saved
    /// state (the rewind surface chooses a point first). The plan records
    /// the difference: forks are at saved state, not at arbitrary entries.
    pub async fn fork(&self, source: &str) -> Result<Harness, HarnessError> {
        // Read the source's saved state and write it as a new session.
        let (_, state) = noel_core::SessionStore::open_storage(self.storage(source)?)?;
        let id = noel_core::new_session_id();
        let forked = self.storage(&id)?;
        noel_core::SessionStore::fork_into_storage(forked, &state, id.clone())?;
        self.conversation(&id).await
    }
}
