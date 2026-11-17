//! `/undo` and `/rewind` (spec §9.2): the checkpoint picker, the choice of
//! what to restore, and the rewind, which hands the prompt it stopped
//! before back to the draft. `/undo` is the rewind to the last prompt.

use super::*;
use crate::session_commands::{CommandOutcome, CommandUi};

/// How many changed files a row names before it counts the rest.
const FILES_NAMED: usize = 3;

/// The files the turns from message `messages` on changed: the paths a
/// rewind to it would write back, in the order they were first changed.
fn changed_files(runtime: &Runtime, messages: usize) -> Vec<&str> {
    let mut files: Vec<&str> = Vec::new();
    for snapshot in &runtime.session().snapshots {
        if snapshot.message_index >= messages && !files.contains(&snapshot.path.as_str()) {
            files.push(&snapshot.path);
        }
    }
    files
}

/// `2 files: a.rs, b.rs`, the first few named.
fn files_label(files: &[&str]) -> String {
    let named = files
        .iter()
        .take(FILES_NAMED)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let more = match files.len().saturating_sub(FILES_NAMED) {
        0 => String::new(),
        more => format!(" +{more} more"),
    };
    format!(
        "{} file{}: {named}{more}",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    )
}

/// A prompt's first line that says something, shortened for a row.
fn prompt_summary(message: &noel_core::ConversationMessage) -> String {
    message
        .content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(60)
        .collect()
}

/// The checkpoints: where the session is now, then one row per user
/// prompt, newest first, each with the files the turns from it on changed.
/// A row's value is `m<messages kept>`: a bare number would be read as a
/// position in the list.
pub(crate) fn rewind_selector(runtime: &Runtime) -> Selector {
    let session = runtime.session();
    let mut items = vec![SelectorItem {
        label: "(current)".to_string(),
        value: format!("m{}", session.messages.len()),
        active: true,
    }];
    let prompts = session
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == noel_core::MessageRole::User);
    for (index, message) in prompts.rev() {
        let files = changed_files(runtime, index);
        let changes = if files.is_empty() {
            "no file changes".to_string()
        } else {
            files_label(&files)
        };
        items.push(SelectorItem {
            label: format!("{} · {changes}", prompt_summary(message)),
            value: format!("m{index}"),
            active: false,
        });
    }
    Selector::new("rewind", items)
}

/// `/rewind`, and Esc-Esc at an empty draft: the checkpoints.
pub(crate) fn open(ui: &mut impl CommandUi, runtime: &Runtime) -> Result<CommandOutcome> {
    let selector = rewind_selector(runtime);
    if selector.items.len() == 1 {
        ui.push(TuiEntryKind::System, "nothing to rewind to");
        return Ok(CommandOutcome::Handled);
    }
    Ok(opened(ui, selector))
}

fn opened(ui: &mut impl CommandUi, selector: Selector) -> CommandOutcome {
    // Every pick comes back as `/select rewind <value>`.
    let state = TuiSelectorState::new("rewind", selector, "", None);
    ui.push(TuiEntryKind::System, format!("{} selector", state.title));
    CommandOutcome::Selector(Box::new(state))
}

