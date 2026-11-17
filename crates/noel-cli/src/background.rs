//! Background runs (spec §9.6): `& <prompt>`, or `/bg <prompt>`, forks the
//! session and sends the prompt to the fork, which the server runs in a
//! worker of its own while this session stays free. `/bg` lists the forks
//! this session started and what their last run came to.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use noel_core::{RunStatus, Runtime, SessionStore};

use crate::server::own_exe;

/// How long the prompt may take to reach the fork's worker, which the
/// server starts for it.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// Forks `runtime`'s session as it is now and sends `prompt` to the fork
/// through `noel client send`, as a client would: the server starts a
/// worker for it, and the run goes on whatever this session does next.
/// Returns the fork's session id once the prompt is accepted.
pub(crate) async fn start(
    runtime: &Runtime,
    session_dir: &Path,
    offline: bool,
    prompt: &str,
) -> Result<String> {
    let (store, state) = SessionStore::fork(session_dir, runtime.session(), false)
        .map_err(|error| anyhow!("forking the session: {error}"))?;
    let id = state.session_id.clone();
    // The fork's worker opens the journal: this process lets go of it.
    drop(store);
    let mut send = tokio::process::Command::new(own_exe());
    // `--` so a prompt that starts with a dash is not read as an option.
    send.args(["client", "send", &id, "--", prompt])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if offline {
        send.env("NOEL_OFFLINE", "1");
    }
    let sent = tokio::time::timeout(SEND_TIMEOUT, send.output())
        .await
        .map_err(|_| anyhow!("{id}: the prompt was not accepted within 30 s"))?
        .map_err(|error| anyhow!("starting noel client: {error}"))?;
    if !sent.status.success() {
        return Err(anyhow!(
            "{id}: {}",
            String::from_utf8_lossy(&sent.stderr).trim()
        ));
    }
    Ok(id)
}

/// The forks of session `parent`, newest first, each with its last run's
/// state and its size: what `/bg` shows. Empty when there are none.
pub(crate) fn list(session_dir: &Path, parent: &str) -> Vec<String> {
    let mut forks: Vec<_> = SessionStore::list(session_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|session| session.parent_session_id.as_deref() == Some(parent))
        .collect();
    forks.sort_by_key(|session| std::cmp::Reverse(session.modified));
    forks
        .iter()
        .map(|session| {
            let Ok(state) = SessionStore::read(&session.path) else {
                return format!("{}  journal unreadable", session.session_id);
            };
            // A journal cannot tell a run in progress from one a stopped
            // worker left: both lack their end.
            let run = match state.runs.last().map(|run| run.status) {
                Some(RunStatus::Running | RunStatus::Interrupted) => "unfinished",
                Some(RunStatus::Completed) => "completed",
                Some(RunStatus::Failed) => "failed",
                Some(RunStatus::Aborted) => "aborted",
                None => "no run yet",
            };
            // What it was sent: the fork's last prompt.
            let prompt: String = state
                .messages
                .iter()
                .rev()
                .find(|message| message.role == noel_core::MessageRole::User)
                .and_then(|message| message.content.lines().find(|line| !line.trim().is_empty()))
                .unwrap_or_default()
                .trim()
                .chars()
                .take(60)
                .collect();
            format!(
                "{}  {run} · {} messages · {prompt}",
                session.session_id,
                state.messages.len()
            )
        })
        .collect()
}
