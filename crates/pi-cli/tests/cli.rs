use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn pi_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command.stdin(Stdio::null());
    command.env_remove("OPENAI_API_KEY");
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("GEMINI_API_KEY");
    command.env_remove("GOOGLE_API_KEY");
    command.env_remove("AZURE_OPENAI_API_KEY");
    command.env_remove("COPILOT_GITHUB_TOKEN");
    command.env_remove("OPENROUTER_API_KEY");
    command.env_remove("GOOGLE_CLOUD_API_KEY");
    command.env_remove("AWS_BEARER_TOKEN_BEDROCK");
    command.env_remove("MISTRAL_API_KEY");
    command.env_remove("CLOUDFLARE_API_KEY");
    command.env_remove("CODEX_ACCESS_TOKEN");
    command
}

#[test]
fn print_mode_uses_faux_model_without_session() {
    let output = pi_command()
        .args(["--no-session", "-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[faux/echo] hello\n"
    );
}

#[test]
fn json_mode_prints_structured_response() {
    let output = pi_command()
        .args([
            "--no-session",
            "--mode",
            "json",
            "-p",
            "--model",
            "faux/echo",
            "hello",
        ])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"message\":\"[faux/echo] hello\"}\n"
    );
}

#[test]
fn continue_reopens_most_recent_session() {
    let root = test_dir("pi-cli-continue");
    let sessions = root.join("sessions");
    let sessions_arg = path_text(&sessions);
    fs::create_dir_all(&root).expect("create root");

    let first = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--model",
            "faux/echo",
            "first",
        ])
        .output()
        .expect("run first turn");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--continue",
            "--model",
            "faux/echo",
            "second",
        ])
        .output()
        .expect("run second turn");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let sessions = fs::read_dir(&sessions)
        .expect("read sessions")
        .collect::<Result<Vec<_>, _>>()
        .expect("session entries");
    assert_eq!(sessions.len(), 1);
    let log = fs::read_to_string(sessions[0].path()).expect("read session log");
    assert!(log.contains("first"));
    assert!(log.contains("second"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn settings_session_dir_is_honored_when_no_override_is_passed() {
    let root = test_dir("pi-cli-settings-session-dir");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"sessionDir":"settings-sessions"}"#,
    )
    .expect("write settings");

    let output = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session_entries = fs::read_dir(root.join("settings-sessions"))
        .expect("read settings session dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("session entries");
    assert_eq!(session_entries.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn rpc_mode_accepts_json_line_requests_from_stdin() {
    let mut command = pi_command();
    command
        .args(["--no-session", "--mode", "rpc", "--model", "faux/echo"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn pi");
    let stdin = child.stdin.as_mut().expect("child stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"prompt","params":{{"prompt":"rpc hello"}}}}"#
    )
    .expect("write prompt request");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"session"}}"#)
        .expect("write session request");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    let first = serde_json::from_str::<serde_json::Value>(lines[0]).expect("parse first");
    let second = serde_json::from_str::<serde_json::Value>(lines[1]).expect("parse second");
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"]["message"], "[faux/echo] rpc hello");
    assert_eq!(second["id"], 2);
    assert_eq!(second["result"]["id"], "ephemeral");
}

#[test]
fn print_mode_expands_at_file_and_exports_session() {
    let root = test_dir("pi-cli-at-file-export");
    let sessions = root.join("sessions");
    let export = root.join("export.json");
    let prompt = root.join("prompt.txt");
    let sessions_arg = path_text(&sessions);
    let export_arg = path_text(&export);
    fs::create_dir_all(&root).expect("create root");
    fs::write(&prompt, "from file").expect("write prompt");

    let output = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--model",
            "faux/echo",
            "--export",
            export_arg.as_str(),
            "@prompt.txt",
        ])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[faux/echo] from file\n"
    );
    let exported =
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(export).expect("export"))
            .expect("parse export");
    assert_eq!(exported["messages"][0]["content"], "from file");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn print_mode_exports_html_and_jsonl_sessions() {
    let root = test_dir("pi-cli-export-formats");
    let sessions = root.join("sessions");
    let html = root.join("export.html");
    let jsonl = root.join("export.jsonl");
    let sessions_arg = path_text(&sessions);
    let html_arg = path_text(&html);
    let jsonl_arg = path_text(&jsonl);
    fs::create_dir_all(&root).expect("create root");

    let html_output = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--model",
            "faux/echo",
            "--export",
            html_arg.as_str(),
            "html export",
        ])
        .output()
        .expect("run pi html export");
    assert!(
        html_output.status.success(),
        "{}",
        String::from_utf8_lossy(&html_output.stderr)
    );

    let jsonl_output = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--model",
            "faux/echo",
            "--export",
            jsonl_arg.as_str(),
            "jsonl export",
        ])
        .output()
        .expect("run pi jsonl export");
    assert!(
        jsonl_output.status.success(),
        "{}",
        String::from_utf8_lossy(&jsonl_output.stderr)
    );

    let html_content = fs::read_to_string(html).expect("read html export");
    assert!(html_content.contains("<!doctype html>"));
    assert!(html_content.contains("html export"));
    let jsonl_content = fs::read_to_string(jsonl).expect("read jsonl export");
    assert!(jsonl_content.contains("\"type\":\"started\""));
    assert!(jsonl_content.contains("jsonl export"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn session_reference_and_fork_flags_preserve_parent_context() {
    let root = test_dir("pi-cli-session-ref-fork");
    let sessions = root.join("sessions");
    let sessions_arg = path_text(&sessions);
    fs::create_dir_all(&root).expect("create root");

    let first = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--model",
            "faux/echo",
            "first",
        ])
        .output()
        .expect("run first turn");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let session_path = fs::read_dir(&sessions)
        .expect("read sessions")
        .next()
        .expect("session entry")
        .expect("session entry")
        .path();
    let session_id = session_path
        .file_stem()
        .expect("session file stem")
        .to_string_lossy()
        .to_string();
    let prefix = &session_id[..8];

    let second = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--session",
            prefix,
            "--model",
            "faux/echo",
            "second",
        ])
        .output()
        .expect("run second turn");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let forked = pi_command()
        .current_dir(&root)
        .args([
            "-p",
            "--session-dir",
            sessions_arg.as_str(),
            "--fork",
            prefix,
            "--model",
            "faux/echo",
            "forked",
        ])
        .output()
        .expect("run forked turn");
    assert!(
        forked.status.success(),
        "{}",
        String::from_utf8_lossy(&forked.stderr)
    );

    let logs = fs::read_dir(&sessions)
        .expect("read sessions")
        .map(|entry| fs::read_to_string(entry.expect("entry").path()).expect("read log"))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 2);
    assert!(logs.iter().any(|log| log.contains("second")));
    assert!(logs.iter().any(|log| {
        log.contains("forked") && log.contains("parent_session_id") && log.contains(&session_id)
    }));

    let _ = fs::remove_dir_all(root);
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}-{}-{}", std::process::id(), unique_suffix()))
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}
