//! `noel --mode rpc` started with upstream's model and thinking flags, against
//! what upstream answered (`launches` in
//! `tests/fixtures/ts-parity/model-resolution.json`): the model and the
//! thinking level in effect that a supervisor reads from `get_state`, how
//! `set_model` and `set_thinking_level` change them, and the warnings and
//! errors startup prints.
//!
//! Each launch has its own agent directory, holding only the launch's
//! settings and `auth.json`, and fake provider keys and logins; no request is
//! sent.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// The variable that makes a provider count as having credentials, as in
/// the fixture script.
fn provider_key(provider: &str) -> (&'static str, &'static str) {
    match provider {
        "anthropic" => ("ANTHROPIC_API_KEY", "sk-ant-fixture"),
        "openai" => ("OPENAI_API_KEY", "sk-fixture"),
        "google" => ("GEMINI_API_KEY", "fixture"),
        other => panic!("no fixture key for {other}"),
    }
}

fn launches() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/ts-parity/model-resolution.json"
    ))
    .expect("parse model-resolution.json");
    fixture["launches"].as_array().expect("launches").clone()
}

fn launch_root(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("model-{name}-{}-{nanos}", std::process::id()));
    for dir in ["cwd", "agent", "home"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    root
}

/// Starts noel as the fixture started upstream and returns what the fixture
/// recorded: the exit code, the lines on stderr, and each command with the
/// events it caused and its answer.
fn run(launch: &Value) -> Value {
    let name = launch["name"].as_str().expect("name");
    let root = launch_root(name);
    // The fixture's settings, plus noel's own switch for its model refresh.
    let mut settings = launch["settings"].clone();
    settings["modelRefresh"] = json!({ "enabled": false });
    fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
    // Upstream's `auth.json`, which noel reads as it is.
    fs::write(root.join("agent/auth.json"), launch["auth"].to_string()).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_noel"));
    command
        .current_dir(root.join("cwd"))
        .env_clear()
        .env("NOEL_TEST_ISOLATION", "1")
        .env("NOEL_DIR", root.join("agent"))
        .env("HOME", root.join("home"))
        .args(["--mode", "rpc", "--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    for provider in launch["authenticated"].as_array().expect("authenticated") {
        let (variable, key) = provider_key(provider.as_str().expect("provider"));
        command.env(variable, key);
    }
    for arg in launch["args"].as_array().expect("args") {
        command.arg(arg.as_str().expect("arg"));
    }

    let commands = launch["exchanges"]
        .as_array()
        .expect("exchanges")
        .iter()
        .map(|exchange| exchange["command"].clone())
        .collect::<Vec<_>>();
    // A launch that fails at startup has no exchanges; noel must not need any
    // command to fail.
    let mut child = command.spawn().expect("start noel");
    let mut stdin = child.stdin.take().expect("stdin");
    for (index, command) in commands.iter().enumerate() {
        let mut command = command.clone();
        command["id"] = json!((index + 1).to_string());
        // Commands are answered in order, so they can all be sent at once.
        let _ = writeln!(stdin, "{command}");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("noel exits");
    fs::remove_dir_all(root).unwrap();

    let mut records = String::from_utf8(output.stdout)
        .expect("utf-8 stdout")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a JSON record per line"))
        .collect::<Vec<_>>()
        .into_iter();
    let mut exchanges = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let id = (index + 1).to_string();
        // Records before the answer are the events the command caused.
        let mut events = Vec::new();
        let response = loop {
            match records.next() {
                Some(record) if record["type"] == "response" && record["id"] == *id => {
                    break record
                }
                Some(record) => events.push(record),
                None => panic!("{name}: no answer to {command}"),
            }
        };
        let mut answer = json!({ "success": response["success"] });
        if let Some(error) = response.get("error") {
            answer["error"] = error.clone();
        }
        if command["type"] == "get_state" {
            answer["model"] = response["data"]["model"].clone();
            answer["thinkingLevel"] = response["data"]["thinkingLevel"].clone();
        } else if command["type"] == "set_model" && response["success"] == true {
            answer["model"] = json!({
                "provider": response["data"]["provider"],
                "id": response["data"]["id"],
            });
        }
        exchanges.push(json!({ "command": command, "events": events, "response": answer }));
    }
    json!({
        "exitCode": output.status.code(),
        "stderr": String::from_utf8(output.stderr)
            .expect("utf-8 stderr")
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>(),
        "exchanges": exchanges,
    })
}

/// A state upstream cannot be in: the Codex login `noel login` used to store
/// under `openai` as well as `openai-codex`. It is `openai-codex`'s
/// credential. `openai` takes an API key, so noel neither starts on its models
/// nor offers them.
#[test]
fn a_chatgpt_login_stored_for_openai_too_starts_on_openai_codex() {
    let login = json!({
        "type": "oauth",
        "access_token": "fixture-access-token",
        "refresh_token": "fixture-refresh-token",
        "expires": 4_102_444_800_u64,
    });
    let answered = run(&json!({
        "name": "login-stored-for-both",
        "args": [],
        "authenticated": [],
        "auth": { "openai": { "default": login }, "openai-codex": { "default": login } },
        "settings": {},
        "exchanges": [
            { "command": { "type": "get_state" } },
            { "command": { "type": "set_model", "provider": "openai", "modelId": "gpt-5.5" } },
        ],
    }));
    assert_eq!(answered["exitCode"], 0, "{answered}");
    let answers = answered["exchanges"]
        .as_array()
        .expect("exchanges")
        .iter()
        .map(|exchange| &exchange["response"])
        .collect::<Vec<_>>();
    assert_eq!(
        answers[0]["model"]["provider"], "openai-codex",
        "{answered}"
    );
    assert_eq!(
        *answers[1],
        json!({ "success": false, "error": "Model not found: openai/gpt-5.5" })
    );
}

#[test]
fn model_and_thinking_launches_match_ts() {
    let launches = launches();
    assert!(launches.len() > 40, "{} launches", launches.len());
    let mut failures = Vec::new();
    for launch in &launches {
        let expected = json!({
            "exitCode": launch["exitCode"],
            "stderr": launch["stderr"],
            "exchanges": launch["exchanges"],
        });
        let actual = run(launch);
        if actual != expected {
            failures.push(format!(
                "{}\n  args: {}\n  expected: {expected}\n  actual:   {actual}",
                launch["name"], launch["args"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} launches differ from upstream:\n{}",
        failures.len(),
        launches.len(),
        failures.join("\n")
    );
}
