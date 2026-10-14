//! A session's workspace (spec §10): where its tools run when that is not
//! this machine. The journal saves it as one `workspace` record, as Durable's
//! container example keeps a conversation's workspace with the conversation;
//! a session without one runs its tools locally, as every session did before.
//!
//! noel-core only keeps the record and asks a hook to open it
//! ([`crate::EnvProvider::Workspaces`]); the adapters (`ssh`) live with the
//! command line, which knows how to reach and trust a machine.
//!
//! The project context files a remote workspace's system prompt lists are
//! read from the workspace, through its [`ExecutionEnv`], as the local ones
//! are read from the session's directory: one file per directory, from the
//! outermost ancestor down to the working directory.

use std::path::PathBuf;

use noel_config::{ContextFile, CONTEXT_FILE_NAMES};
use noel_env::{Context, ExecutionEnv, FileErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Where a session's tools run, as its journal saves it:
/// `{"type":"workspace","adapter":…,"id":…,"cwd":…, …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkspace {
    /// How the workspace is reached, e.g. `ssh`.
    pub adapter: String,
    /// Its file namespace (`FileSystem::id`): workspaces with equal ids see
    /// the same files at the same paths, so `edit` and `write` of one file
    /// wait for each other across sessions.
    pub id: String,
    /// The working directory there, an absolute path on that machine.
    pub cwd: String,
    /// The adapter's own fields, e.g. `host`, `user` and `port`; kept as
    /// saved, so a rewrite of the journal keeps fields a newer version added.
    #[serde(flatten)]
    pub options: Map<String, Value>,
}

impl SessionWorkspace {
    /// An option's text, if it is a string.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.options.get(key).and_then(Value::as_str)
    }
}

/// The project context files in `env`, as `noel_config` loads them from a
/// local directory: in each directory from the outermost ancestor of the
/// working directory down to it, the first of [`CONTEXT_FILE_NAMES`] that is
/// a readable file. A file that cannot be read is passed over, as locally;
/// any other failure (the workspace cannot be reached) fails the whole load,
/// so a later run tries again.
pub(crate) async fn workspace_context_files(
    env: &dyn ExecutionEnv,
    cx: &Context,
) -> Result<Vec<ContextFile>, String> {
    let mut directories = Vec::new();
    let mut current = Some(PathBuf::from(env.cwd()));
    while let Some(directory) = current {
        current = directory.parent().map(PathBuf::from);
        directories.push(directory);
    }
    let mut files = Vec::new();
    for directory in directories.into_iter().rev() {
        for name in CONTEXT_FILE_NAMES {
            let path = directory.join(name);
            let text = path.to_string_lossy();
            match env.read_text_file(&text, cx).await {
                Ok(content) => {
                    files.push(ContextFile { path, content });
                    break;
                }
                Err(error)
                    if matches!(
                        error.code,
                        FileErrorCode::NotFound
                            | FileErrorCode::IsDirectory
                            | FileErrorCode::NotDirectory
                            | FileErrorCode::PermissionDenied
                            | FileErrorCode::Invalid
                    ) => {}
                Err(error) => return Err(error.message),
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_keeps_its_fields_flat_and_adapter_options_as_saved() {
        let line = r#"{"adapter":"ssh","id":"ssh:box","cwd":"/work","host":"box","port":2222,"future":{"a":1}}"#;
        let workspace: SessionWorkspace = serde_json::from_str(line).unwrap();
        assert_eq!(
            (workspace.adapter.as_str(), workspace.id.as_str()),
            ("ssh", "ssh:box")
        );
        assert_eq!(workspace.text("host"), Some("box"));
        assert_eq!(workspace.options["port"], 2222);
        assert_eq!(
            serde_json::to_value(&workspace).unwrap(),
            serde_json::from_str::<Value>(line).unwrap()
        );
    }

    fn record() -> SessionWorkspace {
        serde_json::from_str(
            r#"{"adapter":"ssh","id":"ssh://box","cwd":"/work","host":"box","user":"me"}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_journal_keeps_the_workspace_through_reopening_forks_and_compaction() {
        use crate::{SessionState, SessionStore};

        let dir = std::env::temp_dir().join(format!("noel-ws-journal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (store, state) =
            SessionStore::create_in_workspace(&dir, "/here".into(), "s1", record()).unwrap();
        assert_eq!(state.workspace, Some(record()));
        drop(store);
        let path = dir.join("s1.jsonl");
        let journal = std::fs::read_to_string(&path).unwrap();
        let lines = journal.lines().collect::<Vec<_>>();
        // `started` and `workspace` first, in one write.
        assert_eq!(
            serde_json::from_str::<Value>(lines[0]).unwrap()["type"],
            "started"
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap(),
            serde_json::json!({
                "type": "workspace", "adapter": "ssh", "id": "ssh://box", "cwd": "/work",
                "host": "box", "user": "me",
            })
        );
        assert_eq!(SessionStore::read(&path).unwrap().workspace, Some(record()));
        let (store, reopened) = SessionStore::open(path.clone()).unwrap();
        assert_eq!(reopened.workspace, Some(record()));
        drop(store);
        let (_fork_store, fork) = SessionStore::fork(&dir, &reopened, false).unwrap();
        assert_eq!(fork.workspace, Some(record()));
        assert_eq!(
            SessionStore::read(&dir.join(format!("{}.jsonl", fork.session_id)))
                .unwrap()
                .workspace,
            Some(record())
        );
        // A compaction's rewrite drops partials, never the workspace.
        let rewritten = crate::progress::drop_superseded(
            format!("{journal}{{\"type\":\"partial\",\"attempt\":1,\"text\":\"x\"}}\n").as_bytes(),
            &Default::default(),
        );
        let rewritten = String::from_utf8(rewritten).unwrap();
        assert!(rewritten.contains(r#""type":"workspace""#), "{rewritten}");
        assert!(!rewritten.contains(r#""type":"partial""#), "{rewritten}");
        // A local session is made as before: one `started` record.
        let (_store, local) =
            SessionStore::create_with_workspace(&dir, "/here".into(), None).unwrap();
        let local_journal =
            std::fs::read_to_string(dir.join(format!("{}.jsonl", local.session_id))).unwrap();
        assert_eq!(local_journal.lines().count(), 1, "{local_journal}");
        assert_eq!(local.workspace, None::<SessionWorkspace>);
        // A session state without one serializes as before.
        let state = SessionState::new("s2", "/here".into());
        assert!(!serde_json::to_string(&state).unwrap().contains("workspace"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn context_files_load_from_the_outermost_directory_down() {
        let root = std::env::temp_dir().join(format!("noel-ws-context-{}", std::process::id()));
        let inner = root.join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("a/CLAUDE.md"), "outer").unwrap();
        std::fs::write(inner.join("AGENTS.md"), "inner").unwrap();
        std::fs::write(inner.join("CLAUDE.md"), "shadowed by AGENTS.md").unwrap();
        // A directory with a context file's name is passed over.
        std::fs::create_dir_all(root.join("AGENTS.md")).unwrap();
        let env = noel_env::LocalEnv::new(&inner);
        let files = workspace_context_files(&env, &Context::background())
            .await
            .unwrap();
        // Only those below the scratch root: its ancestors are the machine's.
        let found = files
            .iter()
            .filter_map(|file| {
                let path = file.path.strip_prefix(&root).ok()?;
                Some((path.display().to_string(), file.content.as_str()))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                ("a/CLAUDE.md".to_string(), "outer"),
                ("a/b/AGENTS.md".to_string(), "inner"),
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
