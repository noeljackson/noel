//! Exclusive ownership of a session journal for writing.
//!
//! The lock is `File::try_lock` on a separate `<journal>.lock` file, never on
//! the journal itself: compaction replaces the journal by rename, which would
//! silently orphan a lock held on the old inode. The lock file records the
//! holder's pid for error messages. Std opens files close-on-exec, so the
//! dogfood exec-restart releases the lock before the new image reopens the
//! session. Handles in one process share one lock through a registry keyed by
//! the canonical journal path, so a process can reopen a session it holds.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use crate::SessionError;

/// Bounds retries when the lock file is replaced between open and lock.
const LOCK_ATTEMPTS: usize = 16;

#[derive(Debug)]
pub(crate) struct SessionLock {
    journal: PathBuf,
    lock_path: PathBuf,
    /// Holds the OS lock; taken in `drop` while the registry is locked.
    file: Option<File>,
    /// Serializes appends and rewrites across every handle in this process.
    writer: Mutex<()>,
}

impl SessionLock {
    pub(crate) fn writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

struct Registry {
    held: Mutex<HashMap<PathBuf, Weak<SessionLock>>>,
    released: Condvar,
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry {
        held: Mutex::new(HashMap::new()),
        released: Condvar::new(),
    })
}

/// Takes the session lock for `journal`, or shares it when this process
/// already holds it. Never waits for another process: a held lock fails at
/// once with [`SessionError::Locked`], leaving the wait policy to callers.
pub(crate) fn acquire(journal: &Path) -> Result<Arc<SessionLock>, SessionError> {
    let journal = canonical_journal_path(journal);
    let registry = registry();
    let mut held = registry.held.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        match held.get(&journal).map(Weak::upgrade) {
            Some(Some(lock)) => return Ok(lock),
            // Another thread is dropping the last handle; its drop removes the
            // entry and releases the OS lock before notifying.
            Some(None) => {
                held = registry
                    .released
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            None => break,
        }
    }
    let lock_path = lock_path_for(&journal);
    let file = lock_file(&journal, &lock_path)?;
    let lock = Arc::new(SessionLock {
        journal: journal.clone(),
        lock_path,
        file: Some(file),
        writer: Mutex::new(()),
    });
    held.insert(journal, Arc::downgrade(&lock));
    Ok(lock)
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let registry = registry();
        let mut held = registry.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held
            .get(&self.journal)
            .is_some_and(|handle| handle.strong_count() == 0)
        {
            held.remove(&self.journal);
        }
        if let Some(file) = self.file.take() {
            // Unlink while still holding the lock: a process that opened this
            // inode meanwhile sees it orphaned and retries on a fresh file.
            if is_current_file(&file, &self.lock_path) {
                let _ = fs::remove_file(&self.lock_path);
            }
            drop(file);
        }
        drop(held);
        registry.released.notify_all();
    }
}

fn lock_file(journal: &Path, lock_path: &Path) -> Result<File, SessionError> {
    let lock_error = |source| SessionError::Lock {
        path: journal.to_path_buf(),
        source,
    };
    for _ in 0..LOCK_ATTEMPTS {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(lock_error)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(SessionError::Locked {
                    path: journal.to_path_buf(),
                    pid: holder_pid(&mut file),
                })
            }
            Err(TryLockError::Error(source)) => return Err(lock_error(source)),
        }
        // A holder unlinks its lock file before releasing it; a lock won on
        // that orphaned inode guards nothing, so start over.
        if !is_current_file(&file, lock_path) {
            continue;
        }
        file.set_len(0)
            .and_then(|()| writeln!(file, "{}", std::process::id()))
            .map_err(lock_error)?;
        return Ok(file);
    }
    Err(lock_error(io::Error::other(
        "lock file kept changing while locking",
    )))
}

fn holder_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(unix)]
fn is_current_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), fs::metadata(path)) {
        (Ok(held), Ok(current)) => held.dev() == current.dev() && held.ino() == current.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_current_file(_file: &File, path: &Path) -> bool {
    path.exists()
}

/// One key per journal however it is named; a journal about to be created is
/// keyed by its canonical directory.
fn canonical_journal_path(journal: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(journal) {
        return path;
    }
    let parent = journal
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match (fs::canonicalize(parent), journal.file_name()) {
        (Ok(directory), Some(name)) => directory.join(name),
        _ => std::path::absolute(journal).unwrap_or_else(|_| journal.to_path_buf()),
    }
}

fn lock_path_for(journal: &Path) -> PathBuf {
    let mut name = journal.file_name().map(OsString::from).unwrap_or_default();
    name.push(".lock");
    journal.with_file_name(name)
}
