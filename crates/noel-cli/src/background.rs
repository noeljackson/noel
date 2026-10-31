//! Background runs (spec §9.6): `& <prompt>` in the TUI forks the session
//! and submits the prompt to a `noel server` worker, detached — the TUI
//! stays free, the fork's run outlives it. `/bg` lists the forks this
//! session started and what their last journal record says.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Result};
use noel_core::{Runtime, SessionStore};

use crate::server::own_exe;

/// Forks `runtime`'s session and submits `prompt` to it through
/// `noel client send`, detached: the client starts a server when none
/// listens, and the server's worker (default idle exit 60 s) keeps the
/// run alive after the client exits. Returns the fork's session id.
pub fn start_background_run(
    runtime: &Runtime,
    session_dir: &Path,
    offline: bool,
    prompt: &str,
) -> Result<String> {
    let source_path = session_dir.join(format!("{}.jsonl", runtime.session().session_id));
    let (store, state) = SessionStore::fork(session_dir, &SessionStore::read(&source_path)?, false)
        .map_err(|error| anyhow!("forking the session: {error}"))?;
    let id = state.session_id.clone();
    drop(store);
    let mut command = Command::new(own_exe());
    command
        .args(["client", "send", &id, prompt])
        .env("NOEL_DIR", session_dir.parent().unwrap_or(Path::new(".")))
        // The client subcommand has no `--offline`; the server it starts
        // reads `NOEL_OFFLINE` from its environment, which this env
        // carries to both.
        .env("NOEL_OFFLINE", if offline { "1" } else { "" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(
            session_dir
                .parent()
                .unwrap_or(Path::new("."))
                .join("background.log"),
        )?);
    command
        .spawn()
        .map_err(|error| anyhow!("starting noel client: {error}"))?;
    Ok(id)
}

/// One background session line for `/bg`: the id and its last run's
/// state, read from the fork's journal (a read-only peek that tolerates a
/// partial last line, as any load does).
pub fn background_status(path: &Path) -> String {
    let id = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let state = SessionStore::read(path).ok();
    let (runs, messages) = match &state {
        Some(state) => (state.runs.clone(), state.messages.len()),
        None => return format!("{id}: journal unreadable"),
    };
    let last = runs.last().map(|run| {
        format!(
            "run {}: {}",
            run.id,
            match run.status {
                noel_core::RunStatus::Running => "running".to_string(),
                noel_core::RunStatus::Interrupted => "interrupted".to_string(),
                noel_core::RunStatus::Completed => "completed".to_string(),
                noel_core::RunStatus::Failed => "failed".to_string(),
                noel_core::RunStatus::Aborted => "aborted".to_string(),
            }
        )
    });
    match last {
        Some(run) => format!("{id}: {run}, {} messages", messages),
        None => format!("{id}: no runs, {messages} messages"),
    }
}

/// The forks `/bg` lists: this session's own children (its id is the
/// journal's `parent_session_id`), newest first.
pub fn background_sessions(session_dir: &Path, parent: &str) -> Vec<PathBuf> {
    let mut forks: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(session_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .filter(|path| {
            SessionStore::read(path)
                .map(|state| state.parent_session_id.as_deref() == Some(parent))
                .unwrap_or(false)
        })
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    forks.sort_by_key(|(modified, _)| *modified);
    forks.into_iter().map(|(_, path)| path).collect()
}
