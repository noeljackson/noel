use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    Arc, Mutex,
};

use noel_ai::{create_provider, ProviderApi, ProviderAuth, ProviderConfig};

use super::*;

/// A provider for tool-call tests whose tool never needs one: the
/// `task` tool is the only tool that calls a provider.
pub(crate) struct UnusedProvider;
#[async_trait::async_trait]
impl noel_ai::Provider for UnusedProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        Err(ProviderError::Config("unused".to_string()))
    }
}
pub(crate) fn unused_provider() -> UnusedProvider {
    UnusedProvider
}

fn test_message(role: MessageRole, content: &str) -> ConversationMessage {
    ConversationMessage {
        role,
        content: content.into(),
        thinking: String::new(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    }
}

#[tokio::test]
async fn undo_persists_context_and_keeps_other_files_audit_queue_usage_and_identity() {
    let base = std::env::temp_dir().join(format!("noel-undo-test-{}", new_session_id()));
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("user.txt"), "unrelated edits").unwrap();
    let (store, mut session) = SessionStore::create(&base, base.clone()).unwrap();
    session
        .queue
        .push(crate::QueuedEntry::follow_up("follow up"));
    session.edited_files.push("user.txt".into());
    session.tool_history.push(ToolEvent {
        id: "call".into(),
        name: "write".into(),
        result: "ok".into(),
        is_error: false,
    });
    let mut runtime = Runtime::with_store(session, ReloadableSystems::default(), store.clone());
    for message in [
        test_message(MessageRole::User, "first"),
        test_message(MessageRole::Assistant, "answer"),
        test_message(MessageRole::User, "second"),
        test_message(MessageRole::Assistant, "call"),
        test_message(MessageRole::Tool, "result"),
        test_message(MessageRole::Assistant, "done"),
    ] {
        runtime.push_message(message).unwrap();
    }
    runtime.record_usage(12, 5).unwrap();
    let before = runtime.session().clone();
    let undone = runtime.undo_last_turn().await.unwrap().unwrap();
    assert_eq!(undone.prompt.content, "second");
    assert!(undone.restored.is_empty() && undone.refused.is_empty());
    let mut expected = before;
    expected.messages.truncate(2);
    assert_eq!(runtime.session(), &expected);
    assert_eq!(
        fs::read_to_string(base.join("user.txt")).unwrap(),
        "unrelated edits"
    );
    let loaded = store.load().unwrap();
    assert_eq!(loaded.messages, expected.messages);
    assert_eq!(loaded.usage, expected.usage);
    assert!(runtime.undo_last_turn().await.unwrap().is_some());
    assert!(runtime.undo_last_turn().await.unwrap().is_none());
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn compaction_retains_complete_turns_and_handles_empty_sessions() {
    let mut runtime = Runtime::new(
        SessionState::new("compact", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    runtime.systems.compaction.keep_recent_tokens = 0;
    assert_eq!(
        runtime
            .compact_messages(&CheckpointProvider, CompactionKind::Manual, "")
            .await
            .unwrap()
            .omitted_messages,
        0
    );
    for role in [
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::User,
        MessageRole::Assistant,
        MessageRole::Tool,
        MessageRole::Assistant,
        MessageRole::Tool,
        MessageRole::Assistant,
    ] {
        runtime
            .push_message(test_message(role, &"keep ".repeat(30)))
            .unwrap();
    }
    let record = runtime
        .compact_messages(&CheckpointProvider, CompactionKind::Manual, "")
        .await
        .unwrap();
    assert_eq!(record.omitted_messages, 2);
    assert_eq!(record.retained_messages, 6);
    assert_eq!(runtime.session.messages[1].role, MessageRole::User);
}

#[tokio::test]
async fn usage_counts_last_snapshot_once_and_survives_reload_export_and_compaction() {
    struct UsageProvider;
    #[async_trait::async_trait]
    impl Provider for UsageProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![
                StreamEvent::Usage {
                    input_tokens: 12,
                    output_tokens: 1,
                },
                StreamEvent::Text("ok".into()),
                StreamEvent::Usage {
                    input_tokens: 12,
                    output_tokens: 5,
                },
            ])
        }
    }
    let base = std::env::temp_dir().join(format!("noel-usage-test-{}", new_session_id()));
    let (store, session) = SessionStore::create(&base, base.clone()).unwrap();
    let mut runtime = Runtime::with_store(session, ReloadableSystems::default(), store.clone());
    for _ in 0..3 {
        run_user_turn(&mut runtime, &UsageProvider, "question".into())
            .await
            .unwrap();
    }
    let expected = SessionUsage {
        input_tokens: 36,
        output_tokens: 15,
        requests: 3,
    };
    assert_eq!(runtime.session.usage, expected);
    runtime
        .compact_messages(&UsageProvider, CompactionKind::Manual, "")
        .await
        .unwrap();
    runtime.reload(ReloadableSystems::default()).unwrap();
    assert_eq!(store.load().unwrap().usage, expected);
    for extension in ["json", "jsonl"] {
        let path = base.join(format!("export.{extension}"));
        write_session_export(runtime.session(), &path).unwrap();
        let (_, imported) = SessionStore::import_path(&base.join("imports"), &path).unwrap();
        assert_eq!(imported.usage, expected);
    }
    let mut export = serde_json::to_value(SessionExport::from(runtime.session())).unwrap();
    export.as_object_mut().unwrap().remove("usage");
    assert_eq!(
        serde_json::from_value::<SessionExport>(export)
            .unwrap()
            .usage,
        SessionUsage::default()
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn thinking_is_optional_in_old_sessions_and_separate_in_block_imports() {
    let old = json!({"role": "assistant", "content": "answer"});
    let message: ConversationMessage = serde_json::from_value(old).unwrap();
    assert!(message.thinking.is_empty());
    let imported = ts_jsonl_conversation_message(&json!({
        "role": "assistant", "content": [
            {"type": "thinking", "thinking": "summary"},
            {"type": "text", "text": "answer"}
        ]
    }))
    .unwrap();
    assert_eq!(imported.thinking, "summary");
    assert_eq!(imported.content, "answer");
}

#[test]
fn reload_preserves_session_context() {
    let cwd = PathBuf::from("/repo");
    let model = ModelRef {
        provider: "openai".to_string(),
        id: "gpt-test".to_string(),
    };
    let mut session = SessionState::new("session-1", cwd.clone());
    session.messages.push(ConversationMessage {
        thinking: String::new(),
        role: MessageRole::User,
        content: "keep this".to_string(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    });
    session.tool_history.push(ToolEvent {
        id: "tool-1".to_string(),
        name: "read".to_string(),
        result: "ok".to_string(),
        is_error: false,
    });
    session.queue.push(crate::QueuedEntry::follow_up("next"));
    session.active_model = Some(model.clone());
    session.active_tool_names = BTreeSet::from(["read".to_string()]);

    let mut runtime = Runtime::new(session, ReloadableSystems::default());
    let next = ReloadableSystems {
        config_generation: 1,
        system_prompt: Some("new prompt".to_string()),
        available_models: vec![model],
        available_tool_names: BTreeSet::from(["read".to_string(), "bash".to_string()]),
        keybinding_generation: 1,
        ..ReloadableSystems::default()
    };

    let report = runtime.reload(next).expect("reload should succeed");

    assert!(report.active_model_valid);
    assert!(report.removed_active_tools.is_empty());
    assert_eq!(runtime.session().session_id, "session-1");
    assert_eq!(runtime.session().cwd, cwd);
    assert_eq!(runtime.session().messages.len(), 1);
    assert_eq!(runtime.session().tool_history.len(), 1);
    assert_eq!(runtime.session().queued_messages(), ["next"]);
}

#[test]
fn invalid_reload_keeps_existing_systems() {
    let session = SessionState::new("session-1", PathBuf::from("/repo"));
    let mut runtime = Runtime::new(
        session,
        ReloadableSystems {
            config_generation: 2,
            ..ReloadableSystems::default()
        },
    );

    let result = runtime.reload(ReloadableSystems {
        config_generation: 1,
        ..ReloadableSystems::default()
    });

    assert_eq!(
        result,
        Err(ReloadError::Invalid(
            "config generation cannot move backwards".to_string()
        ))
    );
    assert_eq!(runtime.systems().config_generation, 2);
}

#[test]
fn reload_reports_invalid_active_model_without_clearing_it() {
    let mut session = SessionState::new("session-1", PathBuf::from("/repo"));
    session.active_model = Some(ModelRef {
        provider: "openai".to_string(),
        id: "removed".to_string(),
    });

    let mut runtime = Runtime::new(session, ReloadableSystems::default());
    let report = runtime
        .reload(ReloadableSystems {
            config_generation: 1,
            available_models: vec![ModelRef {
                provider: "anthropic".to_string(),
                id: "claude-test".to_string(),
            }],
            ..ReloadableSystems::default()
        })
        .expect("reload should keep the session and report invalid model");

    assert!(!report.active_model_valid);
    assert_eq!(
        runtime.session().active_model,
        Some(ModelRef {
            provider: "openai".to_string(),
            id: "removed".to_string(),
        })
    );
}

#[test]
fn reload_reports_invalid_active_account_without_clearing_it() {
    let mut session = SessionState::new("session-1", PathBuf::from("/repo"));
    session.active_model = Some(ModelRef {
        provider: "openai".to_string(),
        id: "gpt-test".to_string(),
    });
    session.active_account = Some("work".to_string());

    let mut runtime = Runtime::new(session, ReloadableSystems::default());
    let report = runtime
        .reload(ReloadableSystems {
            config_generation: 1,
            available_models: vec![ModelRef {
                provider: "openai".to_string(),
                id: "gpt-test".to_string(),
            }],
            available_accounts: BTreeMap::from([(
                "openai".to_string(),
                vec!["default".to_string()],
            )]),
            ..ReloadableSystems::default()
        })
        .expect("reload should keep the session and report invalid account");

    assert!(report.active_model_valid);
    assert!(!report.active_account_valid);
    assert_eq!(runtime.session().active_account, Some("work".to_string()));
}

#[test]
fn session_store_round_trips_active_account() {
    let base = std::env::temp_dir().join(format!("noel-session-test-{}", new_session_id()));
    let (store, _state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    store
        .record_active_account(Some("work".to_string()))
        .expect("record account");

    let path = store.path().to_path_buf();
    let (_store, loaded) = SessionStore::open(path.clone()).expect("open session");
    assert_eq!(loaded.active_account, Some("work".to_string()));

    let mut runtime = Runtime::new(loaded, ReloadableSystems::default());
    runtime.set_store(store);
    runtime
        .set_active_account(Some("default".to_string()))
        .expect("set account");
    assert_eq!(
        runtime.session().active_account,
        Some("default".to_string())
    );

    let (_store, reloaded) = SessionStore::open(path).expect("reopen journaled session");
    assert_eq!(reloaded.active_account, Some("default".to_string()));

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn session_store_round_trips_messages() {
    let base = std::env::temp_dir().join(format!("noel-session-test-{}", new_session_id()));
    let (store, mut state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    state.messages.push(ConversationMessage {
        thinking: String::new(),
        role: MessageRole::User,
        content: "hello".to_string(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    });
    state.active_thinking_level = Some("xhigh".to_string());
    store
        .record_message(state.messages[0].clone())
        .expect("record message");
    store
        .record_active_thinking_level(state.active_thinking_level.clone())
        .expect("record thinking");

    let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("open session");

    assert_eq!(loaded.cwd, PathBuf::from("/repo"));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content, "hello");
    assert_eq!(loaded.active_thinking_level, Some("xhigh".to_string()));

    let _ = fs::remove_dir_all(base);
}

#[test]
fn session_exports_jsonl_and_html_and_imports_jsonl() {
    let base = std::env::temp_dir().join(format!("noel-session-export-test-{}", new_session_id()));
    let export_dir = base.join("exports");
    fs::create_dir_all(&export_dir).expect("create export dir");
    let (store, mut state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    state.name = Some("exported".to_string());
    state.labels = BTreeSet::from(["important".to_string()]);
    state.parent_session_id = Some("parent-session".to_string());
    state.active_model = Some(ModelRef {
        provider: "anthropic".to_string(),
        id: "claude".to_string(),
    });
    state.active_thinking_level = Some("xhigh".to_string());
    state.disabled_tools = BTreeSet::from(["write".to_string()]);
    state.active_tool_names = active_tools_with(&state.disabled_tools);
    state.queue = vec!["next prompt".to_string()]
        .into_iter()
        .map(crate::QueuedEntry::follow_up)
        .collect();
    state.messages.push(ConversationMessage {
        thinking: String::new(),
        role: MessageRole::User,
        content: "hello <world>".to_string(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    });
    state.messages.push(ConversationMessage {
        thinking: "summary <private>".into(),
        role: MessageRole::Assistant,
        content: "done".to_string(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    });
    // An aborted reply keeps its mark through every export.
    state.messages.push(ConversationMessage {
        content: "cut off".to_string(),
        aborted: true,
        ..state.messages[1].clone()
    });
    state.tool_history.push(ToolEvent {
        id: "tool-1".to_string(),
        name: "read".to_string(),
        result: "content".to_string(),
        is_error: false,
    });
    state.compactions.push(CompactionRecord {
        kind: CompactionKind::Manual,
        omitted_messages: 1,
        retained_messages: 1,
        summary: "compacted".to_string(),
    });
    state.branch_summaries.push(BranchSummary {
        from_session_id: state.session_id.clone(),
        to_session_id: "branch-session".to_string(),
        summary: "branched".to_string(),
    });
    store.record_metadata(&state).expect("record metadata");
    store
        .record_active_model(state.active_model.clone())
        .expect("record model");
    store
        .record_active_thinking_level(state.active_thinking_level.clone())
        .expect("record thinking");
    store
        .record_disabled_tools(state.disabled_tools.iter().cloned().collect())
        .expect("record disabled tools");
    store
        .record_message(state.messages[0].clone())
        .expect("record user");
    store
        .record_message(state.messages[1].clone())
        .expect("record assistant");
    store
        .record_message(state.messages[2].clone())
        .expect("record aborted assistant");
    store
        .record_tool(state.tool_history[0].clone())
        .expect("record tool");
    store
        .record_queued_message(state.queued_messages()[0].clone())
        .expect("record queued");
    store
        .record_compaction(state.compactions[0].clone())
        .expect("record compaction");
    store
        .record_branch_summary(state.branch_summaries[0].clone())
        .expect("record branch summary");

    let json = export_dir.join("session.json");
    let jsonl = export_dir.join("session.jsonl");
    let html = export_dir.join("session.html");
    store.export_state(&state, &json).expect("export json");
    store.export_state(&state, &jsonl).expect("export jsonl");
    store.export_state(&state, &html).expect("export html");

    let json_content = fs::read_to_string(&json).expect("read json");
    let exported =
        serde_json::from_str::<serde_json::Value>(&json_content).expect("parse json export");
    assert_eq!(exported["messages"][1]["thinking"], "summary <private>");
    assert!(fs::read_to_string(&html)
        .unwrap()
        .contains("summary &lt;private&gt;"));
    assert_eq!(exported["name"], "exported");
    assert_eq!(exported["parent_session_id"], "parent-session");
    assert_eq!(exported["active_thinking_level"], "xhigh");
    assert_eq!(exported["active_model"]["provider"], "anthropic");
    assert_eq!(exported["queued_messages"][0], "next prompt");

    let jsonl_content = fs::read_to_string(&jsonl).expect("read jsonl");
    assert!(jsonl_content.contains("\"type\":\"message\""));
    let jsonl_records = jsonl_content
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse jsonl line"))
        .collect::<Vec<_>>();
    let jsonl_types = jsonl_records
        .iter()
        .map(|record| record["type"].as_str().expect("record type"))
        .collect::<Vec<_>>();
    assert_eq!(jsonl_types.first(), Some(&"session"));
    assert_eq!(jsonl_records[0]["version"], 3);
    assert_eq!(jsonl_records[0]["cwd"], "/repo");
    let timestamp = jsonl_records[0]["timestamp"]
        .as_str()
        .expect("session timestamp");
    assert_ne!(timestamp, "1970-01-01T00:00:00.000Z");
    assert!(timestamp.contains('T'));
    for record in jsonl_records.iter().skip(1) {
        assert_eq!(record["timestamp"], timestamp);
    }
    for expected_type in [
        "custom",
        "session_info",
        "model_change",
        "thinking_level_change",
        "message",
        "label",
        "compaction",
        "branch_summary",
    ] {
        assert!(
            jsonl_types.contains(&expected_type),
            "missing jsonl record type {expected_type}"
        );
    }
    assert_eq!(jsonl_records[1]["parentId"], serde_json::Value::Null);
    assert_eq!(jsonl_records[2]["parentId"], "e1");
    let html_content = fs::read_to_string(&html).expect("read html");
    assert!(html_content.contains("<!doctype html>"));
    assert!(html_content.contains("hello &lt;world&gt;"));
    assert!(html_content.contains("Assistant (aborted)"));
    assert!(html_content.contains("tool history"));
    assert_eq!(exported["messages"][2]["aborted"], true);
    let aborted = jsonl_records
        .iter()
        .filter(|record| record["message"]["stopReason"] == "aborted")
        .count();
    assert_eq!(aborted, 1);

    let import_dir = base.join("imported");
    let (_import_store, imported) =
        SessionStore::import_path(&import_dir, &jsonl).expect("import jsonl");
    assert_eq!(imported.session_id, state.session_id);
    assert_eq!(imported.messages, state.messages);
    assert_eq!(imported.tool_history, state.tool_history);
    assert_eq!(imported.active_thinking_level, state.active_thinking_level);
    assert_eq!(imported.labels, state.labels);

    let (_opened_store, opened) =
        SessionStore::open(jsonl.clone()).expect("open ts-style jsonl directly");
    assert_eq!(opened.session_id, state.session_id);
    assert_eq!(opened.messages, state.messages);
    assert_eq!(opened.active_model, state.active_model);
    assert_eq!(opened.disabled_tools, state.disabled_tools);
    assert_eq!(opened.active_tool_names, state.active_tool_names);
    assert!(!opened.active_tool_names.contains("write"));
    assert!(opened.active_tool_names.contains("read"));
    assert_eq!(opened.labels, state.labels);

    let _ = fs::remove_dir_all(base);
}

#[test]
fn upstream_session_export_fixture_documents_ts_session_surface() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/session-export.json"
    ))
    .expect("parse session export fixture");

    assert_eq!(fixture["header"]["type"], "session");
    assert_eq!(fixture["header"]["version"], 3);
    assert_eq!(fixture["treeRootCount"], 1);
    assert_eq!(fixture["sessionName"], "demo session");
    assert_eq!(fixture["labelForFirstMessage"], "important");
    let entry_types = fixture["entryTypes"]
        .as_array()
        .expect("entry types")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    for expected_type in [
        "message",
        "session_info",
        "model_change",
        "thinking_level_change",
        "custom",
        "compaction",
        "label",
    ] {
        assert!(
            entry_types.contains(expected_type),
            "missing upstream session entry type {expected_type}"
        );
    }
    assert_eq!(fixture["jsonlBranchExport"]["recordTypes"][0], "session");
    assert_eq!(fixture["jsonlBranchExport"]["firstRecordVersion"], 3);
    assert_eq!(
        fixture["jsonlBranchExport"]["parentChain"],
        serde_json::json!([null, "set", "set"])
    );

    let base = std::env::temp_dir().join(format!(
        "noel-ts-session-active-branch-test-{}",
        new_session_id()
    ));
    fs::create_dir_all(&base).expect("create branch fixture dir");
    let path = base.join("ts-branch.jsonl");
    let records = fixture["fullTreeJsonlExport"]["records"]
        .as_array()
        .expect("full tree records");
    let content = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("serialize record"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{content}\n")).expect("write branch fixture");

    let (_store, opened) = SessionStore::open(path).expect("open TS active branch");
    let messages = opened
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        fixture["fullTreeJsonlExport"]["activeMessageTexts"]
            .as_array()
            .expect("active texts")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
    );
    for text in fixture["fullTreeJsonlExport"]["abandonedMessageTexts"]
        .as_array()
        .expect("abandoned texts")
        .iter()
        .filter_map(serde_json::Value::as_str)
    {
        assert!(
            !messages.contains(&text),
            "abandoned TS branch message should not load: {text}"
        );
    }
    assert_eq!(opened.labels, BTreeSet::from(["important".to_string()]));

    let _ = fs::remove_dir_all(base);
}

#[test]
fn upstream_agent_tool_loop_fixture_documents_model_callable_tools() {
    let fixture = serde_json::from_str::<serde_json::Value>(include_str!(
        "../../../tests/fixtures/ts-parity/agent-tool-loop.json"
    ))
    .expect("parse agent tool loop fixture");
    let tool_loop = &fixture["toolLoop"];

    assert_eq!(
        tool_loop["streamCalls"]
            .as_array()
            .expect("stream calls")
            .len(),
        2
    );
    // `toolNames` is the tool set offered to the model. Upstream now
    // declares it on transcript system messages (`toolsAdded`), which the
    // fixture script replays with `getCurrentTools`.
    assert_eq!(
        tool_loop["streamCalls"][0]["toolNames"],
        serde_json::json!(["fixture_echo"])
    );
    assert_eq!(
        tool_loop["streamCalls"][1]["lastRole"],
        serde_json::json!("toolResult")
    );
    assert!(tool_loop["events"]
        .as_array()
        .expect("events")
        .iter()
        .any(|event| event == "tool_execution_start"));
    assert!(tool_loop["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .any(|message| message["toolResultFor"] == "fixture_echo"));
}

#[test]
fn web_tools_are_model_callable_and_respect_scope_and_offline_mode() {
    let mut runtime = Runtime::new(
        SessionState::new("web-test", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let definitions = active_tool_definitions(&runtime);
    for name in ["web_fetch", "web_search"] {
        assert!(definitions.iter().any(|tool| tool.name == name));
    }
    let fetch = ChatToolCall {
        id: "web-1".into(),
        name: "web_fetch".into(),
        arguments: r#"{"url":"https://example.org","max_chars":42}"#.into(),
    };
    assert_eq!(
        model_tool_requests(&fetch).unwrap(),
        [ToolRequest::WebFetch {
            url: "https://example.org".into(),
            max_chars: Some(42)
        }]
    );
    let bad = ChatToolCall {
        arguments: r#"{"url":"https://example.org","max_chars":1.5}"#.into(),
        ..fetch
    };
    assert!(model_tool_requests(&bad).is_err());
    runtime.systems.offline = true;
    assert!(!active_tool_definitions(&runtime)
        .iter()
        .any(|tool| tool.name.starts_with("web_")));
    assert!(parse_tool_command("/web-search rust async")
        .unwrap()
        .is_some());
    assert!(!model_tool_enabled(&runtime, "web_fetch"));
    let next = ReloadableSystems {
        available_tool_names: default_active_tool_names(),
        ..Default::default()
    };
    runtime.reload(next).unwrap();
    assert!(runtime.systems.offline);
    runtime.systems.tool_scope_configured = true;
    runtime.systems.available_tool_names.clear();
    runtime.systems.offline = false;
    assert!(active_tool_definitions(&runtime).is_empty());
    assert!(!model_tool_enabled(&runtime, "web_fetch"));
    let old: ToolEvent =
        serde_json::from_str(r#"{"id":"old","name":"read","result":"ok"}"#).unwrap();
    assert!(!old.is_error);
}

#[test]
fn read_slash_command_parses_offset_and_limit_kwargs() {
    let parsed = parse_tool_command("/read big.txt offset=2001 limit=100")
        .unwrap()
        .unwrap();
    assert_eq!(parsed.name, "read");
    assert_eq!(
        parsed.request,
        ToolRequest::Read {
            path: "big.txt".into(),
            offset: Some(2001),
            limit: Some(100)
        }
    );
    // Plain path still works; unknown kwargs are ignored, not fatal.
    let plain = parse_tool_command("/read a.txt").unwrap().unwrap();
    assert_eq!(
        plain.request,
        ToolRequest::Read {
            path: "a.txt".into(),
            offset: None,
            limit: None
        }
    );
    let ignored = parse_tool_command("/read a.txt bogus=1").unwrap().unwrap();
    assert_eq!(
        ignored.request,
        ToolRequest::Read {
            path: "a.txt".into(),
            offset: None,
            limit: None
        }
    );
}

/// Text that starts with a slash but is no tool command is a prompt for
/// the model, as upstream sends it: a path, or a command nothing defines.
#[tokio::test]
async fn a_prompt_that_is_no_tool_command_goes_to_the_model() {
    for prompt in ["/nope hi", "/home/user/main.rs is broken", "/help", "/"] {
        assert_eq!(parse_tool_command(prompt).unwrap(), None, "{prompt}");
    }
    // A tool command without its argument is still refused.
    assert!(matches!(
        parse_tool_command("/read"),
        Err(AgentError::MissingCommandArgument(command)) if command == "/read"
    ));

    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });
    let reply = run_user_turn(&mut runtime, provider.as_ref(), "/nope hi".to_string())
        .await
        .expect("the model answers");
    assert_eq!(reply, "[faux/echo] /nope hi");
    let roles = runtime
        .session()
        .messages
        .iter()
        .map(|message| &message.role);
    assert_eq!(
        roles.collect::<Vec<_>>(),
        [&MessageRole::User, &MessageRole::Assistant]
    );
}

#[tokio::test]
async fn web_tool_failures_return_to_model_and_persist_error_status() {
    let mut runtime = Runtime::new(
        SessionState::new("web-failure", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    execute_model_tool_call(
        &mut runtime,
        &unused_provider(),
        &ChatToolCall {
            id: "bad-url".into(),
            name: "web_fetch".into(),
            arguments: r#"{"url":"file:///etc/passwd"}"#.into(),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    let event = &runtime.session.tool_history[0];
    assert!(event.is_error);
    assert!(event.result.contains("HTTP(S)"));
    assert_eq!(
        runtime.session.messages[0].tool_call_id.as_deref(),
        Some("bad-url")
    );
    execute_model_tool_call(
        &mut runtime,
        &unused_provider(),
        &ChatToolCall {
            id: "bad-search".into(),
            name: "web_search".into(),
            arguments: r#"{"query":"rust","limit":0}"#.into(),
        },
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(runtime.session.tool_history[1].is_error);
    assert!(runtime.session.tool_history[1].result.contains("limit"));
}

#[tokio::test]
async fn web_fetch_executes_through_model_loop_with_a_local_server() {
    use std::net::TcpListener;
    struct WebProvider {
        url: String,
    }
    #[async_trait::async_trait]
    impl Provider for WebProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            if request
                .messages
                .iter()
                .any(|message| message.role == ChatRole::Tool)
            {
                assert!(request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("fixture web source")));
                return Ok(vec![StreamEvent::Text("verified".into())]);
            }
            assert!(request.tools.iter().any(|tool| tool.name == "web_fetch"));
            Ok(vec![StreamEvent::ToolCall {
                id: "fetch-1".into(),
                name: "web_fetch".into(),
                arguments: json!({"url": self.url}).to_string(),
            }])
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).unwrap() > 0);
        let text = "fixture web source";
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).unwrap();
    });
    let mut runtime = Runtime::new(
        SessionState::new("web-loop", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &WebProvider { url },
        "verify source".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(answer, "verified");
    assert!(events.iter().any(|event| matches!(
        event,
        TurnEvent::ToolOutput {
            is_error: false,
            ..
        }
    )));
    assert!(!runtime.session.tool_history[0].is_error);
    thread.join().unwrap();
}

#[tokio::test]
async fn model_bash_commands_see_the_session_model_and_thinking_level() {
    /// Runs one bash command, then answers with what it printed.
    struct BashProvider;
    #[async_trait::async_trait]
    impl Provider for BashProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let last = request.messages.last().unwrap();
            if last.role == ChatRole::Tool {
                return Ok(vec![StreamEvent::Text(last.content.clone())]);
            }
            // Each value under noel's name, then under pi's.
            let command = r#"printf '%s\n' "${NOEL_SESSION_ID-unset}" "${NOEL_SESSION_FILE-unset}" "${NOEL_PROVIDER-unset}" "${NOEL_MODEL-unset}" "${NOEL_REASONING_LEVEL-unset}" "${PI_SESSION_ID-unset}" "${PI_SESSION_FILE-unset}" "${PI_PROVIDER-unset}" "${PI_MODEL-unset}" "${PI_REASONING_LEVEL-unset}""#;
            Ok(vec![StreamEvent::ToolCall {
                id: "bash-1".into(),
                name: "bash".into(),
                arguments: json!({ "command": command }).to_string(),
            }])
        }

        fn thinking_level(&self) -> Option<String> {
            Some("high".into())
        }
    }
    let base = durable_test_dir("bash-session-env");
    let (store, mut session) =
        SessionStore::create_with_id(&base, base.clone(), "env-session").unwrap();
    let session_file = store.path().display().to_string();
    session.active_model = Some(ModelRef {
        provider: "faux".into(),
        id: "echo".into(),
    });
    let mut saved = Runtime::with_store(session, ReloadableSystems::default(), store);
    // A session without a journal or a model has neither to report.
    let mut unsaved = Runtime::new(
        SessionState::new("ephemeral", base.clone()),
        ReloadableSystems::default(),
    );
    for (runtime, expected) in [
        (
            &mut saved,
            format!("env-session\n{session_file}\nfaux\necho\nhigh\n"),
        ),
        (
            &mut unsaved,
            "ephemeral\nunset\nunset\nunset\nhigh\n".into(),
        ),
    ] {
        runtime.systems.shell_path = Some("/bin/sh".into());
        let printed = run_user_turn_streaming_events_with_media(
            runtime,
            &BashProvider,
            "run it".into(),
            Vec::new(),
            &RunControl::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(printed, format!("{expected}{expected}"));
    }
    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn dropping_an_in_flight_turn_kills_the_model_bash_process_group() {
    // The TUI interrupts a turn (Esc, ctrl+c, /interrupt) by dropping its
    // future; a model bash call running at that moment must take its
    // whole process group with it, including background children.
    struct BashProvider(String);
    #[async_trait::async_trait]
    impl Provider for BashProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![StreamEvent::ToolCall {
                id: "bash-1".into(),
                name: "bash".into(),
                arguments: json!({ "command": self.0 }).to_string(),
            }])
        }
    }
    let pid_file = std::env::temp_dir().join(format!("noel-turn-drop-{}.pid", new_session_id()));
    let provider = BashProvider(format!(
        "sleep 30 & echo $! > '{0}.tmp' && mv '{0}.tmp' '{0}'; wait",
        pid_file.display()
    ));
    let mut runtime = Runtime::new(
        SessionState::new("turn-drop", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    runtime.systems.shell_path = Some("/bin/sh".into());
    let run = RunControl::default();
    let mut turn = Box::pin(run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "run it".into(),
        Vec::new(),
        &run,
        |_| {},
    ));
    let published_pid = async {
        loop {
            if let Ok(pid) = fs::read_to_string(&pid_file) {
                return pid.trim().parse::<i32>().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let pid = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = &mut turn => panic!("turn ended before the interrupt: {result:?}"),
            pid = published_pid => pid,
        }
    })
    .await
    .expect("bash published its background pid");
    drop(turn);
    let _ = fs::remove_file(&pid_file);
    // A killed process stays a zombie (state Z) until reaped.
    let running = || {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while cfg!(target_os = "linux") && running() {
        assert!(
            std::time::Instant::now() < deadline,
            "background child {pid} outlived the interrupted turn"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn failed_auto_compaction_does_not_kill_the_turn() {
    struct EchoProvider;
    #[async_trait::async_trait]
    impl Provider for EchoProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![StreamEvent::Text("still working".into())])
        }
    }
    // An enormous prompt as the only/newest turn leaves nothing older to
    // compact, so auto_compact fails. The turn must continue and surface
    // the failure as an event instead of an error that strands the session.
    let mut runtime = Runtime::new(
        SessionState::new("compaction-stuck", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &EchoProvider,
        "huge ".repeat(120_000),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(answer, "still working");
    assert!(events
        .iter()
        .any(|event| matches!(event, TurnEvent::CompactionFailed(_))));
}

#[test]
fn session_store_forks_with_metadata_and_parent() {
    let base = std::env::temp_dir().join(format!("noel-session-fork-test-{}", new_session_id()));
    let (store, mut state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    state.name = Some("main".to_string());
    state.labels = BTreeSet::from(["feature".to_string()]);
    state.messages.push(ConversationMessage {
        thinking: String::new(),
        role: MessageRole::User,
        content: "parent prompt".to_string(),
        media: Vec::new(),
        tool_call_id: None,
        tool_name: None,
        tool_calls: Vec::new(),
        aborted: false,
    });
    store.record_metadata(&state).expect("record metadata");
    store
        .record_message(state.messages[0].clone())
        .expect("record message");

    let (fork_store, forked) = SessionStore::fork(&base, &state, false).expect("fork session");
    let (_opened_store, opened) =
        SessionStore::open(fork_store.path().to_path_buf()).expect("open fork");

    assert_ne!(forked.session_id, state.session_id);
    assert_eq!(opened.parent_session_id, Some(state.session_id.clone()));
    assert_eq!(opened.name, Some("main".to_string()));
    assert_eq!(opened.labels, BTreeSet::from(["feature".to_string()]));
    assert_eq!(opened.messages[0].content, "parent prompt");
    assert_eq!(opened.branch_summaries.len(), 1);
    assert_eq!(opened.branch_summaries[0].from_session_id, state.session_id);
    assert_eq!(opened.branch_summaries[0].to_session_id, forked.session_id);

    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn compact_messages_persists_summary_record_and_snapshot() {
    let base = std::env::temp_dir().join(format!("noel-compact-test-{}", new_session_id()));
    let (store, mut state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    for index in 0..8 {
        state.messages.push(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: format!("message {index} {}", "detail ".repeat(30)),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        });
        store
            .record_message(state.messages[index].clone())
            .expect("record message");
    }
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());

    runtime.systems.compaction.keep_recent_tokens = 0;
    let record = runtime
        .compact_messages(&CheckpointProvider, CompactionKind::Manual, "")
        .await
        .expect("compact messages");

    assert_eq!(record.omitted_messages, 7);
    assert_eq!(record.retained_messages, 1);
    assert_eq!(runtime.session().messages.len(), 2);
    assert_eq!(runtime.session().compactions, [record]);
    let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("open compacted");
    assert_eq!(loaded.compactions.len(), 1);
    assert_eq!(loaded.messages[0].role, MessageRole::Assistant);
    assert!(loaded.messages[0].content.contains("Goals and constraints"));

    let _ = fs::remove_dir_all(base);
}

#[test]
fn session_store_resolves_id_prefix_and_name() {
    let base = std::env::temp_dir().join(format!("noel-session-resolve-test-{}", new_session_id()));
    let (store, mut state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    state.name = Some("named-session".to_string());
    store.record_metadata(&state).expect("record metadata");

    let prefix = &state.session_id[..8];
    assert_eq!(
        SessionStore::resolve(&base, prefix).expect("resolve prefix"),
        Some(store.path().to_path_buf())
    );
    assert_eq!(
        SessionStore::resolve(&base, "named-session").expect("resolve name"),
        Some(store.path().to_path_buf())
    );
    assert_eq!(
        SessionStore::resolve(&base, "1").expect("resolve index"),
        Some(store.path().to_path_buf())
    );

    let _ = fs::remove_dir_all(base);
}

#[test]
fn messages_snapshot_replaces_loaded_messages() {
    let base =
        std::env::temp_dir().join(format!("noel-session-snapshot-test-{}", new_session_id()));
    let (store, _state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    store
        .record_message(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "old".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        })
        .expect("record message");
    store
        .record_messages_snapshot(vec![ConversationMessage {
            thinking: String::new(),
            role: MessageRole::System,
            content: "summary".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        }])
        .expect("record snapshot");

    let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("open session");

    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content, "summary");

    let _ = fs::remove_dir_all(base);
}

const STARTED_LINE: &str = "{\"type\":\"started\",\"session_id\":\"durable\",\"cwd\":\"/repo\"}\n";
const TORN_RECORD: &str = "{\"type\":\"message\",\"message\":{\"role\":\"us";

fn durable_test_dir(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("noel-{name}-{}", new_session_id()));
    fs::create_dir_all(&base).unwrap();
    base
}

fn user_line(content: &str) -> String {
    format!("{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"{content}\"}}}}")
}

fn message_texts(state: &SessionState) -> Vec<&str> {
    state
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect()
}

fn assert_every_line_is_a_record(path: &Path) -> usize {
    let content = fs::read_to_string(path).unwrap();
    assert!(
        content.ends_with('\n'),
        "journal ends mid-record: {content:?}"
    );
    for line in content.lines() {
        serde_json::from_str::<SessionRecord>(line)
            .unwrap_or_else(|error| panic!("corrupt journal line {line:?}: {error}"));
    }
    content.lines().count()
}

/// Takes a session's lock file the way a second noel process would: through
/// its own open file description, outside this process's registry.
fn hold_lock_elsewhere(journal: &Path, pid: u32) -> File {
    let lock_path = journal.with_file_name(format!(
        "{}.lock",
        journal.file_name().unwrap().to_string_lossy()
    ));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    file.try_lock().unwrap();
    file.set_len(0).unwrap();
    writeln!(file, "{pid}").unwrap();
    file
}

#[test]
fn readers_ignore_a_torn_final_segment_without_touching_the_journal() {
    let base = durable_test_dir("torn-read");
    let path = base.join("torn.jsonl");
    let content = format!("{STARTED_LINE}{}\n{TORN_RECORD}", user_line("kept"));
    fs::write(&path, &content).unwrap();

    let state = SessionStore::read(&path).unwrap();
    assert_eq!(state.session_id, "durable");
    assert_eq!(message_texts(&state), ["kept"]);
    let listed = SessionStore::list(&base).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].message_count, 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), content);
    assert!(!base.join("torn.jsonl.lock").exists());

    // A complete final record that only lost its newline still counts.
    let unterminated = format!("{STARTED_LINE}{}\n{}", user_line("one"), user_line("two"));
    fs::write(&path, &unterminated).unwrap();
    assert_eq!(
        message_texts(&SessionStore::read(&path).unwrap()),
        ["one", "two"]
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), unterminated);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_corrupt_line_before_the_end_stays_a_hard_error() {
    let base = durable_test_dir("corrupt-middle");
    let path = base.join("corrupt.jsonl");
    fs::write(
        &path,
        format!("{STARTED_LINE}{TORN_RECORD}\n{}\n", user_line("after")),
    )
    .unwrap();
    assert!(matches!(
        SessionStore::read(&path),
        Err(SessionError::Parse { .. })
    ));
    assert!(matches!(
        SessionStore::open(path.clone()),
        Err(SessionError::Parse { .. })
    ));
    // A whole JSON value that is not a valid record is not a torn write.
    fs::write(
        &path,
        format!("{STARTED_LINE}{{\"type\":\"message\",\"message\":7}}"),
    )
    .unwrap();
    assert!(matches!(
        SessionStore::read(&path),
        Err(SessionError::Parse { .. })
    ));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn writers_repair_torn_and_unterminated_tails_before_appending() {
    let base = durable_test_dir("tail-repair");
    let path = base.join("durable.jsonl");
    let kept = format!("{STARTED_LINE}{}\n", user_line("kept"));
    fs::write(&path, format!("{kept}{TORN_RECORD}")).unwrap();
    let (store, state) = SessionStore::open(path.clone()).unwrap();
    assert_eq!(message_texts(&state), ["kept"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), kept);
    store
        .record_message(test_message(MessageRole::User, "next"))
        .unwrap();
    assert_eq!(assert_every_line_is_a_record(&path), 3);
    assert_eq!(
        message_texts(&SessionStore::read(&path).unwrap()),
        ["kept", "next"]
    );
    drop(store);

    fs::write(&path, format!("{STARTED_LINE}{}", user_line("last"))).unwrap();
    let (store, state) = SessionStore::open(path.clone()).unwrap();
    assert_eq!(message_texts(&state), ["last"]);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{STARTED_LINE}{}\n", user_line("last"))
    );
    drop(store);

    // Creating over an existing torn journal repairs it the same way.
    fs::write(&path, format!("{kept}{TORN_RECORD}")).unwrap();
    let (store, _) =
        SessionStore::create_with_id(&base, PathBuf::from("/repo"), "durable").unwrap();
    assert_eq!(assert_every_line_is_a_record(store.path()), 3);
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn listing_skips_a_session_that_fails_to_load() {
    let base = durable_test_dir("list-skip");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let good = store.path().to_path_buf();
    drop(store);
    fs::write(
        base.join("broken.jsonl"),
        format!("{STARTED_LINE}{TORN_RECORD}\n{STARTED_LINE}"),
    )
    .unwrap();
    let listed = SessionStore::list(&base).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, good);
    assert_eq!(SessionStore::resolve(&base, "1").unwrap(), Some(good));
    fs::remove_dir_all(base).unwrap();
}

/// Durable's `configure({compaction})` (upstream 922c3ff2): the
/// conversation's settings are journaled, survive a reopen and a fork,
/// and resolve over the settings file's per model.
#[test]
fn a_conversations_compaction_settings_persist_and_resolve_per_model() {
    let base = durable_test_dir("compaction-settings");
    let (store, state) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    runtime
        .set_active_model(Some(ModelRef {
            provider: "local".into(),
            id: "small-32k".into(),
        }))
        .unwrap();
    runtime.systems.compaction.model_overrides.insert(
        "local/small-32k".into(),
        noel_config::CompactionPolicyFields {
            keep_recent_tokens: Some(8_000),
            ..Default::default()
        },
    );
    assert_eq!(runtime.compaction_policy().keep_recent_tokens, 8_000);
    let settings: noel_config::CompactionSettings =
        serde_json::from_value(serde_json::json!({"enabled": false, "reserveTokens": 4096}))
            .unwrap();
    runtime
        .set_compaction_settings(Some(settings.clone()))
        .unwrap();
    let policy = runtime.compaction_policy();
    assert!(!policy.enabled);
    assert_eq!(policy.reserve_tokens, 4096);
    // The settings' model override still beats the conversation's
    // general field it does not set.
    assert_eq!(policy.keep_recent_tokens, 8_000);
    drop(runtime);
    let (store, reopened) = SessionStore::open(path.clone()).unwrap();
    assert_eq!(reopened.compaction, Some(settings.clone()));
    let (_fork, forked) = SessionStore::fork(&base, &reopened, false).unwrap();
    assert_eq!(forked.compaction, Some(settings));
    let mut runtime = Runtime::with_store(reopened, ReloadableSystems::default(), store);
    runtime.set_compaction_settings(None).unwrap();
    drop(runtime);
    assert_eq!(SessionStore::read(&path).unwrap().compaction, None);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn a_session_held_by_another_process_is_refused_with_its_pid() {
    let base = durable_test_dir("locked");
    let (store, state) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let journal = fs::read_to_string(&path).unwrap();
    let other = hold_lock_elsewhere(&path, 4242);

    match SessionStore::open(path.clone()) {
        Err(SessionError::Locked {
            path: locked,
            pid: Some(4242),
        }) => assert!(locked.ends_with(path.file_name().unwrap())),
        other => panic!("expected the session to be locked, got {other:?}"),
    }
    let message = SessionStore::open(path.clone()).unwrap_err().to_string();
    assert!(message.contains("in use by another noel process (pid 4242)"));
    assert!(message.contains(&state.session_id));
    assert!(matches!(
        SessionStore::delete(&path),
        Err(SessionError::Locked { .. })
    ));
    assert!(matches!(
        SessionStore::import(&base, SessionExport::from(&state)),
        Err(SessionError::Locked { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), journal);

    // Like a crashed holder, `other` leaves its lock file behind.
    drop(other);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (store, reopened) = loop {
        match SessionStore::open(path.clone()) {
            // A fork on another test thread holds a copy of every open
            // descriptor until its exec; wait that out.
            Err(SessionError::Locked { .. }) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => break result.unwrap(),
        }
    };
    assert_eq!(reopened.session_id, state.session_id);
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn this_process_can_reopen_a_session_it_holds() {
    let base = durable_test_dir("same-process");
    let (store, state) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let (again, _) = SessionStore::open(store.path().to_path_buf()).unwrap();
    assert!(Arc::ptr_eq(store.lock(), again.lock()));
    let spelled = base.join(".").join(store.path().file_name().unwrap());
    let (spelled, _) = SessionStore::open(spelled).unwrap();
    assert!(Arc::ptr_eq(store.lock(), spelled.lock()));
    // Importing onto the id this process holds rewrites it in place.
    let (imported, _) = SessionStore::import(&base, SessionExport::from(&state)).unwrap();
    assert!(Arc::ptr_eq(store.lock(), imported.lock()));

    let lock_path = store.lock().lock_path().to_path_buf();
    assert!(lock_path.exists());
    drop((store, again, spelled));
    assert!(lock_path.exists(), "a live handle keeps the lock");
    drop(imported);
    assert!(!lock_path.exists(), "the last handle removes the lock file");
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn the_history_heard_live_is_the_history_read_back() {
    let base = durable_test_dir("history");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = heard.clone();
    let store = store.observe_progress(Arc::new(move |saved| {
        if let SavedProgress::History(record) = saved {
            lock(&sink).push(record.clone());
        }
    }));
    store
        .record_message(test_message(MessageRole::User, "one"))
        .unwrap();
    store
        .record_message(test_message(MessageRole::Assistant, "two"))
        .unwrap();
    store
        .record_compaction_checkpoint(
            CompactionRecord {
                kind: CompactionKind::Automatic,
                omitted_messages: 2,
                retained_messages: 0,
                summary: "summary".into(),
            },
            vec![test_message(MessageRole::User, "summary")],
        )
        .unwrap();
    store
        .record_message(test_message(MessageRole::User, "three"))
        .unwrap();
    // An undo: the context is cut back to one message.
    store
        .record_messages_snapshot(vec![test_message(MessageRole::User, "summary")])
        .unwrap();

    let read = SessionStore::read_history(store.path()).unwrap();
    assert_eq!(*lock(&heard), read);
    let mut history = history::History::default();
    for record in &read {
        history.push(record, |message, _| json!(message.content));
    }
    let kinds: Vec<(u64, String)> = history
        .entries()
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.to_json()["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    // The checkpoint rewrote the journal, and every message stayed.
    assert_eq!(
        kinds,
        [
            (1, "pi.user".to_owned()),
            (2, "pi.assistant".to_owned()),
            (3, "pi.compaction".to_owned()),
            (4, "pi.user".to_owned()),
            (5, "pi.reset".to_owned()),
        ]
    );
    assert!(history.entries()[0].timestamp.is_some());
}

#[test]
fn the_lock_survives_a_compaction_checkpoint_rename() {
    let base = durable_test_dir("checkpoint-lock");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    store
        .record_message(test_message(MessageRole::User, "before"))
        .unwrap();
    #[cfg(unix)]
    let inode = || std::os::unix::fs::MetadataExt::ino(&fs::metadata(store.path()).unwrap());
    #[cfg(unix)]
    let before = inode();
    store
        .record_compaction_checkpoint(
            CompactionRecord {
                kind: CompactionKind::Manual,
                omitted_messages: 1,
                retained_messages: 1,
                summary: "summary".into(),
            },
            vec![test_message(MessageRole::Assistant, "checkpoint")],
        )
        .unwrap();
    #[cfg(unix)]
    assert_ne!(inode(), before, "the checkpoint replaces the journal");

    let probe = File::open(store.lock().lock_path()).unwrap();
    assert!(matches!(
        probe.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    store
        .record_message(test_message(MessageRole::User, "after"))
        .unwrap();
    assert_eq!(
        message_texts(&SessionStore::read(store.path()).unwrap()),
        ["checkpoint", "after"]
    );
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_checkpoint_drops_a_torn_tail_instead_of_patching_it() {
    let base = durable_test_dir("checkpoint-torn");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    store
        .record_message(test_message(MessageRole::User, "kept"))
        .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(TORN_RECORD.as_bytes())
        .unwrap();
    store
        .record_compaction_checkpoint(
            CompactionRecord {
                kind: CompactionKind::Manual,
                omitted_messages: 1,
                retained_messages: 0,
                summary: "summary".into(),
            },
            vec![test_message(MessageRole::Assistant, "checkpoint")],
        )
        .unwrap();
    assert_eq!(assert_every_line_is_a_record(store.path()), 3);
    assert_eq!(
        message_texts(&SessionStore::read(store.path()).unwrap()),
        ["checkpoint"]
    );
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn appends_from_store_clones_never_interleave() {
    let base = durable_test_dir("concurrent-appends");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let writers = (0..2)
        .map(|writer| {
            let store = store.clone();
            std::thread::spawn(move || {
                for index in 0..200 {
                    let content = format!("{writer}-{index}-{}", "x".repeat(8192));
                    store
                        .record_message(test_message(MessageRole::User, &content))
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(assert_every_line_is_a_record(store.path()), 401);
    assert_eq!(
        SessionStore::read(store.path()).unwrap().messages.len(),
        400
    );
    drop(store);
    fs::remove_dir_all(base).unwrap();
}

#[cfg(unix)]
#[test]
fn exec_closes_the_lock_so_children_never_keep_a_session() {
    let base = durable_test_dir("cloexec");
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    // A second description of the same lock inode, to observe the OS lock
    // after the store unlinks the path on drop.
    let probe = File::open(store.lock().lock_path()).unwrap();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    drop(store);
    // A fork on another test thread copies every descriptor until its
    // exec, so allow a moment; an inherited lock would last 30 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let released = loop {
        match probe.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => break result,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        released.is_ok(),
        "an exec'd child inherited the session lock: {released:?}"
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn deleting_a_session_takes_its_lock_and_cleans_up() {
    let base = durable_test_dir("delete");
    let missing = base.join("missing.jsonl");
    assert!(matches!(
        SessionStore::open(missing),
        Err(SessionError::Open { .. })
    ));
    assert!(!base.join("missing.jsonl.lock").exists());

    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let path = store.path().to_path_buf();
    let lock_path = store.lock().lock_path().to_path_buf();
    drop(store);
    SessionStore::delete(&path).unwrap();
    assert!(!path.exists());
    assert!(!lock_path.exists());

    // The session this process holds can be deleted; its lock file goes
    // away with the last handle.
    let (store, _) = SessionStore::create(&base, PathBuf::from("/repo")).unwrap();
    let lock_path = store.lock().lock_path().to_path_buf();
    SessionStore::delete(store.path()).unwrap();
    assert!(!store.path().exists());
    assert!(lock_path.exists());
    drop(store);
    assert!(!lock_path.exists());
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn run_user_turn_records_user_and_assistant_messages() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });

    let response = run_user_turn(&mut runtime, provider.as_ref(), "hello".to_string())
        .await
        .expect("run turn");

    assert_eq!(response, "[faux/echo] hello");
    assert_eq!(runtime.session().messages.len(), 2);
    assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    assert_eq!(runtime.session().messages[1].role, MessageRole::Assistant);
}

#[tokio::test]
async fn run_user_turn_streams_text_deltas() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });
    let mut deltas = Vec::new();

    let response = run_user_turn_streaming(
        &mut runtime,
        provider.as_ref(),
        "hello".to_string(),
        |delta| deltas.push(delta.to_string()),
    )
    .await
    .expect("run streaming turn");

    assert_eq!(deltas, ["[faux/echo] ", "hello"]);
    assert_eq!(response, "[faux/echo] hello");
    assert_eq!(runtime.session().messages[1].content, response);
}

#[tokio::test]
async fn midturn_compaction_recovers_a_single_turn_that_outgrows_the_window() {
    // Reproduces the original failure: one user turn, many tool rounds,
    // context over budget. Whole-turn compaction has nothing older to
    // summarize, so the mid-turn path summarizes completed rounds and the
    // turn continues instead of failing every round.
    let cwd = std::env::temp_dir().join(format!("noel-midturn-loop-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
    let mut session = SessionState::new("midturn-loop", cwd.clone());
    session.active_model = Some(ModelRef {
        provider: "faux".to_string(),
        id: "echo".to_string(),
    });
    let mut runtime = Runtime::new(
        session,
        ReloadableSystems {
            compaction: RuntimeCompactionSettings {
                keep_recent_tokens: 500,
                ..Default::default()
            },
            model_context_windows: BTreeMap::from([("faux/echo".to_string(), 3_000)]),
            ..Default::default()
        },
    );
    // Provider: 6 read rounds with ~1k-token outputs, then a final answer.
    use std::sync::atomic::AtomicUsize;
    #[derive(Default)]
    struct BigLoopProvider {
        rounds: AtomicUsize,
        summaries: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for BigLoopProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            // Summarizer requests carry no tools and a single transcript
            // message; count them separately.
            let is_summary = request.tools.is_empty()
                && request.messages.len() == 1
                && request.messages[0].content.contains("transcript fragment");
            if is_summary {
                self.summaries.fetch_add(1, Ordering::SeqCst);
                return Ok(vec![
                    StreamEvent::Text("checkpoint: files read so far".into()),
                    StreamEvent::Stop {
                        reason: "stop".into(),
                    },
                ]);
            }
            let round = self.rounds.fetch_add(1, Ordering::SeqCst);
            if round < 6 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: format!("call_read_{round}"),
                        name: "read".to_string(),
                        arguments: format!(
                            "{{\"path\":\"a.txt\",\"padding\":\"{}\"}}",
                            "x".repeat(4_000)
                        ),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".into(),
                    },
                ]);
            }
            Ok(vec![
                StreamEvent::Text("all done".into()),
                StreamEvent::Stop {
                    reason: "stop".into(),
                },
            ])
        }
    }
    let provider = BigLoopProvider::default();
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "read everything".to_string(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .expect("turn must not strand");
    assert_eq!(answer, "all done");
    assert!(provider.summaries.load(Ordering::SeqCst) >= 1);
    assert!(events
        .iter()
        .any(|event| matches!(event, TurnEvent::CompactionFinished(_))));
    // The session ended with a checkpoint and intact tool pairing.
    assert!(runtime
        .session()
        .messages
        .iter()
        .any(|message| message.role == MessageRole::Assistant
            && message.content.contains("Previous conversation checkpoint")));
    for message in &runtime.session().messages {
        for call in &message.tool_calls {
            assert!(
                runtime
                    .session()
                    .messages
                    .iter()
                    .any(|m| m.tool_call_id.as_deref() == Some(call.id.as_str())),
                "call {} separated from its result",
                call.id
            );
        }
    }
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn turn_events_report_tool_lifecycle() {
    let cwd = std::env::temp_dir().join(format!("noel-turn-events-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ToolLoopProvider {
        requests: requests.clone(),
    };
    let mut events = Vec::new();

    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "read the file".to_string(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .expect("run tool loop");

    assert_eq!(answer, "done");
    // Upstream's order: tool_execution_end, then the tool-result
    // message, then turn_end.
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "read the file""#,
            "call call_read_1 read",
            "stop toolUse",
            r#"assistant "" -> call_read_1"#,
            r#"tool call_read_1 read {"path":"a.txt"}"#,
            r#"output call_read_1 "file contents""#,
            "finished read",
            "round finished",
            "round started",
            r#"text "done""#,
            "stop stop",
            r#"assistant "done""#,
            "round finished",
        ]
    );
    assert!(events.contains(&TurnEvent::MessageAdded(
        runtime.session.messages[1].clone()
    )));

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn a_text_reply_is_one_round_reporting_its_saved_messages() {
    let mut runtime = Runtime::new(
        SessionState::new("text-round", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let provider = Script::new([reply(vec![
        StreamEvent::Thinking("plan".into()),
        StreamEvent::Text("hel".into()),
        StreamEvent::Text("lo".into()),
        StreamEvent::Usage {
            input_tokens: 3,
            output_tokens: 2,
        },
        stop("stop"),
    ])]);
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "hi".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(answer, "hello");
    // Upstream's turn_start comes before the prompt's message events and
    // the reply's message_end after its last delta.
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "hi""#,
            r#"thinking "plan""#,
            r#"text "hel""#,
            r#"text "lo""#,
            "usage 3/2",
            "stop stop",
            r#"assistant "hello""#,
            "round finished",
        ]
    );
    let added = events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::MessageAdded(message) => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(added, runtime.session.messages);
    assert_eq!(added[1].thinking, "plan");
}

#[tokio::test]
async fn steering_is_reported_after_the_start_of_the_round_it_joins() {
    let cwd = std::env::temp_dir().join(format!("noel-steering-events-{}", new_session_id()));
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("a.txt"), "file contents").unwrap();
    let mut runtime = Runtime::new(
        SessionState::new("steering-events", cwd.clone()),
        ReloadableSystems::default(),
    );
    let run = RunControl::default();
    let (during_tools, during_reply) = (run.steering.clone(), run.steering.clone());
    let provider = Script::new([
        Box::new(move || {
            during_tools.send("also b.txt".into());
            Ok(vec![tool_call("read-a", "read", json!({"path": "a.txt"}))])
        }) as Reply,
        // Steering that arrives during a final reply keeps the turn going.
        Box::new(move || {
            during_reply.send("one more".into());
            Ok(vec![StreamEvent::Text("read".into())])
        }),
        reply(vec![StreamEvent::Text("done".into())]),
    ]);
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "read a.txt".into(),
        Vec::new(),
        &run,
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(answer, "readdone");
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "read a.txt""#,
            "call read-a read",
            r#"assistant "" -> read-a"#,
            r#"tool read-a read {"path":"a.txt"}"#,
            r#"output read-a "file contents""#,
            "finished read",
            "round finished",
            "round started",
            r#"user "also b.txt""#,
            r#"text "read""#,
            r#"assistant "read""#,
            "round finished",
            "round started",
            r#"user "one more""#,
            r#"text "done""#,
            r#"assistant "done""#,
            "round finished",
        ]
    );
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn the_round_limit_ends_the_turn_after_its_last_round() {
    let cwd = std::env::temp_dir().join(format!("noel-round-limit-events-{}", new_session_id()));
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("a.txt"), "file contents").unwrap();
    let mut runtime = Runtime::new(
        SessionState::new("round-limit-events", cwd.clone()),
        ReloadableSystems {
            max_tool_rounds: 2,
            ..Default::default()
        },
    );
    let provider = Script::new((0..2).map(|round| {
        reply(vec![tool_call(
            &format!("read-{round}"),
            "read",
            json!({"path": "a.txt"}),
        )])
    }));
    let mut events = Vec::new();
    let error = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "read forever".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AgentError::RoundLimitReached { max_rounds: 2 }
    ));
    let round = |id: &str| {
        [
            "round started".to_string(),
            format!("call {id} read"),
            format!(r#"assistant "" -> {id}"#),
            format!(r#"tool {id} read {{"path":"a.txt"}}"#),
            format!(r#"output {id} "file contents""#),
            "finished read".to_string(),
            "round finished".to_string(),
        ]
    };
    let mut expected = round("read-0").to_vec();
    expected.insert(1, r#"user "read forever""#.into());
    expected.extend(round("read-1"));
    assert_eq!(trace(&events), expected);

    // Steering that arrives during the last allowed reply is still saved;
    // it opens a round that the limit then ends.
    let mut runtime = Runtime::new(
        SessionState::new("round-limit-steering", cwd.clone()),
        ReloadableSystems {
            max_tool_rounds: 1,
            ..Default::default()
        },
    );
    let run = RunControl::default();
    let steering = run.steering.clone();
    let provider = Script::new([Box::new(move || {
        steering.send("keep going".into());
        Ok(vec![StreamEvent::Text("done".into())])
    }) as Reply]);
    let mut events = Vec::new();
    let error = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "hi".into(),
        Vec::new(),
        &run,
        |event| events.push(event.clone()),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AgentError::RoundLimitReached { max_rounds: 1 }
    ));
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "hi""#,
            r#"text "done""#,
            r#"assistant "done""#,
            "round finished",
            "round started",
            r#"user "keep going""#,
        ]
    );
    assert_eq!(
        runtime.session.messages.last().unwrap().content,
        "keep going"
    );
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn mid_turn_compaction_is_reported_inside_its_round() {
    struct Summarizing;
    #[async_trait::async_trait]
    impl Provider for Summarizing {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            // Only summary requests come without tools.
            let text = if request.tools.is_empty() {
                "Goals: finish the task."
            } else {
                "done"
            };
            Ok(vec![StreamEvent::Text(text.into())])
        }
    }
    let mut state = SessionState::new("compaction-events", PathBuf::from("."));
    state.active_model = Some(ModelRef {
        provider: "faux".into(),
        id: "echo".into(),
    });
    state.messages = vec![
        test_message(MessageRole::User, &"old ".repeat(6_000)),
        test_message(MessageRole::Assistant, "old answer"),
    ];
    let mut runtime = Runtime::new(
        state,
        ReloadableSystems {
            compaction: RuntimeCompactionSettings {
                keep_recent_tokens: 0,
                reserve_tokens: 0,
                ..Default::default()
            },
            model_context_windows: BTreeMap::from([("faux/echo".to_string(), 8_000)]),
            ..Default::default()
        },
    );
    let mut events = Vec::new();
    run_user_turn_streaming_events_with_media(
        &mut runtime,
        &Summarizing,
        "continue".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "continue""#,
            "compaction started",
            "compacted 2 messages",
            r#"text "done""#,
            r#"assistant "done""#,
            "round finished",
        ]
    );
}

#[tokio::test]
async fn retries_are_reported_before_each_backoff_and_when_the_request_ends() {
    let mut runtime = Runtime::new(
        SessionState::new("retry-events", PathBuf::from(".")),
        ReloadableSystems {
            retry: RuntimeRetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 1,
                max_agent_delay_ms: 2,
            },
            ..Default::default()
        },
    );
    let provider = Script::new([
        overloaded(),
        overloaded(),
        reply(vec![StreamEvent::Text("ok".into())]),
    ]);
    let mut events = Vec::new();
    run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "hi".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    // Each retry asks again in a new round. Upstream reports
    // auto_retry_end after the reply's message_end.
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "hi""#,
            "retry 1 of 3 in 1ms: provider returned an invalid response: overloaded",
            "round started",
            "retry 2 of 3 in 2ms: provider returned an invalid response: overloaded",
            "round started",
            r#"text "ok""#,
            r#"assistant "ok""#,
            "retried 2: succeeded",
            "round finished",
        ]
    );

    runtime.systems.retry.max_retries = 1;
    let provider = Script::new([overloaded(), overloaded()]);
    let mut events = Vec::new();
    let error = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "again".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, AgentError::Provider(_)));
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "again""#,
            "retry 1 of 1 in 1ms: provider returned an invalid response: overloaded",
            "round started",
            "retried 1: provider returned an invalid response: overloaded",
        ]
    );
}

#[tokio::test]
async fn tool_output_reports_how_long_the_tool_ran() {
    let mut runtime = Runtime::new(
        SessionState::new("tool-duration", PathBuf::from(".")),
        ReloadableSystems {
            shell_path: Some("/bin/sh".into()),
            ..Default::default()
        },
    );
    let provider = Script::new([
        reply(vec![tool_call(
            "bash-1",
            "bash",
            json!({"command": "sleep 0.2"}),
        )]),
        reply(vec![StreamEvent::Text("done".into())]),
    ]);
    let mut events = Vec::new();
    run_user_turn_streaming_events_with_media(
        &mut runtime,
        &provider,
        "wait".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    let duration = events
        .iter()
        .find_map(|event| match event {
            TurnEvent::ToolOutput { duration, .. } => Some(*duration),
            _ => None,
        })
        .unwrap();
    assert!(duration >= Some(Duration::from_millis(200)), "{duration:?}");
}

/// Cancels `run` after `delay`; the handle yields when it did.
fn cancel_after(run: &RunControl, delay: Duration) -> tokio::task::JoinHandle<Instant> {
    let cancellation = run.cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        cancellation.cancel();
        Instant::now()
    })
}

fn assert_ended_promptly(cancelled_at: Instant) {
    let latency = cancelled_at.elapsed();
    assert!(
        latency < Duration::from_secs(1),
        "the turn ended {latency:?} after cancellation"
    );
}

#[tokio::test]
async fn cancelling_a_model_call_ends_the_turn_with_its_prompt_saved() {
    struct Pending;
    #[async_trait::async_trait]
    impl Provider for Pending {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            std::future::pending().await
        }
    }
    let base = std::env::temp_dir().join(format!("noel-cancel-request-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());
    let run = RunControl::default();
    let cancelled = cancel_after(&run, Duration::from_millis(20));
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_user_turn_streaming_events_with_media(
            &mut runtime,
            &Pending,
            "wait".into(),
            Vec::new(),
            &run,
            |event| events.push(event.clone()),
        ),
    )
    .await
    .expect("cancellation ends the request");
    assert_ended_promptly(cancelled.await.unwrap());
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(trace(&events), ["round started", r#"user "wait""#]);
    // The saved prompt is a turn the next run continues.
    assert_eq!(store.load().unwrap(), runtime.session);
    assert!(runtime.session.has_unfinished_turn());
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn cancelling_a_tool_call_kills_its_process_group_and_saves_its_output() {
    let base = std::env::temp_dir().join(format!("noel-cancel-tool-{}", new_session_id()));
    fs::create_dir_all(&base).unwrap();
    let pid_file = base.join("child.pid");
    let (store, state) = SessionStore::create(&base.join("sessions"), base.clone()).unwrap();
    let mut runtime = Runtime::with_store(
        state,
        ReloadableSystems {
            shell_path: Some("/bin/sh".into()),
            ..Default::default()
        },
        store.clone(),
    );
    let command = format!(
        "echo started; sleep 30 & echo $! > '{0}.tmp' && mv '{0}.tmp' '{0}'; wait",
        pid_file.display()
    );
    let provider = Script::new([reply(vec![tool_call(
        "bash-1",
        "bash",
        json!({ "command": command }),
    )])]);
    let run = RunControl::default();
    // Cancel once the tool's background child runs, and its output
    // was reported.
    let cancelled = {
        let (run, pid_file) = (run.clone(), pid_file.clone());
        tokio::spawn(async move {
            let pid = loop {
                if let Ok(pid) = fs::read_to_string(&pid_file) {
                    break pid.trim().parse::<i32>().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            tokio::time::sleep(Duration::from_millis(300)).await;
            run.cancellation.cancel();
            (pid, Instant::now())
        })
    };
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_user_turn_streaming_events_with_media(
            &mut runtime,
            &provider,
            "run it".into(),
            Vec::new(),
            &run,
            |event| events.push(event.clone()),
        ),
    )
    .await
    .expect("cancellation ends the tool call");
    let (pid, cancelled_at) = cancelled.await.unwrap();
    assert_ended_promptly(cancelled_at);
    assert!(matches!(result, Err(AgentError::Cancelled)));
    let lines = trace(&events);
    assert_eq!(
        lines[..4],
        [
            "round started",
            r#"user "run it""#,
            "call bash-1 bash",
            r#"assistant "" -> bash-1"#,
        ]
    );
    assert!(lines[4].starts_with("tool bash-1 bash"), "{lines:?}");
    // The command reported that it starts and what it printed. Its
    // result is that output, and that it was aborted.
    assert_eq!(
        lines[5..],
        [
            r#"update bash-1 """#,
            r#"update bash-1 "started\n""#,
            r#"output bash-1 "started\n\n\nOperation aborted" error"#,
            "finished bash error",
        ]
    );
    // A killed process stays a zombie (state Z) until reaped.
    let running = || {
        fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
        })
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while cfg!(target_os = "linux") && running() {
        assert!(
            Instant::now() < deadline,
            "background child {pid} outlived the cancelled turn"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The journal keeps the reply and that result, which the next
    // request sends; what the call printed as it ran is superseded.
    assert_eq!(store.load().unwrap(), runtime.session);
    assert!(runtime.session.tool_outputs.is_empty());
    let journal = fs::read_to_string(store.path()).unwrap();
    assert!(
        journal.contains(r#""type":"tool_output","call_id":"bash-1","output":"started\n""#),
        "{journal}"
    );
    assert_eq!(runtime.session.tool_history.len(), 1);
    let request = provider_messages(&runtime.session.messages);
    assert_eq!(
        request.last().unwrap().tool_call_id.as_deref(),
        Some("bash-1")
    );
    assert_eq!(
        request.last().unwrap().content,
        "started\n\n\nOperation aborted"
    );
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn cancelling_a_retry_backoff_ends_the_turn() {
    let base = std::env::temp_dir().join(format!("noel-cancel-backoff-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let mut runtime = Runtime::with_store(
        state,
        ReloadableSystems {
            retry: RuntimeRetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 60_000,
                max_agent_delay_ms: 60_000,
            },
            ..Default::default()
        },
        store.clone(),
    );
    let provider = Script::new([overloaded()]);
    let run = RunControl::default();
    let cancelled = cancel_after(&run, Duration::from_millis(50));
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_user_turn_streaming_events_with_media(
            &mut runtime,
            &provider,
            "hi".into(),
            Vec::new(),
            &run,
            |event| events.push(event.clone()),
        ),
    )
    .await
    .expect("cancellation ends the backoff");
    assert_ended_promptly(cancelled.await.unwrap());
    assert!(matches!(result, Err(AgentError::Cancelled)));
    assert_eq!(
        trace(&events),
        [
            "round started",
            r#"user "hi""#,
            "retry 1 of 3 in 60000ms: provider returned an invalid response: overloaded",
        ]
    );
    assert_eq!(store.load().unwrap(), runtime.session);
    assert_eq!(runtime.session.messages.len(), 1);
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test]
async fn thinking_is_streamed_but_not_mixed_into_answer_or_history() {
    struct ThinkingProvider;
    #[async_trait::async_trait]
    impl Provider for ThinkingProvider {
        async fn complete(
            &self,
            _request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            Ok(vec![
                StreamEvent::Thinking("summary".into()),
                StreamEvent::Text("answer".into()),
            ])
        }
    }
    let mut runtime = Runtime::new(
        SessionState::new("thinking-test", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let mut events = Vec::new();
    let answer = run_user_turn_streaming_events_with_media(
        &mut runtime,
        &ThinkingProvider,
        "hello".into(),
        Vec::new(),
        &RunControl::default(),
        |event| events.push(event.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        events
            .into_iter()
            .filter(|event| matches!(event, TurnEvent::Provider(_)))
            .collect::<Vec<_>>(),
        vec![
            TurnEvent::Provider(StreamEvent::Thinking("summary".into())),
            TurnEvent::Provider(StreamEvent::Text("answer".into()))
        ]
    );
    assert_eq!(answer, "answer");
    assert_eq!(runtime.session().messages[1].content, "answer");
    assert_eq!(runtime.session().messages[1].thinking, "summary");
    assert_eq!(
        provider_messages(&runtime.session().messages)[1].content,
        "answer"
    );
    let mut text = String::new();
    run_user_turn_streaming(&mut runtime, &ThinkingProvider, "again".into(), |delta| {
        text.push_str(delta)
    })
    .await
    .unwrap();
    assert_eq!(text, "answer");
}

#[tokio::test]
async fn run_user_turn_emits_provider_events_before_completion() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let saw_delta = Arc::new(AtomicBool::new(false));
    let provider = ObservingStreamingProvider {
        saw_delta: Arc::clone(&saw_delta),
    };

    let response = run_user_turn_streaming(&mut runtime, &provider, "hello".to_string(), |delta| {
        if delta == "early" {
            saw_delta.store(true, AtomicOrdering::SeqCst);
        }
    })
    .await
    .expect("run streaming turn");

    assert_eq!(response, "early done");
    assert_eq!(runtime.session().messages[1].content, response);
}

#[tokio::test]
async fn continuation_reruns_replay_safe_calls_and_reports_the_rest_as_interrupted() {
    struct ContinuationProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }
    #[async_trait::async_trait]
    impl Provider for ContinuationProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            if requests.len() == 1 {
                Ok(vec![StreamEvent::ToolCall {
                    id: "new-read".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"new.txt"}"#.into(),
                }])
            } else {
                Ok(vec![StreamEvent::Text("continued answer".into())])
            }
        }
    }
    let cwd = std::env::temp_dir().join(format!("noel-continuation-{}", new_session_id()));
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("new.txt"), "new result").unwrap();
    fs::write(cwd.join("again.txt"), "read again").unwrap();
    let (store, state) = SessionStore::create(&cwd.join("sessions"), cwd.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    let message = |role, content: &str, tool_call_id, tool_calls| ConversationMessage {
        role,
        content: content.into(),
        thinking: String::new(),
        media: Vec::new(),
        tool_call_id,
        tool_name: None,
        tool_calls,
        aborted: false,
    };
    let call = |id: &str, name: &str, arguments: &str| ChatToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    };
    let calls = vec![
        call(
            "saved",
            "write",
            r#"{"path":"saved.txt","content":"must not replay"}"#,
        ),
        call(
            "missing",
            "write",
            r#"{"path":"missing.txt","content":"uncertain side effect"}"#,
        ),
        call("again", "read", r#"{"path":"again.txt"}"#),
    ];
    let mut user = message(MessageRole::User, "finish the work", None, vec![]);
    user.media.push(MediaInput {
        mime_type: "image/png".into(),
        data_base64: "aW1hZ2U=".into(),
        path: None,
        width: Some(1),
        height: Some(1),
    });
    runtime.push_message(user).unwrap();
    runtime
        .push_message(message(MessageRole::Assistant, "working", None, calls))
        .unwrap();
    runtime
        .push_tool_event(ToolEvent {
            id: "saved".into(),
            name: "write".into(),
            result: "saved write result".into(),
            is_error: false,
        })
        .unwrap();
    runtime
        .push_message(message(
            MessageRole::Tool,
            "saved write result",
            Some("saved".into()),
            vec![],
        ))
        .unwrap();
    runtime.queue_message("later".into()).unwrap();
    runtime
        .set_active_model(Some(ModelRef {
            provider: "faux".into(),
            id: "echo".into(),
        }))
        .unwrap();
    runtime.set_active_account(Some("work".into())).unwrap();
    runtime
        .set_active_thinking_level(Some("off".into()))
        .unwrap();
    let before = runtime.session().clone();
    let (store, state) = SessionStore::open(path.clone()).unwrap();
    let mut reopened = Runtime::with_store(state, ReloadableSystems::default(), store);
    let pending = reopened.session().pending_tool_calls().unwrap().1;
    assert_eq!(
        pending.iter().map(|call| &call.id).collect::<Vec<_>>(),
        ["missing", "again"]
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ContinuationProvider {
        requests: requests.clone(),
    };
    let mut events = Vec::new();
    let answer =
        continue_turn_streaming_events(&mut reopened, &provider, &RunControl::default(), |event| {
            events.push(event.clone())
        })
        .await
        .unwrap();
    assert_eq!(answer, "continued answer");
    assert_eq!(
        &reopened.session().messages[..before.messages.len()],
        before.messages.as_slice()
    );
    assert_eq!(
        reopened
            .session()
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count(),
        1
    );
    assert_eq!(
        reopened.session().queued_messages(),
        before.queued_messages()
    );
    assert_eq!(reopened.session().active_model, before.active_model);
    assert_eq!(reopened.session().active_account, before.active_account);
    assert_eq!(
        reopened.session().active_thinking_level,
        before.active_thinking_level
    );
    // The interrupted writes never run again; the read does, in the
    // round its saved reply started.
    assert!(!cwd.join("saved.txt").exists());
    assert!(!cwd.join("missing.txt").exists());
    assert_eq!(
        trace(&events[..5]),
        [
            r#"round resumed: assistant "working" -> saved -> missing -> again"#,
            r#"tool again read {"path":"again.txt"}"#,
            r#"output again "read again""#,
            "finished read",
            "round finished",
        ]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TurnEvent::ToolStarted { .. }))
            .count(),
        2
    );
    // The interrupted write's result is saved too, unreported: it had
    // saved no output.
    assert_eq!(reopened.session().tool_history.len(), 4);
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].messages[0].media, before.messages[0].media);
    assert_eq!(captured[0].messages[2].content, "saved write result");
    assert_eq!(
        captured[0].messages[3].tool_call_id.as_deref(),
        Some("missing")
    );
    assert_eq!(
        captured[0].messages[3].content,
        "<harness>\n[error] Tool write was interrupted and may have partially run\n</harness>"
    );
    assert_eq!(
        captured[0].messages[4].tool_call_id.as_deref(),
        Some("again")
    );
    assert_eq!(captured[0].messages[4].content, "read again");
    assert_eq!(captured[1].messages.last().unwrap().content, "new result");
    assert!(!reopened.session().has_unfinished_turn());
    assert_eq!(SessionStore::open(path).unwrap().1, *reopened.session());
    // A completed continuation is a no-op if called again.
    assert_eq!(
        continue_turn_streaming_events(&mut reopened, &provider, &RunControl::default(), |_| {})
            .await
            .unwrap(),
        ""
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn continuation_failure_keeps_saved_context_and_queue_without_retry_chain() {
    let mut runtime = Runtime::new(
        SessionState::new("pending", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    runtime.systems.retry.enabled = false;
    runtime
        .push_message(ConversationMessage {
            role: MessageRole::User,
            content: "unfinished prompt".into(),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        })
        .unwrap();
    runtime.queue_message("pending queue".into()).unwrap();
    let before = runtime.session().clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = OverflowProvider {
        attempts: attempts.clone(),
    };
    assert!(continue_turn_streaming_events(
        &mut runtime,
        &provider,
        &RunControl::default(),
        |_| {}
    )
    .await
    .is_err());
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(runtime.session(), &before);
}

#[tokio::test]
async fn provider_round_limit_preserves_tool_results_and_can_continue() {
    struct RepeatedReadProvider {
        steering: SteeringMailbox,
    }
    #[async_trait::async_trait]
    impl Provider for RepeatedReadProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let round = request
                .messages
                .iter()
                .filter(|message| message.role == ChatRole::Tool)
                .count();
            self.steering.send("keep working".into());
            Ok(vec![StreamEvent::ToolCall {
                id: format!("read-{round}"),
                name: "read".into(),
                arguments: r#"{"path":"fixture.txt"}"#.into(),
            }])
        }
    }
    let cwd = std::env::temp_dir().join(format!("noel-round-limit-{}", new_session_id()));
    fs::create_dir_all(&cwd).unwrap();
    fs::write(cwd.join("fixture.txt"), "saved result").unwrap();
    let (store, state) = SessionStore::create(&cwd.join("sessions"), cwd.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    runtime.queue_message("pending follow-up".into()).unwrap();
    let steering = SteeringMailbox::default();
    let error = run_user_turn_streaming_with_media(
        &mut runtime,
        &RepeatedReadProvider {
            steering: steering.clone(),
        },
        "read repeatedly".into(),
        Vec::new(),
        &steering,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        AgentError::RoundLimitReached { max_rounds: 200 }
    ));
    assert!(error.to_string().contains("provider-round limit"));
    assert!(error.to_string().contains("agent.maxToolRounds"));
    assert!(error.to_string().contains("send a prompt to continue"));
    assert_eq!(
        runtime.session().tool_history.len(),
        DEFAULT_MAX_TOOL_ROUNDS
    );
    assert_eq!(
        runtime
            .session()
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .count(),
        DEFAULT_MAX_TOOL_ROUNDS
    );
    assert_eq!(
        runtime
            .session()
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        DEFAULT_MAX_TOOL_ROUNDS
    );
    let last = runtime.session().messages.last().unwrap();
    assert_eq!(last.role, MessageRole::Tool);
    assert_eq!(last.content, "saved result");
    assert_eq!(runtime.session().queued_messages(), ["pending follow-up"]);
    let (store, state) = SessionStore::open(path).unwrap();
    assert_eq!(runtime.session(), &state);
    let mut reopened = Runtime::with_store(state, ReloadableSystems::default(), store);
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".into(),
            id: "echo".into(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });
    let answer = continue_turn_streaming_events(
        &mut reopened,
        provider.as_ref(),
        &RunControl::default(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(answer, "[faux/echo] keep working");
    assert_eq!(
        reopened
            .session()
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        DEFAULT_MAX_TOOL_ROUNDS
    );
    assert_eq!(
        reopened.session().tool_history.len(),
        DEFAULT_MAX_TOOL_ROUNDS
    );
    assert_eq!(reopened.session().queued_messages(), ["pending follow-up"]);
    fs::remove_dir_all(cwd).unwrap();
}

#[test]
fn agent_settings_override_tool_round_limit() {
    assert_eq!(effective_max_tool_rounds(None), DEFAULT_MAX_TOOL_ROUNDS);
    assert_eq!(effective_max_tool_rounds(Some(3)), 3);
    // Zero or nonsensical values clamp to 1 instead of disabling the cap.
    assert_eq!(effective_max_tool_rounds(Some(0)), 1);

    let cwd = std::env::temp_dir().join(format!("noel-round-limit-{}", new_session_id()));
    fs::create_dir_all(&cwd).unwrap();
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    assert_eq!(runtime.systems.max_tool_rounds, DEFAULT_MAX_TOOL_ROUNDS);
    let agent_dir = cwd.join("agent");
    let config = LoadedConfig {
        paths: noel_config::ConfigPaths {
            cwd: cwd.clone(),
            session_dir: agent_dir.join("sessions"),
            settings_path: agent_dir.join("settings.json"),
            project_settings_path: cwd.join(".noel/settings.json"),
            auth_path: agent_dir.join("auth.json"),
            models_path: agent_dir.join("models.json"),
            model_cache_path: agent_dir.join("model-cache.json"),
            keybindings_path: agent_dir.join("keybindings.json"),
            agent_dir,
        },
        settings: noel_config::Settings {
            agent: Some(noel_config::AgentSettings {
                max_tool_rounds: Some(120),
            }),
            ..Default::default()
        },
        auth: Default::default(),
        models: Vec::new(),
        model_scope: Vec::new(),
        image_models: Vec::new(),
        keybindings: Vec::new(),
        context_files: Vec::new(),
        extensions: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
        mcp_servers: Vec::new(),
        prompt_templates: Vec::new(),
        themes: Vec::new(),
        diagnostics: Vec::new(),
        system_prompt: None,
        append_system_prompt: Vec::new(),
    };
    let next = ReloadableSystems::from_config(&config, 1);
    assert_eq!(next.max_tool_rounds, 120);
    runtime.reload(next).unwrap();
    assert_eq!(runtime.systems.max_tool_rounds, 120);
    fs::remove_dir_all(cwd).unwrap();
}

#[tokio::test]
async fn steering_messages_join_the_conversation_mid_turn() {
    let cwd = std::env::temp_dir().join(format!("noel-steering-loop-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ToolLoopProvider {
        requests: requests.clone(),
    };
    let steering = SteeringMailbox::default();
    steering.send("steer one".to_string());
    steering.send("steer two".to_string());

    let response = run_user_turn_streaming_with_media(
        &mut runtime,
        &provider,
        "read the file".to_string(),
        Vec::new(),
        &steering,
        |_| {},
    )
    .await
    .expect("run steered tool loop");

    assert_eq!(response, "done");
    // A queued burst is injected as one combined user message per round,
    // not one message per steering send.
    let captured = requests.lock().expect("requests").clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].messages[0].content, "read the file");
    assert_eq!(
        captured[0].messages[1].content,
        format!("steer one{STEERING_JOIN}steer two")
    );
    assert_eq!(captured[1].messages[3].role, ChatRole::Tool);
    // Both steers were injected before round 1 as one combined message,
    // so round 2 carries that same history (user prompt + combined
    // steer) plus the tool result — no second steering message appears.
    assert_eq!(
        captured[1]
            .messages
            .iter()
            .filter(|message| message.role == ChatRole::User)
            .count(),
        2
    );

    let roles = runtime
        .session()
        .messages
        .iter()
        .map(|message| message.role.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Assistant,
        ]
    );
    assert_eq!(
        runtime.session().messages[1].content,
        format!("steer one{STEERING_JOIN}steer two")
    );

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn steering_sent_during_the_final_response_keeps_the_turn_alive() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let steering = SteeringMailbox::default();
    let provider = SteeringProvider {
        requests: requests.clone(),
        steering: steering.clone(),
    };

    let response = run_user_turn_streaming_with_media(
        &mut runtime,
        &provider,
        "hello".to_string(),
        Vec::new(),
        &steering,
        |_| {},
    )
    .await
    .expect("run steered turn");

    assert_eq!(response, "donedone");
    let captured = requests.lock().expect("requests").clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[1].messages[2].content, "late steer");
    let contents = runtime
        .session()
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(contents, ["hello", "done", "late steer", "done"]);
}

#[tokio::test]
async fn steering_messages_are_combined_into_one_user_message() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });
    let steering = SteeringMailbox::new();
    steering.send("one".to_string());
    steering.send("two".to_string());

    run_user_turn_streaming_with_media(
        &mut runtime,
        provider.as_ref(),
        "hello".to_string(),
        Vec::new(),
        &steering,
        |_| {},
    )
    .await
    .expect("run steered turn");

    let contents = runtime
        .session()
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(contents[0], "hello");
    assert_eq!(contents[1], format!("one{STEERING_JOIN}two"));
    assert_eq!(steering.pending(), 0);
}

#[tokio::test]
async fn run_user_turn_executes_model_tool_calls_and_continues() {
    let cwd = std::env::temp_dir().join(format!("noel-model-tool-loop-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ToolLoopProvider {
        requests: requests.clone(),
    };

    let response = run_user_turn(&mut runtime, &provider, "read the file".to_string())
        .await
        .expect("run tool loop");

    assert_eq!(response, "done");
    assert_eq!(runtime.session().tool_history.len(), 1);
    assert_eq!(runtime.session().tool_history[0].id, "call_read_1");
    assert_eq!(runtime.session().tool_history[0].name, "read");
    assert_eq!(runtime.session().tool_history[0].result, "file contents");
    assert_eq!(runtime.session().messages.len(), 4);
    assert_eq!(runtime.session().messages[1].tool_calls[0].name, "read");
    assert_eq!(
        runtime.session().messages[2].tool_call_id.as_deref(),
        Some("call_read_1")
    );

    let captured = requests.lock().expect("requests").clone();
    assert_eq!(captured.len(), 2);
    assert!(captured[0].tools.iter().any(|tool| tool.name == "read"));
    assert_eq!(captured[1].messages[1].tool_calls[0].id, "call_read_1");
    assert_eq!(captured[1].messages[2].role, ChatRole::Tool);
    assert_eq!(
        captured[1].messages[2].tool_call_id.as_deref(),
        Some("call_read_1")
    );

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn plugin_tools_run_through_their_host_and_survive_a_reload() {
    struct Echo;
    impl plugins::PluginToolHost for Echo {
        fn call(
            &self,
            plugin: &str,
            tool: &str,
            args: Value,
        ) -> std::pin::Pin<Box<dyn Future<Output = plugins::PluginToolOutput> + Send>> {
            let output = format!("{plugin}/{tool}: {}", args["text"].as_str().unwrap_or(""));
            Box::pin(async move {
                plugins::PluginToolOutput {
                    output,
                    is_error: false,
                }
            })
        }
    }
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    runtime.set_plugin_tools(
        Arc::new(Echo),
        vec![plugins::PluginTool {
            plugin: "sample".into(),
            name: "shout".into(),
            description: "Shouts.".into(),
            parameters: serde_json::json!({"type": "object"}),
            replay_safe: false,
        }],
    );
    let call = ChatToolCall {
        id: "call_plugin_1".to_string(),
        name: "shout".to_string(),
        arguments: serde_json::json!({"text": "hi"}).to_string(),
    };
    execute_model_tool_call(&mut runtime, &unused_provider(), &call, &mut |_| {})
        .await
        .expect("the plugin tool runs");
    let event = runtime.session().tool_history.last().expect("tool event");
    assert_eq!(event.result, "sample/shout: hi");
    // A reload keeps the plugin's tools offered.
    runtime
        .reload(ReloadableSystems {
            config_generation: 1,
            ..ReloadableSystems::default()
        })
        .unwrap();
    assert_eq!(
        runtime.systems.extension_tools["shout"].protocol,
        ExtensionToolProtocol::Plugin
    );
}

#[tokio::test]
async fn todo_tool_call_updates_session_todos() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let tool_call = ChatToolCall {
        id: "call_todo_1".to_string(),
        name: "todo".to_string(),
        arguments: serde_json::json!({
            "todos": [
                {"content": "first", "status": "completed"},
                {"content": "second", "status": "in_progress"},
                {"content": "third", "status": "pending"}
            ]
        })
        .to_string(),
    };

    execute_model_tool_call(&mut runtime, &unused_provider(), &tool_call, &mut |_| {})
        .await
        .expect("execute todo tool");

    assert_eq!(runtime.session().todos.len(), 3);
    assert_eq!(runtime.session().todos[1].status, TodoStatus::InProgress);
    let event = runtime.session().tool_history.last().expect("tool event");
    assert_eq!(event.name, "todo");
    assert_eq!(event.result, "1. [x] first\n2. [~] second\n3. [ ] third");
}

#[tokio::test]
async fn todo_tool_call_rejects_invalid_arguments() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems::default(),
    );
    let tool_call = ChatToolCall {
        id: "call_todo_2".to_string(),
        name: "todo".to_string(),
        arguments: "{\"todos\": \"not-a-list\"}".to_string(),
    };

    execute_model_tool_call(&mut runtime, &unused_provider(), &tool_call, &mut |_| {})
        .await
        .expect("execute todo tool");

    assert!(runtime.session().todos.is_empty());
    let event = runtime.session().tool_history.last().expect("tool event");
    assert!(event.result.contains("invalid todo arguments"));
}

#[test]
fn session_store_restores_todos_on_load() {
    let base = std::env::temp_dir().join(format!("noel-session-todos-test-{}", new_session_id()));
    let (store, _state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    store
        .record_todos(vec![TodoItem {
            content: "task".to_string(),
            status: TodoStatus::Pending,
        }])
        .expect("record todos");

    let loaded = store.load().expect("load session");

    assert_eq!(
        loaded.todos,
        vec![TodoItem {
            content: "task".to_string(),
            status: TodoStatus::Pending,
        }]
    );
    let _ = fs::remove_dir_all(base);
}

#[test]
fn disabled_tools_persist_and_derive_active_tools_on_load() {
    let base = std::env::temp_dir().join(format!("noel-disabled-tools-test-{}", new_session_id()));
    let (store, state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    assert_eq!(state.active_tool_names, default_active_tool_names());
    assert!(state.disabled_tools.is_empty());

    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());
    runtime
        .set_disabled_tools(BTreeSet::from(["write".to_string()]))
        .expect("disable write");

    let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("reopen");

    assert_eq!(loaded.disabled_tools, BTreeSet::from(["write".to_string()]));
    assert!(!loaded.active_tool_names.contains("write"));
    assert_eq!(
        loaded.active_tool_names,
        default_active_tool_names()
            .difference(&BTreeSet::from(["write".to_string()]))
            .cloned()
            .collect::<BTreeSet<_>>()
    );
    let _ = fs::remove_dir_all(base);
}

#[test]
fn fresh_session_enables_all_builtin_tools() {
    let state = SessionState::new("session-1", PathBuf::from("/repo"));
    assert!(state.disabled_tools.is_empty());
    for definition in builtin_tool_definitions() {
        assert!(state.active_tool_names.contains(&definition.name));
    }
}

#[test]
fn unknown_record_types_are_skipped_on_load() {
    let base = std::env::temp_dir().join(format!("noel-unknown-record-test-{}", new_session_id()));
    fs::create_dir_all(&base).expect("create session dir");
    let path = base.join("session-1.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"started\",\"session_id\":\"session-1\",\"cwd\":\"/repo\"}\n",
            "{\"type\":\"active_tools\",\"tools\":[\"read\"]}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"hi\",",
            "\"media\":[],\"tool_call_id\":null,\"tool_name\":null,\"tool_calls\":[]}}\n",
        ),
    )
    .expect("write session");

    let (_store, loaded) = SessionStore::open(path).expect("open session");

    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].content, "hi");
    assert_eq!(loaded.active_tool_names, default_active_tool_names());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn todo_list_formats_checklist() {
    assert_eq!(format_todo_list(&[]), "no todos");
    assert_eq!(
        format_todo_list(&[
            TodoItem {
                content: "a".to_string(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "b".to_string(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "c".to_string(),
                status: TodoStatus::Pending,
            },
        ]),
        "1. [x] a\n2. [~] b\n3. [ ] c"
    );
}

#[tokio::test]
async fn write_tool_call_tracks_edited_files() {
    let cwd = std::env::temp_dir().join(format!("noel-edited-files-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let tool_call = ChatToolCall {
        id: "call_write_1".to_string(),
        name: "write".to_string(),
        arguments: serde_json::json!({
            "path": "src/new.rs",
            "content": "fn main() {}\n"
        })
        .to_string(),
    };

    execute_model_tool_call(&mut runtime, &unused_provider(), &tool_call, &mut |_| {})
        .await
        .expect("execute write tool");
    execute_model_tool_call(&mut runtime, &unused_provider(), &tool_call, &mut |_| {})
        .await
        .expect("execute write tool again");

    assert_eq!(runtime.session().edited_files, ["src/new.rs".to_string()]);
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn write_and_edit_accept_empty_text_and_edits_as_a_json_string() {
    let cwd = std::env::temp_dir().join(format!("noel-edit-args-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let call = |id: &str, name: &str, arguments: Value| ChatToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    };

    let calls = [
        call(
            "write_empty",
            "write",
            serde_json::json!({"path": "empty.txt", "content": ""}),
        ),
        call(
            "write_code",
            "write",
            serde_json::json!({"path": "a.rs", "content": "keep\ndrop\nlast\n"}),
        ),
        // Some models send `edits` as a JSON string; an empty newText
        // deletes the matched text.
        call(
            "edit_string",
            "edit",
            serde_json::json!({
                "path": "a.rs",
                "edits": r#"[{"oldText":"drop\n","newText":""},{"oldText":"last","newText":"end"}]"#
            }),
        ),
    ];
    for tool_call in &calls {
        execute_model_tool_call(&mut runtime, &unused_provider(), tool_call, &mut |_| {})
            .await
            .expect("execute tool call");
    }

    for event in &runtime.session().tool_history {
        assert!(!event.is_error, "{}: {}", event.id, event.result);
    }
    assert_eq!(fs::read_to_string(cwd.join("empty.txt")).unwrap(), "");
    assert_eq!(fs::read_to_string(cwd.join("a.rs")).unwrap(), "keep\nend\n");
    let _ = fs::remove_dir_all(cwd);
}

#[test]
fn session_store_restores_the_codemode_store_on_load() {
    let base =
        std::env::temp_dir().join(format!("noel-session-codemode-test-{}", new_session_id()));
    let (store, _state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    let mut values = BTreeMap::new();
    values.insert("seen".to_string(), serde_json::json!(["a", 1]));
    store
        .record_codemode_store(values.clone())
        .expect("record codemode store");

    let loaded = store.load().expect("load session");

    assert_eq!(loaded.codemode_store, values);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn session_store_restores_edited_files_on_load() {
    let base = std::env::temp_dir().join(format!("noel-session-edited-test-{}", new_session_id()));
    let (store, _state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    store
        .record_edited_files(vec!["src/new.rs".to_string()])
        .expect("record edited files");

    let loaded = store.load().expect("load session");

    assert_eq!(loaded.edited_files, ["src/new.rs".to_string()]);
    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn user_write_slash_command_tracks_edited_files() {
    let cwd = std::env::temp_dir().join(format!("noel-user-write-track-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });

    run_user_turn(
        &mut runtime,
        provider.as_ref(),
        "/write note.txt hi".to_string(),
    )
    .await
    .expect("run write command");

    assert_eq!(runtime.session().edited_files, ["note.txt".to_string()]);
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn unavailable_model_tool_call_returns_tool_error_without_execution() {
    let cwd = std::env::temp_dir().join(format!(
        "noel-unavailable-model-tool-test-{}",
        new_session_id()
    ));
    fs::create_dir_all(&cwd).expect("create temp dir");
    fs::write(cwd.join("a.txt"), "file contents").expect("write fixture");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems {
            available_tool_names: BTreeSet::from(["bash".to_string()]),
            ..ReloadableSystems::default()
        },
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ToolLoopProvider {
        requests: requests.clone(),
    };

    let response = run_user_turn(&mut runtime, &provider, "read the file".to_string())
        .await
        .expect("run tool loop");

    assert_eq!(response, "done");
    assert_eq!(
        runtime.session().tool_history[0].result,
        "Tool read not found"
    );
    assert_eq!(runtime.session().messages[2].content, "Tool read not found");
    let captured = requests.lock().expect("requests").clone();
    assert!(!captured[0].tools.iter().any(|tool| tool.name == "read"));
    assert!(captured[0].tools.iter().any(|tool| tool.name == "bash"));

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn failing_model_tool_call_is_recorded_as_tool_output() {
    let cwd =
        std::env::temp_dir().join(format!("noel-failing-model-tool-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = FailingToolLoopProvider {
        requests: requests.clone(),
    };

    let response = run_user_turn(&mut runtime, &provider, "read missing".to_string())
        .await
        .expect("run tool loop");

    assert_eq!(response, "done");
    assert_eq!(runtime.session().tool_history.len(), 1);
    assert_eq!(runtime.session().tool_history[0].id, "call_read_bad");
    assert_eq!(runtime.session().tool_history[0].name, "read");
    let failure = format!(
        "ENOENT: no such file or directory, access '{}'",
        cwd.join("missing.txt").display()
    );
    assert_eq!(runtime.session().tool_history[0].result, failure);
    assert!(runtime.session().tool_history[0].is_error);
    assert_eq!(
        runtime.session().messages[2].tool_call_id.as_deref(),
        Some("call_read_bad")
    );
    assert_eq!(runtime.session().messages[2].content, failure);

    let captured = requests.lock().expect("requests").clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[1].messages[2].role, ChatRole::Tool);
    assert_eq!(
        captured[1].messages[2].tool_call_id.as_deref(),
        Some("call_read_bad")
    );

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn tool_calls_from_a_reply_cut_off_at_the_output_limit_are_not_run() {
    struct TruncatedReplyProvider {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    #[async_trait::async_trait]
    impl Provider for TruncatedReplyProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut requests = self.requests.lock().expect("requests");
            requests.push(request);
            if requests.len() > 1 {
                return Ok(vec![StreamEvent::Text("done".to_string())]);
            }
            // The write arguments parse but were cut off mid-content.
            Ok(vec![
                StreamEvent::ToolCall {
                    id: "call_write_cut".to_string(),
                    name: "write".to_string(),
                    arguments: r#"{"path":"a.txt","content":"partial"}"#.to_string(),
                },
                StreamEvent::Stop {
                    reason: "max_tokens".to_string(),
                },
            ])
        }
    }

    let cwd = std::env::temp_dir().join(format!("noel-truncated-tool-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = TruncatedReplyProvider {
        requests: requests.clone(),
    };

    let response = run_user_turn(&mut runtime, &provider, "write a.txt".to_string())
        .await
        .expect("run tool loop");

    assert_eq!(response, "done");
    assert!(!cwd.join("a.txt").exists(), "truncated write must not run");
    let event = &runtime.session().tool_history[0];
    assert_eq!(event.id, "call_write_cut");
    assert!(event.is_error);
    assert!(
        event.result.contains("output token limit"),
        "{}",
        event.result
    );
    let captured = requests.lock().expect("requests").clone();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[1].messages[2].role, ChatRole::Tool);
    assert_eq!(captured[1].messages[2].content, event.result);

    let _ = fs::remove_dir_all(cwd);
}

#[test]
fn provider_messages_repair_missing_tool_outputs_from_saved_sessions() {
    let messages = vec![
        ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "question".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        },
        ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Assistant,
            content: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![ChatToolCall {
                id: "call_missing".to_string(),
                name: "read".to_string(),
                arguments: r#"{"path":"."}"#.to_string(),
            }],
            aborted: false,
        },
        ConversationMessage {
            thinking: String::new(),
            role: MessageRole::User,
            content: "continue".to_string(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        },
    ];

    let request_messages = provider_messages(&messages);

    assert_eq!(request_messages.len(), 4);
    assert_eq!(request_messages[1].role, ChatRole::Assistant);
    assert_eq!(request_messages[2].role, ChatRole::Tool);
    assert_eq!(
        request_messages[2].tool_call_id.as_deref(),
        Some("call_missing")
    );
    assert!(request_messages[2].content.contains("did not complete"));
    assert_eq!(request_messages[3].content, "continue");
}

#[tokio::test]
async fn run_user_turn_executes_json_extension_model_tools() {
    let cwd = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join(format!(
            "noel-extension-tool-loop-test-{}",
            new_session_id()
        ));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let extension_path = cwd.join("ext-tool.sh");
    fs::write(
        &extension_path,
        "#!/bin/sh\ncat >/dev/null\nprintf '{\"output\":\"extension output\"}\\n'\n",
    )
    .expect("write extension");
    make_executable(&extension_path);
    fs::write(
        cwd.join("ext-tool.sh.noel-extension.json"),
        serde_json::json!({
            "protocol": "json",
            "tools": [{
                "name": "ext_echo",
                "description": "Echo through an extension.",
                "parameters": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            }]
        })
        .to_string(),
    )
    .expect("write manifest");
    let extensions = vec![ResourceFile {
        name: "ext-tool".to_string(),
        path: extension_path,
        content: String::new(),
    }];
    let extension_tools = extension_tools_from_resources(&extensions);
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems {
            available_tool_names: BTreeSet::from(["ext_echo".to_string()]),
            extension_tools,
            ..ReloadableSystems::default()
        },
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = ExtensionToolLoopProvider {
        requests: Arc::clone(&requests),
    };

    let response = run_user_turn(&mut runtime, &provider, "use extension".to_string())
        .await
        .expect("run extension tool loop");

    assert_eq!(response, "done");
    assert_eq!(runtime.session().tool_history[0].name, "ext_echo");
    assert_eq!(runtime.session().tool_history[0].result, "extension output");
    assert_eq!(runtime.session().messages[2].content, "extension output");
    let captured = requests.lock().expect("requests");
    assert!(captured[0].tools.iter().any(|tool| tool.name == "ext_echo"));
    assert_eq!(
        captured[1].messages[2].tool_name.as_deref(),
        Some("ext_echo")
    );

    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn v2_extension_host_serves_model_tool_calls_from_one_process() {
    let cwd = std::env::temp_dir().join(format!("noel-extension-v2-loop-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    // A stateful v2 host: init registers a counter tool; each tool call
    // increments and echoes the count, proving both calls hit one
    // process.
    let extension_path = cwd.join("counter.sh");
    fs::write(
            &extension_path,
            concat!(
                "#!/bin/sh\n",
                "count=0\n",
                "while read line; do\n",
                "  case \"$line\" in\n",
                "    *'\"kind\":\"init\"'*) printf '%s\\n' '{\"output\":\"ready\",\"tools\":[{\"name\":\"counter\",\"replaySafe\":true,\"parameters\":{\"type\":\"object\"}}]}';;\n",
                "    *) count=$((count+1)); printf '%s\\n' \"{\\\"output\\\":\\\"count is $count\\\"}\";;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .expect("write extension");
    make_executable(&extension_path);
    fs::write(
        cwd.join("counter.sh.noel-extension.json"),
        serde_json::json!({ "protocolVersion": 2 }).to_string(),
    )
    .expect("write manifest");
    let extensions = vec![ResourceFile {
        name: "counter".to_string(),
        path: extension_path.clone(),
        content: String::new(),
    }];
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems {
            extension_tools: extension_tools_from_resources(&extensions),
            extension_files: extensions,
            ..ReloadableSystems::default()
        },
    );
    // The provider calls the registered tool twice; the second call's
    // result proves the host process persisted.
    let calls = Arc::new(Mutex::new(Vec::new()));
    struct TwoCallProvider {
        calls: Arc<Mutex<Vec<ProviderRequest>>>,
    }
    #[async_trait::async_trait]
    impl Provider for TwoCallProvider {
        async fn complete(
            &self,
            request: ProviderRequest,
        ) -> Result<Vec<StreamEvent>, ProviderError> {
            let mut calls = self.calls.lock().expect("calls");
            let index = calls.len();
            calls.push(request);
            if index == 0 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_1".to_string(),
                        name: "counter".to_string(),
                        arguments: "{}".to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".to_string(),
                    },
                ]);
            }
            if index == 1 {
                return Ok(vec![
                    StreamEvent::ToolCall {
                        id: "call_2".to_string(),
                        name: "counter".to_string(),
                        arguments: "{}".to_string(),
                    },
                    StreamEvent::Stop {
                        reason: "toolUse".to_string(),
                    },
                ]);
            }
            Ok(vec![
                StreamEvent::Text("done".to_string()),
                StreamEvent::Stop {
                    reason: "stop".to_string(),
                },
            ])
        }
    }
    let provider = TwoCallProvider {
        calls: Arc::clone(&calls),
    };

    let response = run_user_turn(&mut runtime, &provider, "count twice".to_string())
        .await
        .expect("run v2 extension tool loop");

    assert_eq!(response, "done");
    let results: Vec<String> = runtime
        .session()
        .tool_history
        .iter()
        .map(|event| event.result.clone())
        .collect();
    assert_eq!(results, vec!["count is 1", "count is 2"]);
    // The handshake's tool reached the provider's declarations.
    let first = calls.lock().expect("calls");
    assert!(first[0].tools.iter().any(|tool| tool.name == "counter"));
    let _ = fs::remove_dir_all(cwd);
}

/// #159: a reload keeps the live v2 registrations offered, for the
/// extensions it still loads.
#[tokio::test]
async fn a_reload_keeps_the_live_registrations_of_loaded_extensions() {
    let cwd = std::env::temp_dir().join(format!("noel-extension-v2-reload-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create cwd");
    let extension_path = cwd.join("hello.sh");
    fs::write(
            &extension_path,
            concat!(
                "#!/bin/sh\n",
                "while read line; do\n",
                "  printf '%s\\n' '{\"output\":\"\",\"tools\":[{\"name\":\"hello\",\"description\":\"Say hello\"}]}'\n",
                "done\n",
            ),
        )
        .expect("write extension");
    make_executable(&extension_path);
    fs::write(
        cwd.join("hello.sh.noel-extension.json"),
        serde_json::json!({ "protocolVersion": 2 }).to_string(),
    )
    .expect("write manifest");
    let systems = |generation| ReloadableSystems {
        config_generation: generation,
        extension_files: vec![ResourceFile {
            name: "hello".to_string(),
            path: extension_path.clone(),
            content: String::new(),
        }],
        ..ReloadableSystems::default()
    };
    let mut runtime = Runtime::new(SessionState::new("session-1", cwd.clone()), systems(0));
    runtime.start_extension_hosts().await;
    assert!(runtime.systems.extension_tools.contains_key("hello"));
    runtime.reload(systems(1)).expect("reload");
    assert!(runtime.systems.extension_tools.contains_key("hello"));
    runtime
        .reload(ReloadableSystems {
            config_generation: 2,
            ..ReloadableSystems::default()
        })
        .expect("reload without it");
    assert!(!runtime.systems.extension_tools.contains_key("hello"));
    let hosts = runtime.extension_hosts();
    hosts.lock().await.shutdown(&cwd).await;
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn v2_extension_gate_blocks_tool_calls_fail_closed() {
    let cwd = std::env::temp_dir().join(format!("noel-extension-v2-gate-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    // A gate extension: init registers no tools but takes the gate; it
    // blocks every call whose input names "secret" and allows the rest.
    let extension_path = cwd.join("gate.sh");
    fs::write(
            &extension_path,
            concat!(
                "#!/bin/sh\n",
                "while read line; do\n",
                "  case \"$line\" in\n",
                "    *'\"kind\":\"init\"'*) printf '%s\\n' '{\"output\":\"\",\"tools\":[],\"toolGate\":true}'; continue;;\n",
                "  esac\n",
                "  case \"$line\" in\n",
                "    *secret*) printf '%s\\n' '{\"allow\":false,\"reason\":\"secret path is protected\"}';;\n",
                "    *) printf '%s\\n' '{\"allow\":true}';;\n",
                "  esac\n",
                "done\n",
            ),
        )
        .expect("write extension");
    make_executable(&extension_path);
    fs::write(
        cwd.join("gate.sh.noel-extension.json"),
        serde_json::json!({ "protocolVersion": 2 }).to_string(),
    )
    .expect("write manifest");
    let extensions = vec![ResourceFile {
        name: "gate".to_string(),
        path: extension_path,
        content: String::new(),
    }];
    let mut runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems {
            extension_files: extensions,
            ..ReloadableSystems::default()
        },
    );
    runtime.start_extension_hosts().await;
    let gate = {
        let hosts = runtime.extension_hosts();
        let mut hosts = hosts.lock().await;
        hosts
            .gate_tool_call("read", r#"{"path":"secret.txt"}"#, &cwd)
            .await
    };
    assert_eq!(
        gate,
        Ok(extensions::GateDecision::blocked(
            "secret path is protected".to_string()
        ))
    );
    let allow = {
        let hosts = runtime.extension_hosts();
        let mut hosts = hosts.lock().await;
        hosts
            .gate_tool_call("read", r#"{"path":"public.txt"}"#, &cwd)
            .await
    };
    assert_eq!(allow, Ok(extensions::GateDecision::allow()));
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn provider_retry_does_not_duplicate_user_message() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems {
            retry: RuntimeRetrySettings {
                enabled: true,
                max_retries: 2,
                base_delay_ms: 0,
                ..Default::default()
            },
            ..ReloadableSystems::default()
        },
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FlakyProvider {
        attempts: Arc::clone(&attempts),
        fail_before_success: 1,
    };

    let response = run_user_turn(&mut runtime, &provider, "hello".to_string())
        .await
        .expect("retry should recover");

    assert_eq!(response, "retried");
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(runtime.session().messages.len(), 2);
    assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    assert_eq!(runtime.session().messages[1].role, MessageRole::Assistant);
}

#[tokio::test]
async fn disabled_provider_retry_returns_first_attempt_failure() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems {
            retry: RuntimeRetrySettings {
                enabled: false,
                max_retries: 2,
                base_delay_ms: 0,
                ..Default::default()
            },
            ..ReloadableSystems::default()
        },
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = FlakyProvider {
        attempts: Arc::clone(&attempts),
        fail_before_success: 1,
    };

    let error = run_user_turn(&mut runtime, &provider, "hello".to_string())
        .await
        .expect_err("retry should be disabled");

    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::InvalidResponse(_))
    ));
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(runtime.session().messages.len(), 1);
    assert_eq!(runtime.session().messages[0].role, MessageRole::User);
}

#[tokio::test]
async fn provider_retry_skips_context_overflow_errors() {
    let mut runtime = Runtime::new(
        SessionState::new("session-1", PathBuf::from(".")),
        ReloadableSystems {
            retry: RuntimeRetrySettings {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 0,
                ..Default::default()
            },
            ..ReloadableSystems::default()
        },
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let provider = OverflowProvider {
        attempts: Arc::clone(&attempts),
    };

    let error = run_user_turn(&mut runtime, &provider, "hello".to_string())
        .await
        .expect_err("context overflow should not retry");

    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::InvalidResponse(_))
    ));
    assert_eq!(attempts.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(runtime.session().messages.len(), 1);
    assert_eq!(runtime.session().messages[0].role, MessageRole::User);
}

#[test]
fn queued_messages_are_persisted_and_clearable() {
    let base = std::env::temp_dir().join(format!("noel-queue-test-{}", new_session_id()));
    let (store, state) =
        SessionStore::create(&base, PathBuf::from("/repo")).expect("create session");
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());

    runtime
        .queue_message("first follow-up".to_string())
        .expect("queue message");
    runtime
        .queue_message("second follow-up".to_string())
        .expect("queue message");
    let cleared = runtime.clear_queued_messages().expect("clear queue");

    assert_eq!(cleared, 2);
    assert!(runtime.session().queued_messages().is_empty());
    let (_store, loaded) = SessionStore::open(store.path().to_path_buf()).expect("open queue");
    assert!(loaded.queued_messages().is_empty());

    let _ = fs::remove_dir_all(base);
}

#[test]
fn the_saved_queue_keeps_steering_ahead_of_follow_ups() {
    let base = durable_test_dir("queue-kinds");
    let (store, _) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    store
        .record_queue(vec!["steer".into()], vec!["follow".into()])
        .unwrap();
    let last_line = |path: &Path| {
        let journal = fs::read_to_string(path).unwrap();
        journal.lines().last().unwrap().to_owned()
    };
    // An older noel reads the same record as two follow-ups.
    assert_eq!(
        last_line(&path),
        r#"{"type":"queued_messages_snapshot","messages":["steer","follow"],"steering":1}"#
    );
    let loaded = SessionStore::read(&path).unwrap();
    let queue = (vec!["steer".to_owned()], vec!["follow".to_owned()]);
    let saved = |state: &SessionState| {
        let (steering, follow_ups) = state.saved_queue();
        (steering.to_vec(), follow_ups.to_vec())
    };
    assert_eq!(saved(&loaded), queue);
    let (fork, _) = SessionStore::fork(&base, &loaded, false).unwrap();
    assert_eq!(saved(&SessionStore::read(fork.path()).unwrap()), queue);

    // The TUI queues everything as follow-ups, in the same record.
    let mut runtime = Runtime::with_store(loaded, ReloadableSystems::default(), store);
    runtime
        .replace_queued_messages(vec!["steer".into(), "follow".into()])
        .unwrap();
    assert_eq!(
        last_line(&path),
        r#"{"type":"queued_messages_snapshot","messages":["steer","follow"]}"#
    );
    assert_eq!(
        saved(&SessionStore::read(&path).unwrap()),
        (vec![], vec!["steer".to_owned(), "follow".to_owned()])
    );
    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn runs_are_saved_with_their_prompts_and_outlive_compaction_undo_and_restarts() {
    let base = durable_test_dir("runs");
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store.clone());
    let start = |id: &str, request_id: &str| RunStart {
        id: id.into(),
        request_id: Some(request_id.into()),
        kind: SubmissionKind::Prompt,
    };
    let run = |id: &str, request_id: &str, status| Run {
        id: id.into(),
        request_id: Some(request_id.into()),
        kind: SubmissionKind::Prompt,
        status,
        failure: None,
        restarts: 0,
        closed: false,
    };
    let runs = || SessionStore::read(&path).unwrap().runs;

    // The prompt and the run's start are one record: a load that sees
    // the run sees its prompt, and the run is interrupted until it ends.
    let prompt = runtime
        .start_run(start("run-1", "m-1"), "first".into(), Vec::new())
        .unwrap();
    assert_eq!(prompt, test_message(MessageRole::User, "first"));
    assert_eq!(
        runtime.session().runs,
        [run("run-1", "m-1", RunStatus::Running)]
    );
    let loaded = SessionStore::read(&path).unwrap();
    assert_eq!(message_texts(&loaded), ["first"]);
    assert_eq!(loaded.runs, [run("run-1", "m-1", RunStatus::Interrupted)]);
    runtime.finish_run("run-1", RunStatus::Completed).unwrap();
    assert_eq!(runs(), [run("run-1", "m-1", RunStatus::Completed)]);

    // A mid-turn compaction checkpoint and an undo snapshot rewrite the
    // messages, not the runs.
    runtime
        .start_run(start("run-2", "m-2"), "second".into(), Vec::new())
        .unwrap();
    let checkpoint = CompactionRecord {
        kind: CompactionKind::Automatic,
        omitted_messages: 1,
        retained_messages: 1,
        summary: "earlier".into(),
    };
    store
        .record_compaction_checkpoint(
            checkpoint,
            vec![
                test_message(MessageRole::Assistant, "summary"),
                test_message(MessageRole::User, "second"),
            ],
        )
        .unwrap();
    runtime.undo_last_turn().await.unwrap();
    assert_eq!(
        runs(),
        [
            run("run-1", "m-1", RunStatus::Completed),
            run("run-2", "m-2", RunStatus::Interrupted)
        ]
    );

    // After a restart, a new run first ends the interrupted one.
    let (store, state) = SessionStore::open(path.clone()).unwrap();
    assert_eq!(state.run_for_request("m-2").unwrap().id, "run-2");
    let mut restarted = Runtime::with_store(state, ReloadableSystems::default(), store);
    restarted
        .start_run(start("run-3", "m-3"), "third".into(), Vec::new())
        .unwrap();
    assert_eq!(
        runs(),
        [
            run("run-1", "m-1", RunStatus::Completed),
            run("run-2", "m-2", RunStatus::Aborted),
            run("run-3", "m-3", RunStatus::Interrupted)
        ]
    );

    // Runs stay with the session that ran them: forks and imports, new
    // sessions, start without any.
    let source = SessionStore::read(&path).unwrap();
    let (fork, forked) = SessionStore::fork(&base, &source, false).unwrap();
    assert!(forked.runs.is_empty());
    assert!(SessionStore::read(fork.path()).unwrap().runs.is_empty());
    let (_, imported) =
        SessionStore::import(&base.join("imported"), SessionExport::from(&source)).unwrap();
    assert!(imported.runs.is_empty());

    // Journals from before runs load without any.
    let old = base.join("old.jsonl");
    fs::write(&old, format!("{STARTED_LINE}{}\n", user_line("old prompt"))).unwrap();
    let loaded = SessionStore::read(&old).unwrap();
    assert_eq!(message_texts(&loaded), ["old prompt"]);
    assert!(loaded.runs.is_empty());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn replay_safe_tools_are_the_read_only_file_tools_and_marked_extension_tools() {
    let base = durable_test_dir("replay-safe");
    let extension = base.join("tools");
    fs::write(&extension, "").unwrap();
    fs::write(
        base.join("tools.noel-extension.json"),
        r#"{"protocol":"json","tools":[{"name":"lookup","replaySafe":true},{"name":"deploy"}]}"#,
    )
    .unwrap();
    let mut runtime = Runtime::new(
        SessionState::new("replay", base.clone()),
        ReloadableSystems::default(),
    );
    runtime.systems.extension_tools = extension_tools_from_resources(&[ResourceFile {
        name: "tools".into(),
        path: extension,
        content: String::new(),
    }]);
    for tool in ["read", "grep", "find", "ls", "lookup"] {
        assert!(runtime.replay_safe(tool), "{tool}");
    }
    for tool in [
        "bash",
        "edit",
        "write",
        "todo",
        "web_search",
        "web_fetch",
        "deploy",
    ] {
        assert!(!runtime.replay_safe(tool), "{tool}");
    }
    let _ = fs::remove_dir_all(base);
}

#[tokio::test]
async fn excluded_bash_does_not_enter_context() {
    let cwd = std::env::temp_dir().join(format!("noel-excluded-bash-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems::default(),
    );

    let output = run_excluded_bash(&runtime, "printf shell-ok".to_string())
        .await
        .expect("run excluded bash");

    assert_eq!(output, "shell-ok");
    assert!(runtime.session().messages.is_empty());
    assert!(runtime.session().tool_history.is_empty());
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn disabled_tool_command_is_rejected_before_execution() {
    let cwd = std::env::temp_dir().join(format!("noel-disabled-tool-test-{}", new_session_id()));
    fs::create_dir_all(&cwd).expect("create temp dir");
    let mut session = SessionState::new("session-1", cwd.clone());
    session.disabled_tools = BTreeSet::from(["write".to_string()]);
    session.active_tool_names = active_tools_with(&session.disabled_tools);
    let mut runtime = Runtime::new(session, ReloadableSystems::default());
    let provider = create_provider(ProviderConfig {
        model: ModelRef {
            provider: "faux".to_string(),
            id: "echo".to_string(),
        },
        api: ProviderApi::Faux,
        base_url: None,
        auth: ProviderAuth::None,
        thinking_level: None,
        thinking_budget_tokens: None,
        session_id: None,
        traits: noel_ai::ModelTraits::default(),
    });

    let error = run_user_turn(
        &mut runtime,
        provider.as_ref(),
        "/write blocked.txt nope".to_string(),
    )
    .await
    .expect_err("write tool should be disabled");

    assert!(matches!(error, AgentError::DisabledTool(tool) if tool == "write"));
    assert!(!cwd.join("blocked.txt").exists());
    let _ = fs::remove_dir_all(cwd);
}

#[test]
fn session_ids_are_unique_inside_one_process() {
    let first = new_session_id();
    let second = new_session_id();

    assert_ne!(first, second);
}

/// One line per event, for exact-order assertions. Tool durations vary,
/// so they are left out.
pub(crate) fn trace(events: &[TurnEvent]) -> Vec<String> {
    let flag = |is_error: bool| if is_error { " error" } else { "" };
    let message = |message: &ConversationMessage| {
        let role = format!("{:?}", message.role).to_lowercase();
        let calls = message
            .tool_calls
            .iter()
            .map(|call| format!(" -> {}", call.id))
            .collect::<String>();
        format!("{role} {:?}{calls}", message.content)
    };
    events
        .iter()
        .filter(|event| !matches!(event, TurnEvent::HistoryRewritten(_)))
        .map(|event| match event {
            TurnEvent::HistoryRewritten(_) => unreachable!("filtered"),
            TurnEvent::RoundStarted => "round started".to_string(),
            TurnEvent::Notice(text) => format!("notice {text:?}"),
            TurnEvent::RoundResumed(reply) => format!("round resumed: {}", message(reply)),
            TurnEvent::MessageAdded(saved) => message(saved),
            TurnEvent::Provider(StreamEvent::Text(text)) => format!("text {text:?}"),
            TurnEvent::Provider(StreamEvent::Thinking(text)) => format!("thinking {text:?}"),
            TurnEvent::Provider(StreamEvent::ToolCall { id, name, .. }) => {
                format!("call {id} {name}")
            }
            TurnEvent::Provider(StreamEvent::Usage {
                input_tokens,
                output_tokens,
            }) => format!("usage {input_tokens}/{output_tokens}"),
            TurnEvent::Provider(StreamEvent::Stop { reason }) => format!("stop {reason}"),
            TurnEvent::RetryStarted(notice) => format!(
                "retry {} of {} in {}ms: {}",
                notice.attempt, notice.max_attempts, notice.delay_ms, notice.error_message
            ),
            TurnEvent::RetryFinished { attempt, error } => format!(
                "retried {attempt}: {}",
                error.as_deref().unwrap_or("succeeded")
            ),
            TurnEvent::Overflowed { error } => format!(
                "overflowed: {}",
                error.as_deref().unwrap_or("reply cut off")
            ),
            TurnEvent::ToolStarted { call } => {
                format!("tool {} {} {}", call.id, call.name, call.arguments)
            }
            TurnEvent::ToolUpdate { call, output } => format!("update {} {output:?}", call.id),
            TurnEvent::NestedToolCalls { call, calls } => format!(
                "nested {} [{}]",
                call.id,
                calls
                    .iter()
                    .map(|nested| format!("{} {}", nested.name, nested.status.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TurnEvent::ToolOutput {
                call,
                output,
                is_error,
                ..
            } => format!("output {} {output:?}{}", call.id, flag(*is_error)),
            TurnEvent::ToolFinished { name, is_error } => {
                format!("finished {name}{}", flag(*is_error))
            }
            TurnEvent::TodosUpdated(todos) => format!("todos {}", todos.len()),
            TurnEvent::CompactionStarted => "compaction started".to_string(),
            TurnEvent::CompactionFinished(record) => {
                format!("compacted {} messages", record.omitted_messages)
            }
            TurnEvent::CompactionFailed(_) => "compaction failed".to_string(),
            TurnEvent::RoundFinished => "round finished".to_string(),
        })
        .collect()
}

type Reply = Box<dyn FnOnce() -> Result<Vec<StreamEvent>, ProviderError> + Send>;

fn reply(events: Vec<StreamEvent>) -> Reply {
    Box::new(move || Ok(events))
}

/// A failure a retry can fix.
fn overloaded() -> Reply {
    Box::new(|| Err(ProviderError::InvalidResponse("overloaded".into())))
}

/// Answers each request with the next scripted reply, which may also act
/// (steer, fail) when its request arrives.
struct Script(Mutex<VecDeque<Reply>>);

impl Script {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self(Mutex::new(replies.into_iter().collect()))
    }
}

#[async_trait::async_trait]
impl Provider for Script {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let reply = self.0.lock().unwrap().pop_front();
        reply.expect("a request the script did not expect")()
    }
}

fn tool_call(id: &str, name: &str, arguments: Value) -> StreamEvent {
    StreamEvent::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

fn stop(reason: &str) -> StreamEvent {
    StreamEvent::Stop {
        reason: reason.into(),
    }
}

struct CheckpointProvider;
#[async_trait::async_trait]
impl Provider for CheckpointProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        assert!(request.tools.is_empty());
        Ok(vec![StreamEvent::Text(
            "Goals and constraints: keep working. Next steps: test.".into(),
        )])
    }
}

struct FlakyProvider {
    attempts: Arc<AtomicUsize>,
    fail_before_success: usize,
}

struct ToolLoopProvider {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

struct SteeringProvider {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    steering: SteeringMailbox,
}

#[async_trait::async_trait]
impl Provider for SteeringProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut requests = self.requests.lock().expect("requests");
        let call_index = requests.len();
        requests.push(request);
        drop(requests);
        if call_index == 0 {
            self.steering.send("late steer".to_string());
        }
        Ok(vec![
            StreamEvent::Text("done".to_string()),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }
}

struct FailingToolLoopProvider {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

struct ExtensionToolLoopProvider {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

struct ObservingStreamingProvider {
    saw_delta: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl Provider for ObservingStreamingProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        unreachable!("streaming provider should use complete_streaming")
    }

    async fn complete_streaming(
        &self,
        _request: ProviderRequest,
        on_event: &mut (dyn FnMut(StreamEvent) -> Result<(), ProviderError> + Send),
    ) -> Result<(), ProviderError> {
        on_event(StreamEvent::Text("early".to_string()))?;
        assert!(
            self.saw_delta.load(AtomicOrdering::SeqCst),
            "text callback was not invoked before provider completion"
        );
        on_event(StreamEvent::Text(" done".to_string()))?;
        on_event(StreamEvent::Stop {
            reason: "stop".to_string(),
        })?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for ToolLoopProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut requests = self.requests.lock().expect("requests");
        let call_index = requests.len();
        requests.push(request);
        if call_index == 0 {
            return Ok(vec![
                StreamEvent::ToolCall {
                    id: "call_read_1".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                },
                StreamEvent::Stop {
                    reason: "toolUse".to_string(),
                },
            ]);
        }
        Ok(vec![
            StreamEvent::Text("done".to_string()),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }
}

#[async_trait::async_trait]
impl Provider for FailingToolLoopProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut requests = self.requests.lock().expect("requests");
        let call_index = requests.len();
        requests.push(request);
        if call_index == 0 {
            return Ok(vec![
                StreamEvent::ToolCall {
                    id: "call_read_bad".to_string(),
                    name: "read".to_string(),
                    arguments: r#"{"path":"missing.txt"}"#.to_string(),
                },
                StreamEvent::Stop {
                    reason: "toolUse".to_string(),
                },
            ]);
        }
        Ok(vec![
            StreamEvent::Text("done".to_string()),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }
}

#[async_trait::async_trait]
impl Provider for ExtensionToolLoopProvider {
    async fn complete(&self, request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut requests = self.requests.lock().expect("requests");
        let call_index = requests.len();
        requests.push(request);
        if call_index == 0 {
            return Ok(vec![
                StreamEvent::ToolCall {
                    id: "call_ext_1".to_string(),
                    name: "ext_echo".to_string(),
                    arguments: r#"{"text":"hello"}"#.to_string(),
                },
                StreamEvent::Stop {
                    reason: "toolUse".to_string(),
                },
            ]);
        }
        Ok(vec![
            StreamEvent::Text("done".to_string()),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod extension");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[async_trait::async_trait]
impl Provider for FlakyProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        let attempt = self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
        if attempt < self.fail_before_success {
            return Err(ProviderError::InvalidResponse("overloaded".to_string()));
        }
        Ok(vec![
            StreamEvent::Text("retried".to_string()),
            StreamEvent::Stop {
                reason: "stop".to_string(),
            },
        ])
    }
}

struct OverflowProvider {
    attempts: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for OverflowProvider {
    async fn complete(&self, _request: ProviderRequest) -> Result<Vec<StreamEvent>, ProviderError> {
        self.attempts.fetch_add(1, AtomicOrdering::SeqCst);
        Err(ProviderError::InvalidResponse(
            "Your input exceeds the context window of this model".to_string(),
        ))
    }
}

/// #175: a model switch saves the model and its thinking level in one
/// append, and a reopen reads both back.
#[test]
fn a_model_switch_saves_the_model_and_its_level_together() {
    let base = std::env::temp_dir().join(format!("noel-model-switch-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    let model = ModelRef {
        provider: "faux".into(),
        id: "faux-1".into(),
    };
    let before = fs::read(&path).unwrap();
    runtime
        .set_active_model_and_thinking_level(Some(model.clone()), Some("high".into()))
        .unwrap();
    let after = fs::read(&path).unwrap();
    let added = String::from_utf8(after[before.len()..].to_vec()).unwrap();
    let kinds: Vec<String> = added
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["type"].to_string())
        .collect();
    assert_eq!(kinds.len(), 2, "{added}");
    let (_, reopened) = SessionStore::open(path).unwrap();
    assert_eq!(reopened.active_model, Some(model));
    assert_eq!(reopened.active_thinking_level.as_deref(), Some("high"));
    let _ = fs::remove_dir_all(base);
}

/// #181: clones of one store appending from many threads at once leave a
/// journal of whole records, each once, that opens again.
#[test]
fn concurrent_appends_keep_every_record_whole() {
    let base = std::env::temp_dir().join(format!("noel-concurrent-{}", new_session_id()));
    let (store, _) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    let before = fs::read_to_string(&path).unwrap().lines().count();
    let threads = (0..8)
        .map(|thread| {
            let store = store.clone();
            std::thread::spawn(move || {
                for index in 0..25 {
                    let todos = vec![TodoItem {
                        content: format!("{thread}-{index}"),
                        status: TodoStatus::Pending,
                    }];
                    store.record_todos(todos).unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    let journal = fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = journal.lines().collect();
    assert_eq!(lines.len(), before + 8 * 25);
    for line in &lines {
        serde_json::from_str::<Value>(line).expect("a whole record");
    }
    let mut seen = BTreeSet::new();
    for line in &lines[before..] {
        let record: Value = serde_json::from_str(line).unwrap();
        assert!(seen.insert(record["todos"][0]["content"].as_str().unwrap().to_owned()));
    }
    drop(store);
    SessionStore::open(path).expect("the journal opens again");
    let _ = fs::remove_dir_all(base);
}

/// #181: what each setter journals replays to the same session: a session
/// changed through every runtime setter and a run opens again equal to it.
#[test]
fn a_journal_replays_every_change_its_session_made() {
    let base = std::env::temp_dir().join(format!("noel-replay-{}", new_session_id()));
    let (store, state) = SessionStore::create(&base, base.clone()).unwrap();
    let path = store.path().to_path_buf();
    let mut runtime = Runtime::with_store(state, ReloadableSystems::default(), store);
    runtime
        .set_active_model_and_thinking_level(
            Some(ModelRef {
                provider: "faux".into(),
                id: "faux-1".into(),
            }),
            Some("high".into()),
        )
        .unwrap();
    runtime.set_active_account(Some("work".into())).unwrap();
    runtime
        .set_compaction_settings(Some(noel_config::CompactionSettings {
            enabled: Some(false),
            ..Default::default()
        }))
        .unwrap();
    runtime
        .set_disabled_tools(BTreeSet::from(["bash".to_owned()]))
        .unwrap();
    runtime
        .set_todos(vec![TodoItem {
            content: "replay".into(),
            status: TodoStatus::InProgress,
        }])
        .unwrap();
    runtime
        .set_codemode_store(BTreeMap::from([("k".to_owned(), json!(1))]))
        .unwrap();
    runtime
        .set_labels(BTreeSet::from(["label".to_owned()]))
        .unwrap();
    runtime.rename_session(Some("named".into())).unwrap();
    let start = RunStart {
        id: "run-1".into(),
        request_id: Some("r-1".into()),
        kind: SubmissionKind::Prompt,
    };
    runtime
        .start_run(start, "hello".into(), Vec::new())
        .unwrap();
    runtime
        .push_message(ConversationMessage {
            role: MessageRole::Assistant,
            content: "hi".into(),
            thinking: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
            aborted: false,
        })
        .unwrap();
    runtime
        .end_run("run-1", RunStatus::Completed, None)
        .unwrap();
    let expected = runtime.session().clone();
    drop(runtime);
    let (_, reopened) = SessionStore::open(path).unwrap();
    assert_eq!(reopened, expected);
    let _ = fs::remove_dir_all(base);
}
