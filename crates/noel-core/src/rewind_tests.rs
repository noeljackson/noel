//! Rewind tests (spec §9.2): a journal with `rewound` records loads the
//! rewound state; snapshots restore files a dropped turn changed.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{run_user_turn, MessageRole, ReloadableSystems, Runtime, SessionStore};

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
async fn a_rewind_drops_messages_runs_and_queue() {
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
    let (restored, refused) = runtime.rewind(at_second).unwrap();
    assert!(restored.is_empty());
    assert!(refused.is_empty());
    assert!(runtime.session().messages.len() <= at_second);
    assert!(runtime
        .session()
        .messages
        .iter()
        .all(|message| !message.content.contains("two")));
    // Runs that started in the dropped range are gone.
    assert!(runtime
        .session()
        .runs
        .iter()
        .all(|run| run.start_message_index < at_second));

    // The journal reloads the rewound state.
    let (_, state) = SessionStore::open(path).unwrap();
    assert!(state
        .messages
        .iter()
        .all(|message| !message.content.contains("two")));
    assert_eq!(state.rewinds, [at_second]);
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
    let (restored, refused) = runtime.rewind(messages).unwrap();
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
    let (restored, _) = runtime.rewind(messages).unwrap();
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
    let env = noel_tools::local_env(&dir, &noel_tools::ToolRuntimeOptions::default());
    // First turn's edit: snapshot, then change.
    runtime
        .snapshot_before_edit("poem.txt", &env)
        .await
        .unwrap();
    fs::write(&file, "changed\n").unwrap();
    // A second edit snapshots again: restoring the first is safe only
    // while the file still holds what the second edit saw.
    let messages = runtime.session().messages.len();
    runtime
        .snapshot_before_edit("poem.txt", &env)
        .await
        .unwrap();
    fs::write(&file, "changed more\n").unwrap();

    // An outside edit after that: the rewind to the second edit refuses.
    fs::write(&file, "outside change\n").unwrap();
    let (_, refused) = runtime.rewind(messages).unwrap();
    assert_eq!(refused, ["poem.txt".to_string()]);
    assert_eq!(fs::read_to_string(&file).unwrap(), "outside change\n");
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
    runtime.rewind(0).unwrap();
    // The journal still holds the original records plus the rewind.
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("\"one\""));
    assert!(content.contains("rewound"));
    fs::remove_dir_all(&dir).unwrap();
}
