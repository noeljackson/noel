//! Runs pi's built-in tools on the inputs of `tool-behavior.json`, which
//! records what upstream's tools return for them, and compares the results
//! exactly. `scripts/ts-parity/tool-behavior-fixture.ts` writes the fixture
//! and documents its placeholders and normalizations.

use std::fs;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    execute_model_tool_call, model_tool_definition, ChatToolCall, ReloadableSystems, Runtime,
    SessionState,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/ts-parity/tool-behavior.json"
    ))
    .expect("parse tool-behavior.json")
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `prefix + n + suffix` for n = 1..=count, n zero-padded to `pad` digits.
fn series(spec: &Value) -> Vec<String> {
    let text = |key: &str| spec[key].as_str().expect("series text");
    let pad = spec["pad"].as_u64().expect("series pad") as usize;
    (1..=spec["count"].as_u64().expect("series count"))
        .map(|n| format!("{}{n:0pad$}{}", text("prefix"), text("suffix")))
        .collect()
}

/// Creates one entry of a fixture root.
fn write_entry(root: &Path, entry: &Value) {
    let path = root.join(entry["path"].as_str().expect("entry path"));
    if entry.get("dir").is_some() {
        fs::create_dir_all(&path).expect("create directory");
    } else if let Some(files) = entry.get("files") {
        fs::create_dir_all(&path).expect("create directory");
        for name in series(files) {
            fs::write(path.join(name), entry["text"].as_str().expect("file text"))
                .expect("write file");
        }
    } else {
        fs::create_dir_all(path.parent().expect("entry parent")).expect("create parent");
        let content = if let Some(base64) = entry.get("base64") {
            base64::engine::general_purpose::STANDARD
                .decode(base64.as_str().expect("base64 text"))
                .expect("decode base64")
        } else if let Some(lines) = entry.get("lines") {
            series(lines)
                .iter()
                .flat_map(|line| format!("{line}\n").into_bytes())
                .collect()
        } else {
            entry["parts"]
                .as_array()
                .expect("entry parts")
                .iter()
                .map(|part| match part.as_str() {
                    Some(text) => text.to_string(),
                    None => part["repeat"]
                        .as_str()
                        .expect("repeated text")
                        .repeat(part["count"].as_u64().expect("repeat count") as usize),
                })
                .collect::<String>()
                .into_bytes()
        };
        fs::write(path, content).expect("write file");
    }
}

/// The fixture's record of a result text: the text, or for a long one its
/// size, hash, and both ends.
fn record_text(text: &str) -> Value {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.len() <= 2000 {
        return json!(text);
    }
    json!({
        "bytes": text.len(),
        "lines": text.split('\n').count(),
        "sha256": sha256(text.as_bytes()),
        "head": characters[..200].iter().collect::<String>(),
        "tail": characters[characters.len() - 200..].iter().collect::<String>(),
    })
}

/// Applies a call's normalization to its result lines, which end at the
/// first blank line.
fn normalize_results(text: String, mode: Option<&str>) -> String {
    let Some(mode) = mode else {
        return text;
    };
    let (results, rest) = match text.find("\n\n") {
        Some(blank) => text.split_at(blank),
        None => (text.as_str(), ""),
    };
    let mut results = results.split('\n').collect::<Vec<_>>();
    // Messages such as "No files found matching pattern" are not results.
    if results.len() == 1 && results[0].contains(' ') {
        return text;
    }
    match mode {
        "sorted-lines" => {
            results.sort_unstable();
            format!("{}{rest}", results.join("\n"))
        }
        "result-count" => format!("<{} results>{rest}", results.len()),
        other => panic!("unknown normalization {other}"),
    }
}

