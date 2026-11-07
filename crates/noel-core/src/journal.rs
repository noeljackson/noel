//! Reading a session journal: its bytes, its records replayed into a session, and how its last run stopped.

use super::*;

/// A session from its journal's bytes: noel's records, or pi's export.
pub(crate) fn session_state_from_bytes(
    path: &Path,
    content: &[u8],
) -> Result<SessionState, SessionError> {
    if jsonl_is_ts_session_export(content) {
        let export = read_ts_jsonl_export(path, content)?;
        let session_id = if export.session_id.trim().is_empty() {
            "recovered".to_string()
        } else {
            export.session_id.clone()
        };
        return Ok(session_state_from_export(export, session_id));
    }
    read_rust_journal(path, content)
}

/// Whether the journal's last record about its last reply is a
/// `retry_wait`: the run stopped during a retry's backoff.
pub(crate) fn waiting_retry(path: &Path) -> bool {
    let Ok(content) = fs::read(path) else {
        return false;
    };
    for line in content.split(|byte| *byte == b'\n').rev() {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("retry_wait") => return true,
            Some("partial") => {
                let streamed = ["text", "thinking"].iter().any(|key| {
                    record
                        .get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty())
                });
                if streamed {
                    return false;
                }
            }
            Some(
                "message"
                | "messages_snapshot"
                | "compaction_checkpoint"
                | "run_finished"
                | "run_started"
                | "run_restarted",
            ) => return false,
            _ => {}
        }
    }
    false
}

pub(crate) fn read_journal_bytes(path: &Path) -> Result<Vec<u8>, SessionError> {
    let mut file = File::open(path).map_err(|source| SessionError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)
        .map_err(|source| SessionError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(content)
}

pub(crate) fn is_blank_line(line: &[u8]) -> bool {
    std::str::from_utf8(line).is_ok_and(|line| line.trim().is_empty())
}

/// Replays a Rust session journal. Only an unterminated final segment that is
/// not a whole JSON value counts as an interrupted append and is ignored; any
/// other unreadable line, including a corrupt line in the middle, is an error.
pub(crate) fn read_rust_journal(path: &Path, content: &[u8]) -> Result<SessionState, SessionError> {
    let parse_error = |source| SessionError::Parse {
        path: path.to_path_buf(),
        source,
    };
    let (records, tail) = noel_session::split_jsonl_tail(content);
    let records = std::str::from_utf8(records).map_err(|error| SessionError::Read {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
    })?;
    let mut state = None;
    for line in records.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<SessionRecord>(line).map_err(parse_error)?;
        apply_session_record(&mut state, record);
    }
    if !is_blank_line(tail) {
        match serde_json::from_slice::<SessionRecord>(tail) {
            Ok(record) => apply_session_record(&mut state, record),
            Err(_) if !noel_session::is_complete_jsonl_record(tail) => {}
            Err(source) => return Err(parse_error(source)),
        }
    }
    Ok(state.unwrap_or_else(|| SessionState::new("recovered", PathBuf::from("."))))
}