/// A pick. `m<messages>` is a checkpoint: when the turns after it changed
/// files it is answered with the choice of what to restore, whose picks
/// are `m<messages>:all` (the conversation and the files) and
/// `m<messages>:chat` (the conversation; the files stay as they are).
pub(crate) async fn pick(
    ui: &mut impl CommandUi,
    runtime: &mut Runtime,
    value: &str,
) -> Result<CommandOutcome> {
    let (checkpoint, scope) = match value.split_once(':') {
        Some((checkpoint, scope)) => (checkpoint, Some(scope)),
        None => (value, None),
    };
    let messages: usize = checkpoint
        .strip_prefix('m')
        .and_then(|count| count.parse().ok())
        .ok_or_else(|| anyhow!("rewind: not a checkpoint: {value}"))?;
    if messages >= runtime.session().messages.len() {
        ui.push(TuiEntryKind::System, "rewind: already at this point");
        return Ok(CommandOutcome::Handled);
    }
    let files = changed_files(runtime, messages);
    let keep_files = match scope {
        Some("all") => false,
        Some("chat") => true,
        Some(other) => return Err(anyhow!("rewind: not a choice: {other}")),
        None if files.is_empty() => false,
        None => {
            let choice = Selector::new(
                "restore",
                vec![
                    SelectorItem {
                        label: format!("conversation and files · {}", files_label(&files)),
                        value: format!("m{messages}:all"),
                        active: true,
                    },
                    SelectorItem {
                        label: "conversation only · the files stay as they are".to_string(),
                        value: format!("m{messages}:chat"),
                        active: false,
                    },
                ],
            );
            return Ok(opened(ui, choice));
        }
    };
    rewind(ui, runtime, messages, keep_files, "rewound").await
}

/// `/undo`: the rewind to the last prompt, files included.
pub(crate) async fn undo(ui: &mut impl CommandUi, runtime: &mut Runtime) -> Result<CommandOutcome> {
    let last_prompt = runtime
        .session()
        .messages
        .iter()
        .rposition(|message| message.role == noel_core::MessageRole::User);
    let Some(messages) = last_prompt else {
        ui.push(TuiEntryKind::System, "nothing to undo");
        return Ok(CommandOutcome::Handled);
    };
    rewind(ui, runtime, messages, false, "undid the last turn").await
}

/// Rewinds to the first `messages` messages and says what it did after
/// `done`. The prompt it stopped before goes back to the draft.
async fn rewind(
    ui: &mut impl CommandUi,
    runtime: &mut Runtime,
    messages: usize,
    keep_files: bool,
    done: &str,
) -> Result<CommandOutcome> {
    let prompt = runtime
        .session()
        .messages
        .get(messages)
        .filter(|message| message.role == noel_core::MessageRole::User)
        .cloned();
    let files = if keep_files {
        let kept = !changed_files(runtime, messages).is_empty();
        runtime.rewind_keeping_files(messages)?;
        if kept {
            "; the files stay as they are".to_string()
        } else {
            String::new()
        }
    } else {
        let (restored, refused) = runtime.rewind(messages).await?;
        restored_summary(&restored, &refused)
    };
    ui.push(TuiEntryKind::System, format!("{done}{files}"));
    Ok(match prompt {
        Some(prompt) => CommandOutcome::Draft {
            text: prompt.content,
            media: prompt.media,
        },
        None => CommandOutcome::Handled,
    })
}

/// What a rewind did to the files, after what it did to the conversation.
fn restored_summary(restored: &[String], refused: &[String]) -> String {
    let mut summary = String::new();
    if !restored.is_empty() {
        summary.push_str(&format!(
            "; {} file{} restored",
            restored.len(),
            if restored.len() == 1 { "" } else { "s" }
        ));
    }
    if !refused.is_empty() {
        summary.push_str(&format!(
            "; {} left, changed outside the session: {}",
            refused.len(),
            refused.join(", ")
        ));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_names_the_first_files_and_counts_the_rest() {
        assert_eq!(files_label(&["a.rs"]), "1 file: a.rs");
        assert_eq!(files_label(&["a.rs", "b.rs"]), "2 files: a.rs, b.rs");
        assert_eq!(
            files_label(&["a", "b", "c", "d", "e"]),
            "5 files: a, b, c +2 more"
        );
    }

    #[test]
    fn the_summary_says_what_was_restored_and_what_was_left() {
        let paths = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(restored_summary(&[], &[]), "");
        assert_eq!(restored_summary(&paths(&["a"]), &[]), "; 1 file restored");
        assert_eq!(
            restored_summary(&paths(&["a", "b"]), &paths(&["c"])),
            "; 2 files restored; 1 left, changed outside the session: c"
        );
    }
}
