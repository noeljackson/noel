use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Every `pi` a test runs is isolated: a cleared environment (no provider
/// keys or PI_* variables leak in), PI_TEST_ISOLATION (pi refuses the default
/// ~/.pi/agent, imports no login from $HOME, and starts no background model
/// refresh), and its own agent dir and HOME under the target dir (paths
/// only; nothing exists there unless pi writes). A test that needs a
/// particular agent dir or HOME sets its own.
fn pi_command() -> Command {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let isolated = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pi-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .stdin(Stdio::null())
        .env_clear()
        .env("PI_TEST_ISOLATION", "1")
        .env("PI_CODING_AGENT_DIR", isolated.join("agent"))
        .env("HOME", isolated.join("home"));
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

#[test]
fn tests_cannot_fall_back_to_the_default_agent_dir() {
    // Without its own agent dir a test fails instead of reading ~/.pi/agent.
    let output = pi_command()
        .env_remove("PI_CODING_AGENT_DIR")
        .args(["--no-session", "-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run pi");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing the default agent dir ~/.pi/agent"),
        "{stderr}"
    );
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

/// A pi command answering from `root/script.json` and logging each request.
fn faux_script_pi(root: &Path, args: &[&str]) -> Command {
    let mut command = pi_command();
    command
        .current_dir(root)
        .env("HOME", root)
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("PI_FAUX_SCRIPT", root.join("script.json"))
        .env("PI_FAUX_SCRIPT_LOG", root.join("requests.jsonl"))
        .args(["--offline", "--no-session", "--model", "faux/script"])
        .args(args);
    command
}

fn faux_script_log(root: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(root.join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("log line"))
        .collect()
}

#[test]
fn print_mode_runs_a_faux_script_tool_round_and_logs_each_request() {
    let root = test_dir("pi-cli-faux-script");
    fs::create_dir_all(root.join("agent")).unwrap();
    fs::write(root.join("notes.txt"), "alpha\nbeta\n").unwrap();
    fs::write(
        root.join("script.json"),
        r#"[
            [{"tool_call": {"name": "read", "arguments": {"path": "notes.txt"}}}],
            [{"text": "notes.txt lists alpha and beta"}]
        ]"#,
    )
    .unwrap();

    let output = faux_script_pi(
        &root,
        &[
            "--no-context-files",
            "--system-prompt",
            "Be brief.",
            "-p",
            "read notes.txt",
        ],
    )
    .output()
    .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "notes.txt lists alpha and beta\n"
    );
    let log = faux_script_log(&root);
    assert_eq!(log.len(), 2);
    assert_eq!(log[1]["system_prompt"], "Be brief.");
    assert!(log[1]["tools"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("read")));
    let messages = log[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "read notes.txt");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["tool_calls"],
        serde_json::json!([{"id": "call_1_1", "name": "read", "arguments": {"path": "notes.txt"}}])
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_1_1");
    assert_eq!(messages[2]["tool_name"], "read");
    assert!(
        messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("alpha\nbeta"),
        "{}",
        messages[2]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn faux_script_log_resumes_the_script_in_a_new_process() {
    let root = test_dir("pi-cli-faux-script-restart");
    fs::create_dir_all(root.join("agent")).unwrap();
    fs::write(
        root.join("script.json"),
        r#"[
            [{"text": "partial"}, {"wait_for_file": "never"}],
            [{"text": "resumed"}]
        ]"#,
    )
    .unwrap();

    // The first process logs its request, then holds the reply open until killed.
    let mut first = faux_script_pi(&root, &["-p", "first"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn pi");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fs::read_to_string(root.join("requests.jsonl")).is_ok_and(|log| log.ends_with('\n')) {
        assert!(first.try_wait().unwrap().is_none(), "first pi exited");
        assert!(Instant::now() < deadline, "first request was never logged");
        std::thread::sleep(Duration::from_millis(20));
    }
    first.kill().expect("kill first pi");
    first.wait().expect("reap first pi");

    let output = faux_script_pi(&root, &["-p", "second"])
        .output()
        .expect("run pi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "resumed\n");
    let log = faux_script_log(&root);
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["messages"][0]["content"], "first");
    assert_eq!(log[1]["messages"][0]["content"], "second");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn theme_flag_accepts_presets_paths_and_no_themes_fallback() {
    let root = test_dir("pi-cli-theme-flags");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        root.join("custom.json"),
        r##"{"base":"light","colors":{"accent":"#123456"}}"##,
    )
    .unwrap();
    for theme in ["system", "light", "dark", "kimi", "default", "custom.json"] {
        let output = pi_command()
            .current_dir(&root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .args([
                "--offline",
                "--no-session",
                "-p",
                "--model",
                "faux/echo",
                "--theme",
                theme,
                "hello",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "[faux/echo] hello\n"
        );
        assert!(!agent.join("settings.json").exists());
    }
    let output = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--offline",
            "--no-session",
            "-p",
            "--model",
            "faux/echo",
            "--theme",
            "missing",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("theme not found: missing"));
    let output = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--offline",
            "--no-session",
            "-p",
            "--model",
            "faux/echo",
            "--theme",
            "missing",
            "--no-themes",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::remove_dir_all(root).unwrap();
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
fn list_models_accepts_optional_search() {
    let output = pi_command()
        .args(["--list-models", "faux"])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("faux/echo"));
    assert!(!stdout.contains("openai/gpt"));
}

#[test]
fn list_image_models_accepts_optional_search() {
    let root = test_dir("pi-cli-image-models");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let output = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["images", "gemini"])
        .output()
        .expect("run pi images");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("openrouter/google/gemini-3.1-flash-image-preview"));
    assert!(!stdout.contains("black-forest-labs/flux.2-pro"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn ts_style_multi_letter_cli_aliases_are_accepted() {
    let output = pi_command()
        .args([
            "-nt",
            "-nbt",
            "-ne",
            "-ns",
            "-np",
            "-nc",
            "--list-models",
            "faux",
        ])
        .output()
        .expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("faux/echo"));
}

#[test]
fn ts_style_version_alias_is_accepted() {
    let output = pi_command().arg("-v").output().expect("run pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("pi "));
}

#[test]
fn package_commands_manage_settings_sources() {
    let root = test_dir("pi-cli-package-commands");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let install_user = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["install", "./user-plugin"])
        .output()
        .expect("install user package");
    assert!(
        install_user.status.success(),
        "{}",
        String::from_utf8_lossy(&install_user.stderr)
    );

    let install_project = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["install", "./project-plugin", "--local"])
        .output()
        .expect("install project package");
    assert!(
        install_project.status.success(),
        "{}",
        String::from_utf8_lossy(&install_project.stderr)
    );

    let user_settings = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("settings.json")).expect("user settings"),
    )
    .expect("parse user settings");
    assert_eq!(user_settings["packages"][0], "./user-plugin");
    let project_settings = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(root.join(".pi/settings.json")).expect("project settings"),
    )
    .expect("parse project settings");
    assert_eq!(project_settings["packages"][0], "./project-plugin");

    let list = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .arg("list")
        .output()
        .expect("list packages");
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("user: ./user-plugin"));
    assert!(stdout.contains("project: ./project-plugin"));

    let remove = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["remove", "./user-plugin"])
        .output()
        .expect("remove user package");
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let user_settings = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("settings.json")).expect("user settings after remove"),
    )
    .expect("parse user settings after remove");
    assert_eq!(
        user_settings["packages"]
            .as_array()
            .expect("packages")
            .len(),
        0
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn update_pulls_local_git_package_source() {
    let root = test_dir("pi-cli-package-update");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    git(&source, ["init"]);
    git(&source, ["config", "commit.gpgsign", "false"]);
    git(&source, ["config", "user.email", "pi@example.test"]);
    git(&source, ["config", "user.name", "pi test"]);
    fs::write(source.join("plugin.txt"), "v1").expect("write initial plugin");
    git(&source, ["add", "plugin.txt"]);
    git(&source, ["commit", "-m", "initial"]);
    git(
        &root,
        [
            "clone",
            source.to_str().expect("source path"),
            "package-plugin",
        ],
    );

    let install = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["install", "./package-plugin"])
        .output()
        .expect("install package");
    assert!(
        install.status.success(),
        "{}",
        String::from_utf8_lossy(&install.stderr)
    );

    fs::write(source.join("plugin.txt"), "v2").expect("write updated plugin");
    git(&source, ["add", "plugin.txt"]);
    git(&source, ["commit", "-m", "update"]);

    let update = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["update", "./package-plugin"])
        .output()
        .expect("update package");
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("updated package"), "{stdout}");
    assert_eq!(
        fs::read_to_string(root.join("package-plugin/plugin.txt")).expect("updated plugin"),
        "v2"
    );

    let _ = fs::remove_dir_all(root);
}

fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn login_commands_manage_api_key_auth() {
    let root = test_dir("pi-cli-login-commands");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let login = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["login", "anthropic", "--api-key", "env:ANTHROPIC_API_KEY"])
        .output()
        .expect("login anthropic");
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );

    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("auth.json")).expect("auth settings"),
    )
    .expect("parse auth settings");
    assert_eq!(auth["anthropic"]["default"]["type"], "api_key");
    assert_eq!(auth["anthropic"]["default"]["key"], "env:ANTHROPIC_API_KEY");

    let logout = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["logout", "anthropic"])
        .output()
        .expect("logout anthropic");
    assert!(
        logout.status.success(),
        "{}",
        String::from_utf8_lossy(&logout.stderr)
    );
    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("auth.json")).expect("auth after logout"),
    )
    .expect("parse auth after logout");
    assert!(auth.as_object().expect("auth object").is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn login_logout_round_trip_named_accounts() {
    let root = test_dir("pi-cli-named-accounts");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    for args in [
        vec!["login", "anthropic", "--api-key", "personal-key"],
        vec![
            "login",
            "anthropic",
            "--account",
            "work",
            "--api-key",
            "work-key",
        ],
    ] {
        let login = pi_command()
            .current_dir(&root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .args(&args)
            .output()
            .expect("login anthropic");
        assert!(
            login.status.success(),
            "{}",
            String::from_utf8_lossy(&login.stderr)
        );
    }

    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("auth.json")).expect("auth settings"),
    )
    .expect("parse auth settings");
    assert_eq!(auth["anthropic"]["default"]["key"], "personal-key");
    assert_eq!(auth["anthropic"]["work"]["key"], "work-key");

    let logout = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["logout", "anthropic", "--account", "work"])
        .output()
        .expect("logout work account");
    assert!(
        logout.status.success(),
        "{}",
        String::from_utf8_lossy(&logout.stderr)
    );
    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("auth.json")).expect("auth after logout"),
    )
    .expect("parse auth after logout");
    assert_eq!(auth["anthropic"]["default"]["key"], "personal-key");
    assert!(auth["anthropic"].get("work").is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn login_waits_for_another_process_auth_lock_and_writes_owner_only() {
    let root = test_dir("pi-cli-login-lock");
    let agent = root.join("agent");
    let auth_path = agent.join("auth.json");
    let held = pi_config::AuthFileLock::try_acquire(&auth_path)
        .expect("lock auth.json")
        .expect("free lock");

    let mut login = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["login", "anthropic", "--api-key", "locked-key"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn login");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        login.try_wait().expect("poll login").is_none(),
        "login finished while another process held the auth.json lock"
    );
    assert!(!auth_path.exists());

    drop(held);
    let output = login.wait_with_output().expect("wait for login");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(&auth_path).expect("auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(auth["anthropic"]["default"]["key"], "locked-key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&auth_path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn accounts_status_prints_table_or_empty_state() {
    let root = test_dir("pi-cli-accounts-status");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let base = |args: &[&str]| {
        let mut command = pi_command();
        command
            .current_dir(&root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("HOME", &root)
            .args(args);
        for name in [
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "AWS_BEARER_TOKEN_BEDROCK",
            "CEREBRAS_API_KEY",
            "DEEPSEEK_API_KEY",
            "FIREWORKS_API_KEY",
            "HF_TOKEN",
            "KIMI_API_KEY",
            "MINIMAX_API_KEY",
            "MINIMAX_CN_API_KEY",
            "MOONSHOT_API_KEY",
            "CODEX_API_KEY",
            "CHATGPT_ACCOUNT_ID",
            "OPENCODE_API_KEY",
            "TOGETHER_API_KEY",
            "AI_GATEWAY_API_KEY",
            "XAI_API_KEY",
            "XIAOMI_API_KEY",
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            "ZAI_API_KEY",
        ] {
            command.env_remove(name);
        }
        command.output().expect("accounts status")
    };

    let output = base(&["accounts", "status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "no accounts configured");

    let refresh = base(&["accounts", "status", "--refresh"]);
    assert!(
        refresh.status.success(),
        "{}",
        String::from_utf8_lossy(&refresh.stderr)
    );

    let _ = fs::remove_dir_all(root);
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
fn rpc_reload_keeps_an_untrusted_project_untrusted() {
    let root = test_dir("pi-cli-rpc-reload-trust");
    let agent = root.join("agent");
    let project = root.join("project");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::create_dir_all(project.join(".pi")).expect("create project config dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    // An untrusted repository's settings must not take effect, before or
    // after a reload.
    fs::write(
        project.join(".pi/settings.json"),
        r#"{"shellCommandPrefix":"echo untrusted-prefix"}"#,
    )
    .expect("write project settings");

    let mut child = pi_command()
        .current_dir(&project)
        .env("HOME", &root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--model",
            "faux/echo",
            "--no-approve",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pi");
    let mut stdin = child.stdin.take().expect("child stdin");
    for request in [
        r#"{"jsonrpc":"2.0","id":1,"method":"prompt","params":{"prompt":"/bash echo hi"}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"reload"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"prompt","params":{"prompt":"/bash echo hi"}}"#,
    ] {
        writeln!(stdin, "{request}").expect("write request");
    }
    drop(stdin);
    let output = child.wait_with_output().expect("wait for pi");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json response"))
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 3, "{responses:?}");
    assert!(responses[1].get("result").is_some(), "{}", responses[1]);
    for response in [&responses[0], &responses[2]] {
        assert_eq!(
            response["result"]["message"].as_str().map(str::trim),
            Some("hi"),
            "{response}"
        );
    }

    let _ = fs::remove_dir_all(root);
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
fn print_mode_respects_block_images_setting() {
    let root = test_dir("pi-cli-block-images");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"images":{"blockImages":true}}"#,
    )
    .expect("write settings");

    let output = pi_command()
        .current_dir(&root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--no-session",
            "-p",
            "--model",
            "faux/echo",
            "--image",
            "blocked.png",
            "describe",
        ])
        .output()
        .expect("run pi");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("images are blocked by settings"));

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
    assert!(jsonl_content.contains("\"type\":\"session\""));
    assert!(jsonl_content.contains("\"version\":3"));
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

/// A pi command for one isolated home, agent dir, and session dir.
fn isolated_pi(root: &Path, args: &[&str]) -> Command {
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    let mut command = pi_command();
    command
        .current_dir(root)
        .env("HOME", root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args(["--offline", "--model", "faux/echo", "--session-dir"])
        .arg(root.join("sessions"))
        .args(args);
    command
}

#[test]
fn a_session_open_in_another_process_makes_startup_wait_then_exit_75() {
    let root = test_dir("pi-cli-session-lock");
    let mut holder = isolated_pi(&root, &["--mode", "rpc", "--session-id", "shared"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rpc holder");
    // The journal's first record is written only after the lock is taken.
    let journal = root.join("sessions/shared.jsonl");
    let deadline = Instant::now() + Duration::from_secs(30);
    while fs::metadata(&journal).map_or(true, |metadata| metadata.len() == 0) {
        assert!(holder.try_wait().unwrap().is_none(), "rpc holder exited");
        assert!(
            Instant::now() < deadline,
            "rpc holder never opened its session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    let blocked = isolated_pi(&root, &["-p", "--session-id", "shared", "hi"])
        .output()
        .expect("run blocked print");
    let waited = started.elapsed();
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert_eq!(blocked.status.code(), Some(75), "{stderr}");
    assert!(
        waited >= Duration::from_millis(4_500) && waited < Duration::from_secs(20),
        "waited {waited:?}"
    );
    assert!(stderr.contains("shared.jsonl"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "in use by another pi process (pid {})",
            holder.id()
        )),
        "{stderr}"
    );
    assert!(blocked.stdout.is_empty());

    // SIGKILL runs no cleanup and leaves the lock file behind; the kernel
    // still releases the lock, so the session opens right away.
    holder.kill().expect("kill rpc holder");
    holder.wait().expect("reap rpc holder");
    let after = isolated_pi(&root, &["-p", "--session-id", "shared", "hi"])
        .output()
        .expect("run print after holder exit");
    assert!(
        after.status.success(),
        "{}",
        String::from_utf8_lossy(&after.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&after.stdout), "[faux/echo] hi\n");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_skips_a_damaged_session_and_writers_repair_torn_tails() {
    let root = test_dir("pi-cli-damaged-sessions");
    let first = isolated_pi(&root, &["-p", "first"]).output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let sessions = root.join("sessions");
    let good = fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .expect("first session journal");
    let cwd = serde_json::to_string(&root).unwrap();
    fs::write(
        sessions.join("damaged.jsonl"),
        format!(
            "{{\"type\":\"started\",\"session_id\":\"damaged\",\"cwd\":{cwd}}}\n{{\"type\":\"mess\n"
        ),
    )
    .unwrap();

    // One unreadable journal must not break listing for --continue.
    let second = isolated_pi(&root, &["-p", "--continue", "second"])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let log = fs::read_to_string(&good).unwrap();
    assert!(log.contains("first") && log.contains("second"), "{log}");

    // A writer drops a torn tail left by an interrupted append, then appends.
    let torn = sessions.join("torn.jsonl");
    fs::write(
        &torn,
        format!(
            "{{\"type\":\"started\",\"session_id\":\"torn\",\"cwd\":{cwd}}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"us"
        ),
    )
    .unwrap();
    let resumed = isolated_pi(
        &root,
        &["-p", "--session", torn.to_str().unwrap(), "after torn"],
    )
    .output()
    .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&resumed.stdout),
        "[faux/echo] after torn\n"
    );
    let log = fs::read_to_string(&torn).unwrap();
    assert!(log.ends_with('\n'));
    for line in log.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("corrupt line {line:?}: {error}"));
    }
    assert!(log.contains("after torn"));
    fs::remove_dir_all(root).unwrap();
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

#[test]
fn rpc_semantic_compaction_uses_active_local_provider_focus_and_persists_original_transcript() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    let root = test_dir("pi-cli-semantic-rpc");
    let agent = root.join("agent");
    let sessions = root.join("sessions");
    fs::create_dir_all(&agent).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let captured = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        for _ in 0..4 {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "local provider timed out"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("local server: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let body_start = loop {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..body_start]);
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < body_start + length {
                let mut chunk = [0u8; 4096];
                let n = socket.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request: serde_json::Value =
                serde_json::from_slice(&bytes[body_start..body_start + length]).unwrap();
            let compact = request["messages"][0]["content"]
                .as_str()
                .is_some_and(|s| s.starts_with("Create a concise continuation checkpoint"));
            captured.lock().unwrap().push(request);
            let answer = if compact {
                "Decisions: SQLite; use transactions. Next steps: finish migration."
            } else {
                "ack"
            };
            let body = serde_json::json!({"choices":[{"message":{"content":answer},"finish_reason":"stop"}],"usage":{"prompt_tokens":30,"completion_tokens":10}}).to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    fs::write(agent.join("settings.json"), r#"{"modelRefresh":{"enabled":false},"retry":{"enabled":false},"compaction":{"enabled":false,"keepRecentTokens":0}}"#).unwrap();
    fs::write(agent.join("models.json"), serde_json::json!([{
        "provider":"local","id":"test","api":"openai-completions","baseUrl":url,"contextWindow":8192
    }]).to_string()).unwrap();
    fs::write(
        agent.join("auth.json"),
        r#"{"local":{"type":"api_key","key":"fake-local-key"}}"#,
    )
    .unwrap();
    let mut command = pi_command();
    command
        .current_dir(&root)
        .env("HOME", &root)
        .env("PI_CODING_AGENT_DIR", &agent)
        .args([
            "--mode",
            "rpc",
            "--model",
            "local/test",
            "--no-context-files",
            "--no-tools",
            "--session-dir",
        ])
        .arg(&sessions)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for (id, prompt) in [
        (
            1,
            format!("use SQLite transactions {}", "reference ".repeat(300)),
        ),
        (2, "current migration".into()),
    ] {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"id":id,"method":"prompt","params":{"prompt":prompt}})
        )
        .unwrap();
    }
    writeln!(stdin, "{}", serde_json::json!({"id":3,"method":"compact","params":{"focus":"preserve database decisions"}})).unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"id":4,"method":"prompt","params":{"prompt":"continue"}})
    )
    .unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    let responses = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[2]["result"]["omitted_messages"], 2);
    assert!(responses[2]["result"]["summary"]
        .as_str()
        .unwrap()
        .contains("SQLite"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].get("tools").is_none());
    assert_eq!(requests[2]["max_tokens"], 2048);
    assert!(requests[2]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("preserve database decisions"));
    assert!(requests[3]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Previous conversation checkpoint"));
    let journal_path = fs::read_dir(&sessions)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let journal = fs::read_to_string(&journal_path).unwrap();
    assert!(journal.contains("use SQLite transactions reference"));
    assert!(journal.contains("compaction_checkpoint"));
    let loaded = pi_core::SessionStore::read(&journal_path).unwrap();
    assert_eq!(loaded.compactions.len(), 1);
    assert_eq!(loaded.usage.requests, 4);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn upstream_v1_1_flags_are_accepted_in_print_mode() {
    let root = test_dir("pi-cli-v1-1-flags");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    let run = |extra: &[&str]| {
        pi_command()
            .current_dir(&root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .args(["--offline", "--no-session", "-p", "--model", "faux/echo"])
            .args(extra)
            .arg("hello")
            .output()
            .unwrap()
    };

    let output = run(&["--no-mcp", "--tui-mode", "regular", "--use-theme", "light"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "[faux/echo] hello\n"
    );
    assert!(!agent.join("settings.json").exists());

    let pair = run(&["--use-theme", "light/dark"]);
    assert!(!pair.status.success());
    assert!(String::from_utf8_lossy(&pair.stderr).contains("theme pairs are not supported"));
    for args in [
        &["--no-themes", "--use-theme", "light"][..],
        &["--theme", "dark", "--use-theme", "light"],
    ] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("cannot be used with") && stderr.contains("--use-theme"),
            "{args:?}: {stderr}"
        );
    }
    let invalid_mode = run(&["--tui-mode", "split"]);
    assert!(!invalid_mode.status.success());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn mcp_command_reports_missing_support_instead_of_prompting() {
    let output = pi_command().args(["mcp", "list"]).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no MCP support"));
    assert!(output.stdout.is_empty());
}

#[test]
fn auth_commands_print_and_check_stored_credentials() {
    let root = test_dir("pi-cli-auth-command");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("auth.json"),
        r#"{"anthropic":{"default":{"type":"api_key","key":"sk-ant-synthetic"}}}"#,
    )
    .unwrap();
    let auth = |args: &[&str]| {
        pi_command()
            .current_dir(&root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("HOME", &root)
            .arg("auth")
            .args(args)
            .output()
            .unwrap()
    };
    let stdout =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stdout).to_string();
    let stderr =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).to_string();

    let help = auth(&["--help"]);
    assert!(help.status.success());
    assert!(stdout(&help).contains("pi auth print-bearer-token"));

    let key = auth(&["print-api-key", "--provider", "anthropic"]);
    assert!(key.status.success(), "{}", stderr(&key));
    assert_eq!(stdout(&key), "sk-ant-synthetic\n");

    let ready = auth(&["check", "--provider", "anthropic", "--json"]);
    assert_eq!(ready.status.code(), Some(0), "{}", stderr(&ready));
    assert_eq!(
        stdout(&ready),
        "{\"status\":\"ready\",\"provider\":\"anthropic\",\"authType\":\"api_key\"}\n"
    );

    let not_ready = auth(&["check", "--provider", "mistral"]);
    assert_eq!(not_ready.status.code(), Some(1), "{}", stderr(&not_ready));
    assert_eq!(stdout(&not_ready), "not_ready\n");

    let bearer = auth(&["print-bearer-token", "--provider", "anthropic"]);
    assert_eq!(bearer.status.code(), Some(1));
    assert!(stderr(&bearer).contains("is not configured with an OAuth bearer token"));
    assert!(stdout(&bearer).is_empty());

    // Exit statuses follow upstream: argument errors make a check invalid (2)
    // and a print fail (1), while an unknown command or option exits 1 for
    // every kind (upstream pins `auth check --credentails` to 1).
    let missing = auth(&["check"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("Auth checks require --provider"));
    let positional = auth(&["check", "--provider", "anthropic", "extra"]);
    assert_eq!(positional.status.code(), Some(2), "{}", stderr(&positional));
    let missing = auth(&["print-api-key"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("Credential printing requires --provider"));

    let unknown = auth(&["status"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).contains("Unknown auth command \"status\""));
    let misspelled = auth(&["check", "--provider", "anthropic", "--credentails"]);
    assert_eq!(misspelled.status.code(), Some(1));
    assert!(stderr(&misspelled).contains("Unknown option --credentails for \"auth check\"."));
    assert!(stdout(&misspelled).is_empty());

    fs::remove_dir_all(root).unwrap();
}

/// Claude Code and Codex logins pi reads from their own files: refreshing one
/// early would spend that CLI's single-use refresh token and sign it out.
#[test]
fn auth_commands_leave_imported_logins_unrefreshed_until_they_expire() {
    use base64::Engine as _;

    let root = test_dir("pi-cli-auth-imports");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::create_dir_all(root.join(".codex")).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claude_login = |expires: u64| {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"claude-code-access","refreshToken":"claude-code-refresh","expiresAt":{}}}}}"#,
            expires * 1000
        )
    };
    let jwt_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!(r#"{{"exp":{}}}"#, now + 600));
    let codex_access = format!("eyJhbGciOiJub25lIn0.{jwt_payload}.signature");
    let codex_login = format!(
        r#"{{"tokens":{{"access_token":"{codex_access}","refresh_token":"codex-refresh","account_id":"account"}}}}"#
    );
    fs::write(root.join(".codex/auth.json"), &codex_login).unwrap();
    let auth = |args: &[&str]| {
        let mut command = pi_command();
        command
            .current_dir(&root)
            .env("HOME", &root)
            .env("PI_CODING_AGENT_DIR", &agent)
            // PI_TEST_ISOLATION switches the login import off. This test
            // checks that import against login files in its own HOME, so it
            // runs without the marker.
            .env_remove("PI_TEST_ISOLATION")
            // Any refresh attempt fails locally instead of reaching a real token endpoint.
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("https_proxy", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9")
            .env("all_proxy", "http://127.0.0.1:9");
        for name in [
            "NO_PROXY",
            "no_proxy",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            command.env_remove(name);
        }
        command.arg("auth").args(args).output().unwrap()
    };
    let stdout =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stdout).to_string();
    let stderr =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).to_string();

    // Ten minutes left is under the 30m default, but the login is printed
    // unrefreshed with a warning.
    fs::write(
        root.join(".claude/.credentials.json"),
        claude_login(now + 600),
    )
    .unwrap();
    let claude = auth(&["print-bearer-token", "--provider", "anthropic"]);
    assert_eq!(claude.status.code(), Some(0), "{}", stderr(&claude));
    assert_eq!(stdout(&claude), "claude-code-access\n");
    for expected in [
        "Warning: the anthropic OAuth token from Claude Code expires in ",
        "sooner than the requested 30m",
        "so Claude Code stays signed in",
    ] {
        assert!(stderr(&claude).contains(expected), "{}", stderr(&claude));
    }
    let codex = auth(&["print-bearer-token", "--provider", "openai-codex"]);
    assert_eq!(codex.status.code(), Some(0), "{}", stderr(&codex));
    assert_eq!(stdout(&codex), format!("{codex_access}\n"));
    assert!(
        stderr(&codex).contains("from Codex expires in"),
        "{}",
        stderr(&codex)
    );

    // Three minutes left: enough for a 1m request, so no warning; a check
    // reports it ready but expiring soon.
    fs::write(
        root.join(".claude/.credentials.json"),
        claude_login(now + 180),
    )
    .unwrap();
    let short = auth(&[
        "print-bearer-token",
        "--provider",
        "anthropic",
        "--min-expiry",
        "1m",
    ]);
    assert_eq!(short.status.code(), Some(0), "{}", stderr(&short));
    assert_eq!(stdout(&short), "claude-code-access\n");
    assert!(stderr(&short).is_empty(), "{}", stderr(&short));
    let json = auth(&["check", "--provider", "anthropic", "--json"]);
    assert_eq!(json.status.code(), Some(0), "{}", stderr(&json));
    assert_eq!(
        stdout(&json),
        "{\"status\":\"ready\",\"provider\":\"anthropic\",\"authType\":\"oauth\",\"expiresSoon\":true}\n"
    );
    assert!(stderr(&json).is_empty(), "{}", stderr(&json));
    let text = auth(&["check", "--provider", "anthropic"]);
    assert_eq!(text.status.code(), Some(0), "{}", stderr(&text));
    assert_eq!(stdout(&text), "ready\n");
    assert!(
        stderr(&text).contains("Warning: the anthropic OAuth token from Claude Code expires in"),
        "{}",
        stderr(&text)
    );

    // Nothing was refreshed: pi stored no copy and the logins are untouched.
    assert!(!agent.join("auth.json").exists());
    assert_eq!(
        fs::read_to_string(root.join(".claude/.credentials.json")).unwrap(),
        claude_login(now + 180)
    );
    assert_eq!(
        fs::read_to_string(root.join(".codex/auth.json")).unwrap(),
        codex_login
    );

    fs::remove_dir_all(root).unwrap();
}

/// Whether `pid` still runs. A killed process stays a zombie (state Z) until
/// it is reaped, which counts as exited.
#[cfg(unix)]
fn process_is_running(pid: &str) -> bool {
    if cfg!(target_os = "linux") {
        return fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| !fields.trim_start().starts_with('Z'))
        });
    }
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Serves `responses` OpenAI chat-completions bodies, one per connection.
#[cfg(unix)]
fn serve_chat_completions(responses: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for body in responses {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "local provider timed out");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("local server: {error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let body_start = loop {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let length = String::from_utf8_lossy(&request[..body_start])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while request.len() < body_start + length {
                let mut chunk = [0u8; 4096];
                let read = socket.read(&mut chunk).unwrap();
                assert!(read > 0, "request ended before its body");
                request.extend_from_slice(&chunk[..read]);
            }
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        }
    });
    (url, server)
}