pub(crate) fn apply_session_record(state: &mut Option<SessionState>, record: SessionRecord) {
    if let SessionRecord::Started { session_id, cwd } = record {
        *state = Some(SessionState::new(session_id, cwd));
        return;
    }
    // Records before the first `started` have no session to apply to.
    let Some(state) = state else {
        return;
    };
    match record {
        SessionRecord::Started { .. } | SessionRecord::Unknown => {}
        SessionRecord::Workspace(workspace) => state.workspace = Some(workspace),
        SessionRecord::Metadata {
            name,
            labels,
            parent_session_id,
        } => {
            state.name = name;
            state.labels = labels.into_iter().collect();
            state.parent_session_id = parent_session_id;
        }
        SessionRecord::Message {
            message,
            run,
            placed,
            ..
        } => {
            state.supersede_progress(&message);
            state.messages.push(message);
            // A message is progress: the active run's restarts start again.
            if let Some(active) = state
                .runs
                .last_mut()
                .filter(|run| !run.status.is_finished())
            {
                active.restarts = 0;
            }
            // Whatever started a run, a load did not: it was interrupted
            // unless a `run_finished` record follows.
            state
                .runs
                .extend(run.map(|run| Run::started(run, RunStatus::Interrupted)));
            // Submissions are placed only while a run is active: the last.
            let run_id = state.runs.last().map(|run| run.id.clone());
            state.place_queued(placed, run_id.as_deref());
        }
        SessionRecord::MessagesSnapshot { messages } => {
            state.partial = None;
            state.messages = messages;
        }
        SessionRecord::Tool { event } => state.tool_history.push(event),
        SessionRecord::ActiveModel { model } => state.active_model = model,
        SessionRecord::ActiveAccount { account } => state.active_account = account,
        SessionRecord::ActiveThinkingLevel { level } => state.active_thinking_level = level,
        SessionRecord::CompactionSettings { settings } => state.compaction = settings,
        SessionRecord::DisabledTools { tools } => {
            state.disabled_tools = tools.into_iter().collect();
            state.active_tool_names = active_tools_with(&state.disabled_tools);
        }
        SessionRecord::Todos { todos } => state.todos = todos,
        SessionRecord::EditedFiles { paths } => state.edited_files = paths,
        SessionRecord::CodemodeStore { store } => state.codemode_store = store,
        SessionRecord::QueuedMessage { message } => {
            // A record like this one names no identity; the others keep
            // theirs (#138).
            state.queue.push(QueuedEntry::follow_up(message));
        }
        SessionRecord::QueuedMessagesSnapshot {
            messages,
            steering,
            items,
            withdrawn,
        } => {
            let aligned = items.len() == messages.len();
            state.queue = messages
                .into_iter()
                .enumerate()
                .map(|(index, content)| QueuedEntry {
                    mode: if index < steering {
                        QueueMode::Steer
                    } else {
                        QueueMode::FollowUp
                    },
                    content,
                    id: aligned.then(|| items[index].clone()).flatten(),
                })
                .collect();
            state.past_submissions.extend(
                withdrawn
                    .into_iter()
                    .map(|id| PastSubmission { id, run_id: None }),
            );
        }
        SessionRecord::Compaction { record } => state.compactions.push(record),
        SessionRecord::CompactionCheckpoint { record, messages } => {
            state.partial = None;
            state.messages = messages;
            state.compactions.push(record);
        }
        SessionRecord::BranchSummary { summary } => state.branch_summaries.push(summary),
        SessionRecord::Usage { usage } => state.usage = usage,
        SessionRecord::RetryWait { .. } => {}
        SessionRecord::RunStarted { run } => {
            state.runs.push(Run::started(run, RunStatus::Interrupted));
        }
        SessionRecord::RunClosed { run_id } => {
            if let Some(run) = state.run_mut(&run_id) {
                run.close();
            }
        }
        SessionRecord::RunRestarted { run_id } => {
            if let Some(run) = state.run_mut(&run_id) {
                run.restart();
            }
        }
        SessionRecord::RunReopened { run_id } => {
            if let Some(run) = state.run_mut(&run_id) {
                run.reopen();
            }
        }
        SessionRecord::RunFinished {
            run_id,
            status,
            failure,
        } => {
            if let Some(run) = state.run_mut(&run_id) {
                run.finish(status, failure);
            }
            // The run settled what it left running.
            state.partial = None;
            state.tool_outputs.clear();
        }
        SessionRecord::Partial {
            run_id,
            attempt,
            text,
            thinking,
            append,
        } => match &mut state.partial {
            Some(partial) if append => {
                if partial.run_id == run_id && partial.attempt == attempt {
                    partial.text.push_str(&text);
                    partial.thinking.push_str(&thinking);
                }
            }
            // An append without the record it extends has nothing to
            // extend: a compaction dropped that reply's records.
            None if append => {}
            partial => {
                *partial = Some(PartialReply {
                    run_id,
                    attempt,
                    text,
                    thinking,
                })
            }
        },
        SessionRecord::ToolOutput {
            run_id,
            call_id,
            output,
            truncated,
            append,
        } => match state.tool_outputs.get_mut(&call_id) {
            Some(saved) if append => {
                if saved.run_id == run_id {
                    saved.output.push_str(&output);
                    saved.truncated |= truncated;
                }
            }
            None if append => {}
            _ => {
                state.tool_outputs.insert(
                    call_id,
                    SavedToolOutput {
                        run_id,
                        output,
                        truncated,
                    },
                );
            }
        },
    }
}
