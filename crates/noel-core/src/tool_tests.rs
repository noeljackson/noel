//! How noel selects tools and runs model tool calls where that is its own
//! design: extension tools, tool selection, and what a tool result carries.
//! `tool_parity_tests` compares the built-in tools with upstream.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use serde_json::json;

use crate::{
    active_tool_definitions, available_tool_names, bash_timeout_ms, execute_model_tool_call,
    extension_tools_from_resources, prompt_tools, provider_messages, runtime_system_prompt,
    ChatToolCall, ConversationMessage, ExtensionTool, MessageRole, ReloadableSystems, ResourceFile,
    Runtime, SessionState,
};

/// A session in a new directory with one extension, `ext-tool`, whose
/// executable is the shell `script` and which defines the tool `tool_name`.
fn runtime_with_extension(test: &str, tool_name: &str, script: &str) -> (Runtime, PathBuf) {
    // Under `target/`: the system temp directory may not allow executables.
    let cwd = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join(format!("noel-tool-tests-{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&cwd);
    fs::create_dir_all(&cwd).expect("create test dir");
    let extension_path = cwd.join("ext-tool.sh");
    fs::write(
        &extension_path,
        format!("#!/bin/sh\ncat >/dev/null\n{script}\n"),
    )
    .expect("write extension");
    fs::set_permissions(&extension_path, fs::Permissions::from_mode(0o755))
        .expect("make extension executable");
    fs::write(
        cwd.join("ext-tool.sh.noel-extension.json"),
        json!({
            "protocol": "json",
            "tools": [{ "name": tool_name, "description": "From the extension." }],
        })
        .to_string(),
    )
    .expect("write manifest");
    let extension_tools = extension_tools_from_resources(&[ResourceFile {
        name: "ext-tool".to_string(),
        path: extension_path,
        content: String::new(),
    }]);
    let runtime = Runtime::new(
        SessionState::new("session-1", cwd.clone()),
        ReloadableSystems {
            extension_tools,
            ..ReloadableSystems::default()
        },
    );
    (runtime, cwd)
}

/// Runs one model tool call and returns its saved output and error flag.
async fn call_tool(
    runtime: &mut Runtime,
    name: &str,
    arguments: serde_json::Value,
) -> (String, bool) {
    let call = ChatToolCall {
        id: format!("call-{name}"),
        name: name.to_string(),
        arguments: arguments.to_string(),
    };
    execute_model_tool_call(runtime, &call, &mut |_| {})
        .await
        .expect("save tool result");
    let event = runtime.session.tool_history.last().expect("tool event");
    (event.result.clone(), event.is_error)
}

#[tokio::test]
async fn failed_extension_tool_is_an_error_result() {
    let (mut runtime, cwd) =
        runtime_with_extension("failure", "ext_fail", "echo partial; echo boom >&2; exit 3");
    let (output, is_error) = call_tool(&mut runtime, "ext_fail", json!({})).await;
    assert!(is_error, "a non-zero exit is a failed tool call");
    assert_eq!(output, "extension ext-tool failed:\npartial\nboom\n");
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn extension_tool_output_is_capped_like_builtin_tools() {
    let script =
        r#"printf '{"output":"'; seq 1 3000 | tr '\n' ' ' | sed 's/ /\\n/g'; printf '"}\n'"#;
    let (mut runtime, cwd) = runtime_with_extension("cap", "ext_long", script);
    let (output, is_error) = call_tool(&mut runtime, "ext_long", json!({})).await;
    assert!(!is_error, "{output}");
    let (shown, notice) = output
        .split_once("\n\n[Showing lines 1-2000 of 3000. Full output: ")
        .expect("truncation notice");
    assert!(shown.starts_with("1\n2\n") && shown.ends_with("\n2000"));
    let saved = PathBuf::from(notice.strip_suffix(']').expect("notice end"));
    let full = fs::read_to_string(&saved).expect("saved output");
    assert_eq!(full.lines().count(), 3000);
    let _ = fs::remove_file(saved);
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn extension_tool_replaces_the_builtin_tool_of_the_same_name() {
    let script = r#"printf '{"output":"read by the extension"}\n'"#;
    let (mut runtime, cwd) = runtime_with_extension("override", "read", script);
    let definitions = active_tool_definitions(&runtime);
    let read = definitions
        .iter()
        .filter(|definition| definition.name == "read")
        .collect::<Vec<_>>();
    assert_eq!(read.len(), 1, "one definition per tool name");
    assert_eq!(read[0].description, "From the extension.");
    // The other built-in tools are still offered.
    assert!(definitions
        .iter()
        .any(|definition| definition.name == "bash"));

    let (output, is_error) = call_tool(&mut runtime, "read", json!({ "path": "absent.txt" })).await;
    assert_eq!(
        (output.as_str(), is_error),
        ("read by the extension", false)
    );
    let _ = fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn read_of_an_image_attaches_it_to_the_tool_result() {
    let cwd = std::env::temp_dir().join(format!("noel-tool-tests-image-{}", std::process::id()));
    fs::create_dir_all(&cwd).expect("create test dir");
    let png = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==",
    )
    .expect("decode png");
    fs::write(cwd.join("pixel.png"), &png).expect("write image");
    let session = || SessionState::new("session-1", cwd.clone());

    let mut runtime = Runtime::new(session(), ReloadableSystems::default());
    // The reply whose call the result answers.
    runtime
        .push_message(ConversationMessage {
            thinking: String::new(),
            role: MessageRole::Assistant,
            content: String::new(),
            media: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: vec![ChatToolCall {
                id: "call-read".to_string(),
                name: "read".to_string(),
                arguments: json!({ "path": "pixel.png" }).to_string(),
            }],
        })
        .expect("save reply");
    let (output, is_error) = call_tool(&mut runtime, "read", json!({ "path": "pixel.png" })).await;
    assert_eq!(
        (output.as_str(), is_error),
        ("Read image file [image/png]", false)
    );
    // The image is saved with the result and goes to the provider with it.
    let sent = provider_messages(&runtime.session.messages);
    let result = sent.last().expect("tool result");
    assert_eq!(result.media.len(), 1);
    assert_eq!(result.media[0].mime_type, "image/png");
    assert_eq!(
        base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &result.media[0].data_base64
        )
        .expect("decode image"),
        png
    );

    // `images.blockImages` keeps images away from the model.
    let mut blocked = Runtime::new(
        session(),
        ReloadableSystems {
            block_images: true,
            ..ReloadableSystems::default()
        },
    );
    let (output, _) = call_tool(&mut blocked, "read", json!({ "path": "pixel.png" })).await;
    assert_eq!(
        output,
        "Read image file [image/png]\nImage reading is disabled."
    );
    assert!(blocked
        .session
        .messages
        .last()
        .expect("result")
        .media
        .is_empty());
    let _ = fs::remove_dir_all(cwd);
}

#[test]
fn bash_timeout_is_in_seconds_and_has_no_default() {
    let timeout =
        |args: serde_json::Value| bash_timeout_ms(&args).map_err(|error| error.to_string());
    assert_eq!(timeout(json!({ "command": "true" })), Ok(None));
    assert_eq!(timeout(json!({ "timeout": 30 })), Ok(Some(30_000)));
    assert_eq!(timeout(json!({ "timeout": 0.25 })), Ok(Some(250)));
    for invalid in [json!(0), json!(-1), json!("5")] {
        assert_eq!(
            timeout(json!({ "timeout": invalid })),
            Err("Invalid timeout: must be a finite number of seconds".to_string())
        );
    }
    assert_eq!(
        timeout(json!({ "timeout": 3_000_000 })),
        Err("Invalid timeout: maximum is 2147483.647 seconds".to_string())
    );
}

fn extension_tool(name: &str) -> (String, ExtensionTool) {
    let tool = ExtensionTool {
        name: name.to_string(),
        description: String::new(),
        parameters: json!({}),
        extension_name: "ext".to_string(),
        extension_path: PathBuf::from("ext"),
        protocol: "json".to_string(),
        replay_safe: false,
        prompt_snippet: None,
        prompt_guidelines: Vec::new(),
    };
    (name.to_string(), tool)
}

#[test]
fn tool_selection_follows_default_tools_allowlist_and_exclusions() {
    let extension_tools = BTreeMap::from([extension_tool("ext_echo")]);
    let selected = |settings: serde_json::Value, excluded: &[&str]| {
        let mut settings: noel_config::Settings =
            serde_json::from_value(settings).expect("settings");
        settings.excluded_tools = excluded.iter().map(ToString::to_string).collect();
        available_tool_names(&settings, &extension_tools)
    };
    let names = |names: &[&str]| {
        names
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
    };
    let all = [
        "bash",
        "edit",
        "ext_echo",
        "find",
        "grep",
        "ls",
        "read",
        "todo",
        "web_fetch",
        "web_search",
        "write",
    ];

    // Unset, every built-in tool and every extension tool is available.
    assert_eq!(selected(json!({}), &[]), names(&all));
    // `defaultTools` selects built-in tools; extension tools stay.
    assert_eq!(
        selected(
            json!({ "defaultTools": ["read", "+grep", "no_such_tool"] }),
            &[]
        ),
        names(&["ext_echo", "grep", "read"])
    );
    assert_eq!(
        selected(json!({ "defaultTools": ["-bash", "-web_fetch"] }), &[]),
        names(&all)
            .difference(&names(&["bash", "web_fetch"]))
            .cloned()
            .collect()
    );
    // An empty list is `--no-builtin-tools`.
    assert_eq!(
        selected(json!({ "defaultTools": [] }), &[]),
        names(&["ext_echo"])
    );
    // `enabledTools` is an allowlist of names and patterns over all tools.
    assert_eq!(
        selected(
            json!({ "enabledTools": ["re*", "*_echo"], "defaultTools": [] }),
            &[]
        ),
        names(&["ext_echo", "read"])
    );
    assert_eq!(selected(json!({ "enabledTools": [] }), &[]), names(&[]));
    // Exclusions apply last, to extension tools too.
    assert_eq!(
        selected(
            json!({ "enabledTools": ["*"] }),
            &["web_*", "ext_echo", "todo"]
        ),
        names(&["bash", "edit", "find", "grep", "ls", "read", "write"])
    );
}

#[test]
fn system_prompt_lists_the_tools_the_request_declares() {
    // An extension with its own tool and one named like a built-in tool.
    let mut extension_tools = BTreeMap::from([extension_tool("ext_echo"), extension_tool("ls")]);
    for (name, tool) in &mut extension_tools {
        tool.prompt_snippet = Some(format!("Extension {name}"));
    }
    let runtime = |settings: serde_json::Value, excluded: &[&str]| {
        let mut settings: noel_config::Settings =
            serde_json::from_value(settings).expect("settings");
        settings.excluded_tools = excluded.iter().map(ToString::to_string).collect();
        Runtime::new(
            SessionState::new("session-1", PathBuf::from("/repo")),
            ReloadableSystems {
                available_tool_names: available_tool_names(&settings, &extension_tools),
                tool_scope_configured: true,
                extension_tools: extension_tools.clone(),
                ..ReloadableSystems::default()
            },
        )
    };
    for (selection, excluded, declared) in [
        (
            json!({}),
            &[][..],
            &[
                "read",
                "bash",
                "edit",
                "write",
                "grep",
                "find",
                "todo",
                "web_search",
                "web_fetch",
                "ext_echo",
                "ls",
            ][..],
        ),
        // defaultTools, as a list and as modifiers.
        (
            json!({ "defaultTools": ["read", "+grep"] }),
            &[],
            &["read", "grep", "ext_echo", "ls"],
        ),
        (
            json!({ "defaultTools": ["-bash", "-todo", "-web_search", "-web_fetch"] }),
            &[],
            &["read", "edit", "write", "grep", "find", "ext_echo", "ls"],
        ),
        // --no-builtin-tools keeps the extension's tools.
        (json!({ "defaultTools": [] }), &[], &["ext_echo", "ls"]),
        // --tools patterns, and --exclude-tools.
        (
            json!({ "enabledTools": ["re*", "ext_*"] }),
            &[],
            &["read", "ext_echo"],
        ),
        (
            json!({ "enabledTools": ["*"] }),
            &["web_*", "ext_*", "todo"],
            &["read", "bash", "edit", "write", "grep", "find", "ls"],
        ),
        (json!({ "enabledTools": [] }), &[], &[]),
    ] {
        let runtime = runtime(selection.clone(), excluded);
        let declared_names = active_tool_definitions(&runtime)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(declared_names, declared, "{selection}");
        let prompt_names = prompt_tools(&runtime)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(prompt_names, declared, "{selection}");

        // The prompt's tool list has one line per declared tool, and the
        // extension's `ls` replaces the built-in one there too.
        let prompt = runtime_system_prompt(&runtime);
        let (_, after) = prompt.split_once("<tools>\n").expect("tools section");
        let listed = after
            .lines()
            .take_while(|line| line.starts_with("- "))
            .map(|line| line[2..].split(':').next().expect("tool name"))
            .collect::<Vec<_>>();
        assert_eq!(listed, declared, "{selection}");
        if declared.contains(&"ls") {
            assert!(prompt.contains("- ls: Extension ls\n"), "{prompt}");
            assert!(!prompt.contains("List directory contents"), "{prompt}");
        }
    }
}