#[cfg(unix)]
#[test]
fn fatal_signals_during_a_print_mode_bash_tool_kill_its_tree_and_end_pi_like_upstream() {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};
    let root = test_dir("pi-cli-signal-bash");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false},"retry":{"enabled":false},"compaction":{"enabled":false}}"#,
    )
    .unwrap();
    fs::write(
        agent.join("auth.json"),
        r#"{"local":{"type":"api_key","key":"fake-local-key"}}"#,
    )
    .unwrap();

    // Upstream exits 143/129 on SIGTERM/SIGHUP and dies of SIGINT.
    for (signal, expected) in [
        ("TERM", (Some(143), None)),
        ("INT", (None, Some(2))),
        ("HUP", (Some(129), None)),
    ] {
        // The model calls bash; the shell and a background child publish
        // their pids, then wait on the child. Each case has its own
        // one-response provider, so a dying pi cannot take the next reply.
        let pid_file = root.join(format!("{signal}.pids"));
        let command = format!(
            "echo $$ > '{0}.tmp'; sleep 30 & echo $! >> '{0}.tmp'; mv '{0}.tmp' '{0}'; wait",
            pid_file.display()
        );
        let (url, server) = serve_chat_completions(vec![serde_json::json!({
            "choices":[{"message":{"content":null,"tool_calls":[{
                "id":"call-1","type":"function",
                "function":{"name":"bash","arguments":serde_json::json!({"command":command}).to_string()}
            }]},"finish_reason":"tool_calls"}]
        })
        .to_string()]);
        fs::write(
            agent.join("models.json"),
            serde_json::json!([{
                "provider":"local","id":"test","api":"openai-completions","baseUrl":url,"contextWindow":8192
            }])
            .to_string(),
        )
        .unwrap();
        let mut pi = pi_command()
            .current_dir(&root)
            .env("HOME", &root)
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("SHELL", "/bin/sh")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args(["--no-session", "--no-context-files", "-p"])
            .args(["--model", "local/test", "run the tool"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let pids = loop {
            if let Ok(pids) = fs::read_to_string(&pid_file) {
                break pids
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
            }
            if Instant::now() >= deadline || pi.try_wait().unwrap().is_some() {
                let _ = pi.kill();
                let output = pi.wait_with_output().unwrap();
                panic!(
                    "the bash tool never started: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(Command::new("kill")
            .args([&format!("-{signal}"), &pi.id().to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = pi.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = pi.kill();
                panic!("pi ignored SIG{signal}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|pid| process_is_running(pid)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors = pids
            .iter()
            .filter(|pid| process_is_running(pid))
            .collect::<Vec<_>>();
        for pid in &survivors {
            let _ = Command::new("kill").args(["-KILL", pid.as_str()]).status();
        }
        let mut stderr = String::new();
        pi.stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert_eq!(
            (status.code(), status.signal()),
            expected,
            "SIG{signal}: {stderr}"
        );
        assert!(
            survivors.is_empty(),
            "SIG{signal} left tool processes running: {survivors:?}"
        );
        server.join().unwrap();
    }
    fs::remove_dir_all(root).unwrap();
}