/// Replaces the path of bash's saved output with its placeholder and
/// returns the fixture's record of the saved file, which is then removed.
fn take_saved_output(text: &mut String) -> Option<Value> {
    let start = text.find("Full output: ")? + "Full output: ".len();
    let end = start + text[start..].find(']')?;
    let path = PathBuf::from(&text[start..end]);
    assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
    let name = path.file_name()?.to_str()?;
    let random = name.strip_prefix("pi-bash-")?.strip_suffix(".log")?;
    assert!(
        random.len() == 16 && random.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "unexpected output file name {name}"
    );
    let saved = fs::read(&path).expect("read saved output");
    fs::remove_file(&path).expect("remove saved output");
    text.replace_range(start..end, "<output-file>");
    Some(json!({ "bytes": saved.len(), "sha256": sha256(&saved) }))
}

#[test]
fn builtin_tool_definitions_match_upstream() {
    for upstream in fixture()["definitions"].as_array().expect("definitions") {
        let name = upstream["name"].as_str().expect("tool name");
        let definition = model_tool_definition(name).expect("tool definition");
        assert_eq!(
            json!(definition.description),
            upstream["description"],
            "description of {name}"
        );
        assert_eq!(
            definition.parameters, upstream["parameters"],
            "parameters of {name}"
        );
    }
}

#[tokio::test]
async fn builtin_tools_behave_like_upstream() {
    let fixture = fixture();
    let tmp = std::env::temp_dir().join(format!("pi-tool-behavior-{}", std::process::id()));
    // `grep` applies `.gitignore` only inside a git repository, so the
    // directory the roots live in must not be inside one.
    assert!(
        !tmp.ancestors()
            .any(|directory| directory.join(".git").exists()),
        "the temp directory {} is inside a git repository",
        tmp.display()
    );
    let _ = fs::remove_dir_all(&tmp);
    for (root, entries) in fixture["roots"].as_object().expect("roots") {
        fs::create_dir_all(tmp.join(root)).expect("create root");
        for entry in entries.as_array().expect("root entries") {
            write_entry(&tmp.join(root), entry);
        }
    }
    let tmp_text = tmp.to_str().expect("temp path");

    let mut failures = Vec::new();
    for call in fixture["calls"].as_array().expect("calls") {
        let name = call["name"].as_str().expect("call name");
        let cwd = tmp.join(call["cwd"].as_str().expect("call cwd"));
        let mut runtime = Runtime::new(
            SessionState::new("tool-behavior", cwd),
            ReloadableSystems::default(),
        );
        let tool_call = ChatToolCall {
            id: name.to_string(),
            name: call["tool"].as_str().expect("call tool").to_string(),
            arguments: call["args"].to_string().replace("<tmp>", tmp_text),
        };
        execute_model_tool_call(&mut runtime, &tool_call)
            .await
            .expect("record tool result");
        let event = runtime.session.tool_history.last().expect("tool event");
        let message = runtime.session.messages.last().expect("tool message");

        let mut text = event.result.clone();
        let saved_output = take_saved_output(&mut text);
        let text = normalize_results(text.replace(tmp_text, "<tmp>"), call["normalize"].as_str());
        let mut result = json!({ "text": record_text(&text), "isError": event.is_error });
        if !message.media.is_empty() {
            result["images"] = message
                .media
                .iter()
                .map(|image| {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(&image.data_base64)
                        .expect("decode image");
                    json!({
                        "mimeType": image.mime_type,
                        "bytes": bytes.len(),
                        "sha256": sha256(&bytes),
                    })
                })
                .collect();
        }
        if let Some(saved_output) = saved_output {
            result["savedOutput"] = saved_output;
        }
        if result != call["result"] {
            failures.push(format!(
                "{name}\n  upstream: {}\n  pi:       {result}",
                call["result"]
            ));
        }
    }
    let _ = fs::remove_dir_all(&tmp);
    assert!(
        failures.is_empty(),
        "{} of {} calls differ from upstream:\n{}",
        failures.len(),
        fixture["calls"].as_array().map_or(0, Vec::len),
        failures.join("\n")
    );
}
