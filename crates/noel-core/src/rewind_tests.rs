//! Rewind tests (spec §9.2): a journal with `rewound` records loads the
//! rewound state; snapshots restore files a dropped turn changed.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{
    run_user_turn, ConversationMessage, MessageRole, ReloadableSystems, Runtime, SessionStore,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("noel-rewind-{}-{}", name, crate::new_session_id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn echo_provider() -> Box<dyn noel_ai::Provider> {
    noel_ai::create_provider(noel_ai::ProviderConfig {
        model: noel_ai::ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: noel_ai::ProviderApi::Faux,
        base_url: None,
        auth: noel_ai::ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    })
}

fn runtime_in(dir: &Path) -> Runtime {
    let (store, state) = SessionStore::create(&dir.join("sessions"), dir.to_path_buf()).unwrap();
    Runtime::with_store(state, ReloadableSystems::default(), store)
}

#[tokio::test]
async fn a_rewind_drops_messages_and_keeps_runs() {
    let dir = temp_dir("drops");
    let mut runtime = runtime_in(&dir);
    let path = dir
        .join("sessions")
        .join(format!("{}.jsonl", runtime.session().session_id));
    let provider = echo_provider();
    run_user_turn(&mut runtime, provider.as_ref(), "one".into())
        .await
        .unwrap();
    run_user_turn(&mut runtime, provider.as_ref(), "two".into())
        .await
        .unwrap();
    assert!(runtime.session().messages.len() >= 4);

    // Rewind to before the second prompt.
    let at_second = runtime
        .session()
        .messages
        .iter()
        .position(|message| message.role == MessageRole::User && message.content.contains("two"))
        .unwrap();
    let (restored, refused) = runtime.rewind(at_second).await.unwrap();
    assert!(restored.is_empty());
    assert!(refused.is_empty());
    assert!(runtime.session().messages.len() <= at_second);
    assert!(runtime
        .session()
        .messages
        .iter()
        .all(|message| !message.content.contains("two")));

    // The journal reloads the rewound state.
    let (_, state) = SessionStore::open(path).unwrap();
    assert!(state
        .messages
        .iter()
        .all(|message| !message.content.contains("two")));
    assert_eq!(state, *runtime.session());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_snapshot_restores_an_edited_file() {
    let dir = temp_dir("snapshot");
    let mut runtime = runtime_in(&dir);
    let file = dir.join("poem.txt");
    fs::write(&file, "original line\n").unwrap();

    // The model edits the file through the tool loop: a snapshot is taken
    // before the edit.
    let env = noel_tools::local_env(&dir, &noel_tools::ToolRuntimeOptions::default());
    runtime
        .snapshot_before_edit("poem.txt", &env)
        .await
        .unwrap();
    fs::write(&file, "changed line\n").unwrap();

    let messages = runtime.session().messages.len();
    let (restored, refused) = runtime.rewind(messages).await.unwrap();
    assert_eq!(restored, ["poem.txt".to_string()]);
    assert!(refused.is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "original line\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_created_file_is_deleted_back() {
    let dir = temp_dir("created");
    let mut runtime = runtime_in(&dir);
    let env = noel_tools::local_env(&dir, &noel_tools::ToolRuntimeOptions::default());
    runtime
        .snapshot_before_edit("made-up.txt", &env)
        .await
        .unwrap();
    assert_eq!(runtime.session().snapshots.len(), 1);
    assert!(runtime.session().snapshots[0].created);
    fs::write(dir.join("made-up.txt"), "new file").unwrap();

    let messages = runtime.session().messages.len();
    let (restored, _) = runtime.rewind(messages).await.unwrap();
    assert_eq!(restored, ["made-up.txt".to_string()]);
    assert!(!dir.join("made-up.txt").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_file_changed_outside_the_session_is_not_restored() {
    let dir = temp_dir("outside");
    let mut runtime = runtime_in(&dir);
    let file = dir.join("poem.txt");
    fs::write(&file, "original\n").unwrap();
    // Two edits of the one file, each with its snapshot and what it left.
    turn_editing(&mut runtime, &dir, "one", "poem.txt", "changed\n").await;
    turn_editing(&mut runtime, &dir, "two", "poem.txt", "changed more\n").await;

    // An edit outside the session after that: the file no longer holds
    // what the last edit left, so the rewind leaves it and names it.
    fs::write(&file, "outside change\n").unwrap();
    let (restored, refused) = runtime.rewind(0).await.unwrap();
    assert!(restored.is_empty());
    assert_eq!(refused, ["poem.txt".to_string()]);
    assert_eq!(fs::read_to_string(&file).unwrap(), "outside change\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_file_edited_more_than_once_goes_back_to_before_the_first_edit() {
    let dir = temp_dir("twice");
    let mut runtime = runtime_in(&dir);
    let file = dir.join("poem.txt");
    fs::write(&file, "original\n").unwrap();
    turn_editing(&mut runtime, &dir, "one", "poem.txt", "changed\n").await;
    turn_editing(&mut runtime, &dir, "two", "poem.txt", "changed more\n").await;
    turn_editing(&mut runtime, &dir, "three", "poem.txt", "changed again\n").await;

    let (restored, refused) = runtime.rewind(0).await.unwrap();
    assert_eq!(restored, ["poem.txt".to_string()]);
    assert!(refused.is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "original\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_file_that_cannot_be_read_is_not_snapshotted_as_new() {
    let dir = temp_dir("unreadable");
    let mut runtime = runtime_in(&dir);
    // A directory where the edit names a file: it is there, and reading
    // it fails. A snapshot saying "created" would have a rewind remove it.
    fs::create_dir(dir.join("thing")).unwrap();
    let env = noel_tools::local_env(&dir, &noel_tools::ToolRuntimeOptions::default());
    runtime.snapshot_before_edit("thing", &env).await.unwrap();
    assert!(runtime.session().snapshots.is_empty());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn snapshots_reload_from_the_journal() {
    let dir = temp_dir("reload");
    let mut runtime = runtime_in(&dir);
    let path = dir
        .join("sessions")
        .join(format!("{}.jsonl", runtime.session().session_id));
    let env = noel_tools::local_env(&dir, &noel_tools::ToolRuntimeOptions::default());
    runtime
        .snapshot_before_edit("poem.txt", &env)
        .await
        .unwrap();
    fs::write(dir.join("poem.txt"), "changed").unwrap();

    let (_, state) = SessionStore::open(path).unwrap();
    assert_eq!(state.snapshots.len(), 1);
    assert_eq!(state.snapshots[0].path, "poem.txt");
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn rewound_sessions_keep_the_journal_append_only() {
    let dir = temp_dir("append-only");
    let mut runtime = runtime_in(&dir);
    let path = dir
        .join("sessions")
        .join(format!("{}.jsonl", runtime.session().session_id));
    let provider = echo_provider();
    run_user_turn(&mut runtime, provider.as_ref(), "one".into())
        .await
        .unwrap();
    runtime.rewind(0).await.unwrap();
    // The journal still holds the original records plus the rewind.
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("\"one\""));
    assert!(content.contains("rewound"));
    fs::remove_dir_all(&dir).unwrap();
}

fn message(role: MessageRole, content: &str) -> ConversationMessage {
    ConversationMessage {
        role,
        content: content.to_string(),
        thinking: String::new(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    }
}

/// A turn whose reply changes `file` to `content`: the prompt, the
/// snapshot the edit takes, the edit, the reply. Returns the prompt's
/// index, which is the checkpoint a rewind to it keeps.
async fn turn_editing(
    runtime: &mut Runtime,
    dir: &Path,
    prompt: &str,
    file: &str,
    content: &str,
) -> usize {
    let checkpoint = runtime.session().messages.len();
    runtime
        .push_message(message(MessageRole::User, prompt))
        .unwrap();
    let env = noel_tools::local_env(dir, &noel_tools::ToolRuntimeOptions::default());
    runtime.snapshot_before_edit(file, &env).await.unwrap();
    fs::write(dir.join(file), content).unwrap();
    runtime.settle_snapshot(file, &env).await.unwrap();
    runtime
        .push_message(message(MessageRole::Assistant, "done"))
        .unwrap();
    checkpoint
}

fn reloaded(runtime: &Runtime, dir: &Path) -> crate::SessionState {
    let path = dir
        .join("sessions")
        .join(format!("{}.jsonl", runtime.session().session_id));
    SessionStore::open(path).unwrap().1
}

#[tokio::test]
async fn a_rewind_spends_the_snapshots_it_restored() {
    let dir = temp_dir("spent");
    let mut runtime = runtime_in(&dir);
    fs::write(dir.join("a.txt"), "zero").unwrap();
    let one = turn_editing(&mut runtime, &dir, "one", "a.txt", "one").await;
    let two = turn_editing(&mut runtime, &dir, "two", "a.txt", "two").await;
    turn_editing(&mut runtime, &dir, "three", "made.txt", "new").await;

    let (restored, refused) = runtime.rewind(two).await.unwrap();
    assert_eq!(restored, ["a.txt", "made.txt"]);
    assert!(refused.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one");
    assert!(!dir.join("made.txt").exists());
    // What is left is the first turn's snapshot: the dropped turns' are
    // not offered, or tried, again.
    assert_eq!(runtime.session().snapshots.len(), 1);
    assert_eq!(
        reloaded(&runtime, &dir).snapshots,
        runtime.session().snapshots
    );

    // The same file edited again and rewound to the first prompt goes back
    // to what it held before the session, with nothing left over to refuse.
    turn_editing(&mut runtime, &dir, "two again", "a.txt", "two again").await;
    let (restored, refused) = runtime.rewind(one).await.unwrap();
    assert_eq!(restored, ["a.txt"]);
    assert!(refused.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "zero");
    assert!(runtime.session().snapshots.is_empty());
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_rewind_keeping_files_leaves_them_for_an_earlier_checkpoint() {
    let dir = temp_dir("keeping");
    let mut runtime = runtime_in(&dir);
    fs::write(dir.join("a.txt"), "zero").unwrap();
    let one = turn_editing(&mut runtime, &dir, "one", "a.txt", "one").await;
    let two = turn_editing(&mut runtime, &dir, "two", "a.txt", "two").await;

    runtime.rewind_keeping_files(two).unwrap();
    assert_eq!(runtime.session().messages.len(), two);
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "two");
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());

    // A prompt after it that changes nothing: rewinding to it restores
    // nothing, since the kept change predates it.
    let later = runtime.session().messages.len();
    runtime
        .push_message(message(MessageRole::User, "later"))
        .unwrap();
    runtime
        .push_message(message(MessageRole::Assistant, "reply"))
        .unwrap();
    let (restored, refused) = runtime.rewind(later).await.unwrap();
    assert!(restored.is_empty() && refused.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "two");

    // Rewinding to the first prompt takes back both turns' changes.
    let (restored, refused) = runtime.rewind(one).await.unwrap();
    assert_eq!(restored, ["a.txt"]);
    assert!(refused.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "zero");
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());

    // With no turn kept there is no earlier checkpoint to hold them.
    turn_editing(&mut runtime, &dir, "again", "a.txt", "again").await;
    runtime.rewind_keeping_files(0).unwrap();
    assert!(runtime.session().snapshots.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "again");
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn an_undo_is_a_rewind_to_the_last_prompt() {
    let dir = temp_dir("undo");
    let mut runtime = runtime_in(&dir);
    fs::write(dir.join("a.txt"), "zero").unwrap();
    turn_editing(&mut runtime, &dir, "one", "a.txt", "one").await;
    let two = turn_editing(&mut runtime, &dir, "two", "a.txt", "two").await;

    let undone = runtime.undo_last_turn().await.unwrap().unwrap();
    assert_eq!(undone.prompt.content, "two");
    assert_eq!(undone.restored, ["a.txt"]);
    assert!(undone.refused.is_empty());
    assert_eq!(runtime.session().messages.len(), two);
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one");
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_compaction_forgets_the_snapshots_before_it() {
    let dir = temp_dir("compacted");
    let mut runtime = runtime_in(&dir);
    fs::write(dir.join("a.txt"), "zero").unwrap();
    turn_editing(&mut runtime, &dir, "one", "a.txt", "one").await;
    // The list is replaced by one that does not start it, as a compaction
    // does: message counts no longer place the snapshot.
    runtime
        .replace_messages(vec![
            message(MessageRole::Assistant, "summary"),
            message(MessageRole::User, "kept"),
        ])
        .unwrap();
    assert!(runtime.session().snapshots.is_empty());
    assert_eq!(reloaded(&runtime, &dir), *runtime.session());
    let (restored, refused) = runtime.rewind(0).await.unwrap();
    assert!(restored.is_empty() && refused.is_empty());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one");
    fs::remove_dir_all(&dir).unwrap();
}
