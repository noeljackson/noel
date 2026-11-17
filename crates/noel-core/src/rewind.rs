//! Rewind (spec §9.2): the snapshots an `edit`/`write` takes, and the way
//! back to an earlier prompt, of which `/undo` is the nearest.
//!
//! A snapshot holds what a file held before one edit, and, once the edit
//! has run, the hash of what it left. A rewind writes back, per file, the
//! oldest snapshot of the turns it drops, through the workspace the tools
//! ran in, and only where the file still holds what its last edit left: a
//! file changed outside the session since is left and named.

use noel_env::{Context, ExecutionEnv, RemoveOptions};

use crate::{sha256_hex, ConversationMessage, MessageRole, Runtime, SessionError, SnapshotRef};

/// What [`Runtime::undo_last_turn`] took back: the turn's prompt, and the
/// files its rewind restored and those it left (changed outside the
/// session since).
#[derive(Debug, Clone, PartialEq)]
pub struct Undone {
    pub prompt: ConversationMessage,
    pub restored: Vec<String>,
    pub refused: Vec<String>,
}

impl Runtime {
    /// Saves the file as it is now, before an `edit`/`write` changes it,
    /// and journals the snapshot. `env` is the workspace the tool runs in:
    /// the bytes are read there. A file that is there and cannot be read
    /// gets no snapshot, rather than one that says it was not there.
    pub async fn snapshot_before_edit(
        &mut self,
        path: &str,
        env: &dyn ExecutionEnv,
    ) -> Result<(), SessionError> {
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        let cx = Context::background();
        let bytes = match env.exists(path, &cx).await {
            Ok(false) => None,
            Ok(true) => match env.read_binary_file(path, &cx).await {
                Ok(bytes) => Some(bytes),
                Err(_) => return Ok(()),
            },
            Err(_) => return Ok(()),
        };
        let id = format!("snap-{}", self.session.snapshots.len() + 1);
        let mut snapshot = store.record_file_snapshot(&id, path, bytes)?;
        snapshot.message_index = self.session.messages.len();
        self.session.snapshots.push(snapshot);
        Ok(())
    }

    /// Records what the edit left in `path`, on the snapshot taken for it,
    /// so a rewind can tell whether the file still holds it. Nothing is
    /// recorded when the file is not there or cannot be read.
    pub async fn settle_snapshot(
        &mut self,
        path: &str,
        env: &dyn ExecutionEnv,
    ) -> Result<(), SessionError> {
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        let Ok(bytes) = env.read_binary_file(path, &Context::background()).await else {
            return Ok(());
        };
        let Some(snapshot) = self
            .session
            .snapshots
            .iter_mut()
            .rev()
            .find(|snapshot| snapshot.path == path)
        else {
            return Ok(());
        };
        let after_sha = sha256_hex(&bytes);
        store.record_file_edited(&snapshot.id, &after_sha)?;
        snapshot.after_sha = Some(after_sha);
        Ok(())
    }

    /// Rewinds to the first `messages` messages: drops the rest, and
    /// restores every file a dropped turn changed to what it held before
    /// the dropped range, where the file still holds what the session left
    /// there. Returns the restored paths and the ones left (changed outside
    /// the session since, or not writable). Nothing is changed when the
    /// workspace of the files cannot be opened.
    pub async fn rewind(
        &mut self,
        messages: usize,
    ) -> Result<(Vec<String>, Vec<String>), SessionError> {
        let Some(store) = self.store.clone() else {
            self.session.rewind_to(messages, &[]);
            return Ok((Vec::new(), Vec::new()));
        };
        // What a path goes back to is what it held when the dropped range
        // began: its oldest snapshot inside the range. Snapshots are in
        // journal order, so the first met is the oldest.
        let mut restore: Vec<SnapshotRef> = Vec::new();
        for snapshot in &self.session.snapshots {
            if snapshot.message_index >= messages
                && !restore.iter().any(|picked| picked.path == snapshot.path)
            {
                restore.push(snapshot.clone());
            }
        }
        let mut restored = Vec::new();
        let mut refused = Vec::new();
        if !restore.is_empty() {
            let env = self.tool_env().map_err(SessionError::Workspace)?;
            let cx = Context::background();
            for snapshot in &restore {
                let before = if snapshot.created {
                    None
                } else {
                    match store.read_snapshot(&snapshot.id) {
                        Ok(Some(bytes)) => Some(bytes),
                        // A snapshot whose bytes are gone restores nothing.
                        _ => {
                            refused.push(snapshot.path.clone());
                            continue;
                        }
                    }
                };
                let current = match env.exists(&snapshot.path, &cx).await {
                    Ok(false) => None,
                    Ok(true) => match env.read_binary_file(&snapshot.path, &cx).await {
                        Ok(bytes) => Some(bytes),
                        Err(_) => {
                            refused.push(snapshot.path.clone());
                            continue;
                        }
                    },
                    Err(_) => {
                        refused.push(snapshot.path.clone());
                        continue;
                    }
                };
                // Already as it was before the range: nothing to write.
                if current == before {
                    continue;
                }
                // The last edit of the path is what the session left in
                // it. Without its hash (an edit that was not seen to
                // finish) the session's write is still the last one known.
                let left = self
                    .session
                    .snapshots
                    .iter()
                    .rev()
                    .find(|other| other.path == snapshot.path)
                    .and_then(|last| last.after_sha.as_deref());
                let held = current.as_deref().map(sha256_hex);
                if left.is_some() && held.as_deref() != left {
                    refused.push(snapshot.path.clone());
                    continue;
                }
                let written = match &before {
                    Some(bytes) => env.write_file(&snapshot.path, bytes, &cx).await,
                    None => {
                        let options = RemoveOptions {
                            recursive: false,
                            force: true,
                        };
                        env.remove(&snapshot.path, options, &cx).await
                    }
                };
                match written {
                    Ok(()) => restored.push(snapshot.path.clone()),
                    Err(_) => refused.push(snapshot.path.clone()),
                }
            }
        }
        let ids = restore.into_iter().map(|snapshot| snapshot.id).collect();
        store.record_rewind(&mut self.session, messages, ids)?;
        Ok((restored, refused))
    }

    /// Rewinds the conversation to the first `messages` messages and
    /// leaves every file as it is. The dropped turns' changes stay
    /// restorable by a later rewind to an earlier prompt.
    pub fn rewind_keeping_files(&mut self, messages: usize) -> Result<(), SessionError> {
        if messages >= self.session.messages.len() {
            return Ok(());
        }
        match self.store.clone() {
            Some(store) => store.record_rewind(&mut self.session, messages, Vec::new()),
            None => {
                self.session.rewind_to(messages, &[]);
                Ok(())
            }
        }
    }

    /// Undoes the last turn: a rewind to its prompt, which is returned
    /// with the files the rewind restored and left. The tool audit,
    /// queues, runs and billed usage are deliberately unchanged.
    pub async fn undo_last_turn(&mut self) -> Result<Option<Undone>, SessionError> {
        let Some(start) = self
            .session
            .messages
            .iter()
            .rposition(|message| message.role == MessageRole::User)
        else {
            return Ok(None);
        };
        let prompt = self.session.messages[start].clone();
        let (restored, refused) = self.rewind(start).await?;
        Ok(Some(Undone {
            prompt,
            restored,
            refused,
        }))
    }
}
