use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Every `noel` a test runs is isolated: a cleared environment (no provider
/// keys or NOEL_* variables leak in), NOEL_TEST_ISOLATION (noel refuses the default
/// ~/.noel, imports no login from $HOME, and starts no background model
/// refresh), and its own agent dir and HOME under the target dir (paths
/// only; nothing exists there unless noel writes). So no test touches the
/// developer's ~/.noel or imports their ~/.pi on a first run. A test that
/// needs a particular agent dir or HOME sets its own.
fn noel_command() -> Command {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let isolated = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "noel-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_noel"));
    command
        .stdin(Stdio::null())
        .env_clear()
        .env("NOEL_TEST_ISOLATION", "1")
        .env("NOEL_DIR", isolated.join("agent"))
        .env("HOME", isolated.join("home"));
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
}

#[test]
fn tests_cannot_fall_back_to_the_default_agent_dir() {
    // Without its own agent dir a test fails instead of reading ~/.noel.
    let output = noel_command()
        .env_remove("NOEL_DIR")
        .args(["--no-session", "-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run noel");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing the default agent dir ~/.noel"),
        "{stderr}"
    );
}

#[test]
fn print_mode_uses_faux_model_without_session() {
    let output = noel_command()
        .args(["--no-session", "-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run noel");

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

/// A noel command answering from `root/script.json` and logging each request.
fn faux_script_noel(root: &Path, args: &[&str]) -> Command {
    let mut command = noel_command();
    command
        .current_dir(root)
        .env("HOME", root)
        .env("NOEL_DIR", root.join("agent"))
        .env("NOEL_FAUX_SCRIPT", root.join("script.json"))
        .env("NOEL_FAUX_SCRIPT_LOG", root.join("requests.jsonl"))
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
    let root = test_dir("noel-cli-faux-script");
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

    let output = faux_script_noel(
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
    .expect("run noel");

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
    // A custom prompt replaces the default role, tool list, and rules; the
    // working directory still follows it.
    let cwd = fs::canonicalize(&root).unwrap();
    assert_eq!(
        log[1]["system_prompt"],
        format!("Be brief.\n\n<cwd>\n{}\n</cwd>", cwd.display())
    );
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

/// With retries off, a rate limit ends a run on one account. With account
/// failover on and a second stored account, the same rate limit is answered
/// by that account instead.
#[test]
fn print_mode_fails_over_to_a_second_account_when_the_first_is_rate_limited() {
    let run = |name: &str, failover: bool| {
        let root = test_dir(name);
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::write(
            root.join("agent/settings.json"),
            serde_json::json!({"retry": {"enabled": false}, "failover": {"enabled": failover}})
                .to_string(),
        )
        .unwrap();
        fs::write(
            root.join("agent/auth.json"),
            r#"{"faux": {
                "default": {"type": "api_key", "key": "default-key"},
                "second": {"type": "api_key", "key": "second-key"}
            }}"#,
        )
        .unwrap();
        fs::write(
            root.join("script.json"),
            r#"[
                [{"error": {"status": 429, "body": "rate limited"}}],
                [{"text": "served by the second account"}]
            ]"#,
        )
        .unwrap();
        let output = faux_script_noel(&root, &["--no-context-files", "-p", "hello"])
            .output()
            .expect("run noel");
        let requests = faux_script_log(&root).len();
        fs::remove_dir_all(root).unwrap();
        (output, requests)
    };

    let (output, requests) = run("noel-cli-failover-on", true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "served by the second account\n"
    );
    assert_eq!(requests, 2);

    let (output, requests) = run("noel-cli-failover-off", false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("request failed with status 429 Too Many Requests: rate limited"),
        "{stderr}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert_eq!(requests, 1);
}

/// Runs noel once in `root` on a one-reply script and returns the system
/// prompt its request carried.
fn system_prompt_sent(root: &Path, args: &[&str]) -> String {
    fs::write(root.join("script.json"), r#"[[{"text": "ok"}]]"#).unwrap();
    let _ = fs::remove_file(root.join("requests.jsonl"));
    let output = faux_script_noel(root, args)
        .args(["-p", "hello"])
        .output()
        .expect("run noel");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = faux_script_log(root);
    assert_eq!(log.len(), 1);
    log[0]["system_prompt"]
        .as_str()
        .expect("system prompt")
        .to_string()
}

#[test]
fn print_mode_sends_the_default_system_prompt_and_the_prompt_files() {
    let root = test_dir("noel-cli-system-prompt");
    fs::create_dir_all(root.join("agent")).unwrap();
    fs::create_dir_all(root.join(".noel")).unwrap();
    fs::write(root.join("AGENTS.md"), "Project rules.\n").unwrap();
    fs::write(root.join("agent/APPEND_SYSTEM.md"), "Answer in French.\n").unwrap();
    fs::write(root.join(".noel/SYSTEM.md"), "You are the project's bot.\n").unwrap();
    let cwd = fs::canonicalize(&root).unwrap();
    let cwd = cwd.display();
    let after_role = format!(
        "<addendum>\nAnswer in French.\n\n</addendum>\n\n\
         <project_context>\nProject-specific instructions and guidelines:\n\n\
         <project_instructions path=\"{cwd}/AGENTS.md\">\nProject rules.\n\n</project_instructions>\n\
         </project_context>\n\n\
         <cwd>\n{cwd}\n</cwd>"
    );

    // Not trusted: the project's .noel/SYSTEM.md is ignored.
    let prompt = system_prompt_sent(&root, &["--no-approve"]);
    let role = format!(
        "You are an expert coding assistant operating inside {}, a coding agent harness.",
        noel_core::PRODUCT_NAME
    );
    assert!(prompt.starts_with(&role), "{prompt}");
    assert!(
        prompt.contains("\n\n<tools>\n- read: Read file contents\n- bash: "),
        "{prompt}"
    );
    // --offline keeps the web tools out of the request, so out of the list.
    assert!(prompt.contains("\n- todo: "), "{prompt}");
    assert!(!prompt.contains("web_search"), "{prompt}");
    assert!(
        prompt.ends_with(&format!("\n</rules>\n\n{after_role}")),
        "{prompt}"
    );

    // Trusted: it replaces the role, tool list, and rules.
    assert_eq!(
        system_prompt_sent(&root, &["--approve"]),
        format!("You are the project's bot.\n\n\n{after_role}")
    );

    // Flags replace both files, whatever the trust decision.
    assert_eq!(
        system_prompt_sent(
            &root,
            &[
                "--approve",
                "--no-context-files",
                "--system-prompt",
                "Be brief.",
                "--append-system-prompt",
                "No emojis.",
            ],
        ),
        format!("Be brief.\n\n<addendum>\nNo emojis.\n</addendum>\n\n<cwd>\n{cwd}\n</cwd>")
    );
    fs::remove_dir_all(root).unwrap();
}

/// A project written for pi keeps working: noel reads its `.pi/` (here a
/// skill and `APPEND_SYSTEM.md`) while the project has no `.noel/`, and only
/// `.noel/` once it has one.
#[test]
fn a_project_written_for_pi_is_read_from_pi_until_it_has_a_noel_dir() {
    let root = fs::canonicalize({
        let root = test_dir("noel-cli-pi-project");
        fs::create_dir_all(root.join("agent")).unwrap();
        fs::create_dir_all(root.join(".pi/skills/release")).unwrap();
        root
    })
    .unwrap();
    let skill = |description: &str| format!("---\ndescription: {description}\n---\nDo it.\n");
    fs::write(
        root.join(".pi/skills/release/SKILL.md"),
        skill("Cut a release."),
    )
    .unwrap();
    fs::write(root.join(".pi/APPEND_SYSTEM.md"), "From pi's directory.\n").unwrap();
    let location = |dir: &str| {
        format!(
            "<location>{}</location>",
            root.join(dir).join("SKILL.md").display()
        )
    };
    let addendum = "<addendum>\nFrom pi's directory.\n\n</addendum>";

    let prompt = system_prompt_sent(&root, &["--approve", "--no-context-files"]);
    assert!(prompt.contains(&location(".pi/skills/release")), "{prompt}");
    assert!(prompt.contains(addendum), "{prompt}");

    // Trust gates pi's directory as it gates noel's.
    let prompt = system_prompt_sent(&root, &["--no-approve", "--no-context-files"]);
    assert!(
        !prompt.contains("<skills>") && !prompt.contains(addendum),
        "{prompt}"
    );

    fs::create_dir_all(root.join(".noel/skills/deploy")).unwrap();
    fs::write(
        root.join(".noel/skills/deploy/SKILL.md"),
        skill("Deploy the app."),
    )
    .unwrap();
    let prompt = system_prompt_sent(&root, &["--approve", "--no-context-files"]);
    assert!(
        prompt.contains(&location(".noel/skills/deploy")),
        "{prompt}"
    );
    assert!(
        !prompt.contains(".pi/") && !prompt.contains(addendum),
        "{prompt}"
    );
    fs::remove_dir_all(root).unwrap();
}

/// Print mode expands a skill command and a prompt template before the
/// model sees the prompt, and lists the skill in the system prompt.
/// `--no-skills` and `--no-prompt-templates` keep what `--skill` and
/// `--prompt-template` name and drop what is discovered.
#[test]
fn print_mode_expands_skills_and_templates_and_honors_the_resource_flags() {
    let root = fs::canonicalize({
        let root = test_dir("noel-cli-skills");
        fs::create_dir_all(root.join("agent/prompts")).unwrap();
        fs::create_dir_all(root.join("agent/skills/found")).unwrap();
        fs::create_dir_all(root.join("given/review")).unwrap();
        root
    })
    .unwrap();
    let skill = |description: &str| format!("---\ndescription: {description}\n---\nDo it.\n");
    fs::write(root.join("agent/skills/found/SKILL.md"), skill("Found.")).unwrap();
    fs::write(root.join("given/review/SKILL.md"), skill("Given.")).unwrap();
    fs::write(root.join("agent/prompts/fix.md"), "Fix $1.\n").unwrap();
    fs::write(root.join("given/explain.md"), "Explain ${1:-it}.\n").unwrap();
    fs::write(
        root.join("script.json"),
        r#"[[{"text": "ok"}], [{"text": "ok"}], [{"text": "ok"}], [{"text": "ok"}], [{"text": "ok"}]]"#,
    )
    .unwrap();
    // The prompt of each run, and its request's last user message and
    // system prompt.
    let run = |args: &[&str]| {
        let output = faux_script_noel(&root, &["--no-context-files"])
            .args(args)
            .output()
            .expect("run noel");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let request = faux_script_log(&root).pop().expect("a request");
        let prompt = request["messages"].as_array().unwrap().last().unwrap()["content"].clone();
        let system = request["system_prompt"].as_str().unwrap().to_string();
        (prompt.as_str().unwrap().to_string(), system)
    };
    let block = |name: &str, dir: &str| {
        let dir = root.join(dir);
        format!(
            "<skill name=\"{name}\" location=\"{}\">\nReferences are relative to {}.\n\nDo it.\n</skill>",
            dir.join("SKILL.md").display(),
            dir.display()
        )
    };
    let location = |dir: &str| {
        format!(
            "<location>{}</location>",
            root.join(dir).join("SKILL.md").display()
        )
    };

    // Arguments join into one prompt, which is the skill's command.
    let (prompt, system) = run(&[
        "--skill",
        "given/review",
        "-p",
        "/skill:review",
        "the parser",
    ]);
    assert_eq!(
        prompt,
        format!("{}\n\nthe parser", block("review", "given/review"))
    );
    assert!(system.contains(&location("agent/skills/found")), "{system}");
    assert!(system.contains(&location("given/review")), "{system}");

    let (prompt, _) = run(&["-p", "/fix 42"]);
    assert_eq!(prompt, "Fix 42.\n");
    let (prompt, _) = run(&["--prompt-template", "given/explain.md", "-p", "/explain"]);
    assert_eq!(prompt, "Explain it.\n");

    // Discovery off: the explicit paths still load, the discovered do not.
    let flags = [
        "--no-skills",
        "--skill",
        "given/review",
        "-np",
        "--prompt-template",
        "given",
    ];
    let (prompt, system) = run(&[&flags[..], &["-p", "/skill:review"]].concat());
    assert_eq!(prompt, block("review", "given/review"));
    assert!(!system.contains("agent/skills/found"), "{system}");
    assert!(system.contains(&location("given/review")), "{system}");
    let (prompt, _) = run(&[&flags[..], &["-p", "/fix 42 /skill:found /explain now"]].concat());
    assert_eq!(prompt, "/fix 42 /skill:found /explain now");
    fs::remove_dir_all(root).unwrap();
}

/// Prints what a bash command sees of its session and of noel, a line each:
/// the session under noel's names, then under pi's, then the process markers.
const SESSION_ENV: &str = r#"printf '%s\n' "${NOEL_SESSION_ID-unset}" "${NOEL_SESSION_FILE-unset}" "${NOEL_PROVIDER-unset}" "${NOEL_MODEL-unset}" "${NOEL_REASONING_LEVEL-unset}" "${PI_SESSION_ID-unset}" "${PI_SESSION_FILE-unset}" "${PI_PROVIDER-unset}" "${PI_MODEL-unset}" "${PI_REASONING_LEVEL-unset}" "${AI_AGENT-unset}" "${NOEL_CODING_AGENT-unset}" "${PI_CODING_AGENT-unset}""#;

/// Variables a noel started by another noel's bash command inherits, under
/// noel's names and pi's.
fn inside_another_noel(command: &mut Command) -> &mut Command {
    for prefix in ["NOEL", "PI"] {
        command
            .env(format!("{prefix}_SESSION_ID"), "parent")
            .env(format!("{prefix}_SESSION_FILE"), "/parent.jsonl")
            .env(format!("{prefix}_PROVIDER"), "parent-provider")
            .env(format!("{prefix}_MODEL"), "parent-model")
            .env(format!("{prefix}_REASONING_LEVEL"), "max");
    }
    command
}

#[test]
fn model_bash_commands_see_their_session_and_the_process_markers() {
    let root = test_dir("noel-cli-bash-session-env");
    fs::create_dir_all(root.join("agent")).unwrap();
    let call =
        serde_json::json!([{"tool_call": {"name": "bash", "arguments": {"command": SESSION_ENV}}}]);
    let done = serde_json::json!([{"text": "done"}]);
    fs::write(
        root.join("script.json"),
        serde_json::json!([call, done, call, done]).to_string(),
    )
    .unwrap();
    // What the model's bash call printed in the run of `command`.
    let seen = |command: &mut Command| {
        let output = command.output().expect("run noel");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let log = faux_script_log(&root);
        let result = log.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(result["role"], "tool");
        let lines = result["content"].as_str().unwrap().lines();
        lines.map(str::to_string).collect::<Vec<_>>()
    };

    // Without a session file there is none to name, and nothing the parent
    // noel set is passed on.
    assert_eq!(
        seen(inside_another_noel(&mut faux_script_noel(
            &root,
            &["-p", "go"]
        ))),
        [
            "ephemeral",
            "unset",
            "faux",
            "script",
            "off",
            "ephemeral",
            "unset",
            "faux",
            "script",
            "off",
            noel_core::PRODUCT_NAME,
            "true",
            "true"
        ]
    );

    // A saved session names its journal.
    let sessions = fs::canonicalize(&root).unwrap().join("sessions");
    let mut saved = noel_command();
    saved
        .current_dir(&root)
        .env("HOME", &root)
        .env("NOEL_DIR", root.join("agent"))
        .env("NOEL_FAUX_SCRIPT", root.join("script.json"))
        .env("NOEL_FAUX_SCRIPT_LOG", root.join("requests.jsonl"))
        .args(["--offline", "--model", "faux/script", "--session-dir"])
        .arg(&sessions)
        .args(["--session-id", "saved", "-p", "go"]);
    assert_eq!(
        seen(&mut saved),
        [
            "saved",
            &path_text(&sessions.join("saved.jsonl")),
            "faux",
            "script",
            "off",
            "saved",
            &path_text(&sessions.join("saved.jsonl")),
            "faux",
            "script",
            "off",
            noel_core::PRODUCT_NAME,
            "true",
            "true"
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn user_bash_commands_are_not_given_session_variables() {
    // `/bash` runs the tool without the model: it inherits noel's environment
    // as it is, which has noel's process markers.
    let root = test_dir("noel-cli-user-bash-env");
    fs::create_dir_all(&root).unwrap();
    let mut command = noel_command();
    command
        .current_dir(&root)
        .args(["--offline", "--no-session", "--model", "faux/echo", "-p"])
        .arg(format!("/bash {SESSION_ENV}"));
    let output = inside_another_noel(&mut command)
        .output()
        .expect("run noel");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Print mode ends the command's output with a line break of its own.
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .lines()
            .collect::<Vec<_>>(),
        [
            "parent",
            "/parent.jsonl",
            "parent-provider",
            "parent-model",
            "max",
            "parent",
            "/parent.jsonl",
            "parent-provider",
            "parent-model",
            "max",
            noel_core::PRODUCT_NAME,
            "true",
            "true"
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn faux_script_log_resumes_the_script_in_a_new_process() {
    let root = test_dir("noel-cli-faux-script-restart");
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
    let mut first = faux_script_noel(&root, &["-p", "first"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn noel");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fs::read_to_string(root.join("requests.jsonl")).is_ok_and(|log| log.ends_with('\n')) {
        assert!(first.try_wait().unwrap().is_none(), "first noel exited");
        assert!(Instant::now() < deadline, "first request was never logged");
        std::thread::sleep(Duration::from_millis(20));
    }
    first.kill().expect("kill first noel");
    first.wait().expect("reap first noel");

    let output = faux_script_noel(&root, &["-p", "second"])
        .output()
        .expect("run noel");
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
    let root = test_dir("noel-cli-theme-flags");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        root.join("custom.json"),
        r##"{"base":"light","colors":{"accent":"#123456"}}"##,
    )
    .unwrap();
    for theme in ["system", "light", "dark", "kimi", "default", "custom.json"] {
        let output = noel_command()
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
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
    let output = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let output = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let output = noel_command()
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
        .expect("run noel");

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
    let output = noel_command()
        .args(["--list-models", "faux"])
        .output()
        .expect("run noel");

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
    let root = test_dir("noel-cli-image-models");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let output = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
        .args(["images", "gemini"])
        .output()
        .expect("run noel images");

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
    let output = noel_command()
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
        .expect("run noel");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("faux/echo"));
}

#[test]
fn ts_style_version_alias_is_accepted() {
    let output = noel_command().arg("-v").output().expect("run noel");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("noel "));
}

#[test]
fn package_commands_manage_settings_sources() {
    let root = test_dir("noel-cli-package-commands");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let install_user = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
        .args(["install", "./user-plugin"])
        .output()
        .expect("install user package");
    assert!(
        install_user.status.success(),
        "{}",
        String::from_utf8_lossy(&install_user.stderr)
    );

    let install_project = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
        &fs::read_to_string(root.join(".noel/settings.json")).expect("project settings"),
    )
    .expect("parse project settings");
    assert_eq!(project_settings["packages"][0], "./project-plugin");

    let list = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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

    let remove = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let root = test_dir("noel-cli-package-update");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    let source = root.join("source");
    fs::create_dir_all(&source).expect("create source repo");
    git(&source, ["init"]);
    git(&source, ["config", "commit.gpgsign", "false"]);
    git(&source, ["config", "user.email", "noel@example.test"]);
    git(&source, ["config", "user.name", "noel test"]);
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

    let install = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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

    let update = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let root = test_dir("noel-cli-login-commands");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let login = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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

    let logout = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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

/// An API key is `openai`'s credential and a ChatGPT login is
/// `openai-codex`'s; `noel login` stores neither for the other provider.
#[test]
fn login_keeps_api_keys_for_openai_and_chatgpt_logins_for_openai_codex() {
    let root = test_dir("noel-cli-login-openai");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    let login = |args: &[&str]| {
        let mut command = noel_command();
        command
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
            // A login flow, were one to start, fails here instead of
            // reaching a real endpoint.
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "http://127.0.0.1:9");
        command
            .arg("login")
            .args(args)
            .output()
            .expect("noel login")
    };
    let stderr =
        |output: &std::process::Output| String::from_utf8_lossy(&output.stderr).to_string();

    let refused = login(&["openai-codex", "--api-key", "sk-test"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("openai-codex signs in with ChatGPT and takes no API key"),
        "{}",
        stderr(&refused)
    );
    // `openai` has no login to run: without a key there is nothing to store.
    let keyless = login(&["openai"]);
    assert!(!keyless.status.success());
    assert!(
        stderr(&keyless).contains("usage: noel login openai --api-key"),
        "{}",
        stderr(&keyless)
    );
    assert!(!agent.join("auth.json").exists());

    let stored = login(&["openai", "--api-key", "sk-test"]);
    assert!(stored.status.success(), "{}", stderr(&stored));
    let auth = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(agent.join("auth.json")).expect("auth.json"),
    )
    .expect("parse auth.json");
    assert_eq!(
        auth,
        serde_json::json!({ "openai": { "default": { "type": "api_key", "key": "sk-test" } } })
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn login_logout_round_trip_named_accounts() {
    let root = test_dir("noel-cli-named-accounts");
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
        let login = noel_command()
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
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

    let logout = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let root = test_dir("noel-cli-login-lock");
    let agent = root.join("agent");
    let auth_path = agent.join("auth.json");
    let held = noel_config::AuthFileLock::try_acquire(&auth_path)
        .expect("lock auth.json")
        .expect("free lock");

    let mut login = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
    let root = test_dir("noel-cli-accounts-status");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");

    let base = |args: &[&str]| {
        let mut command = noel_command();
        command
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
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
    let root = test_dir("noel-cli-continue");
    let sessions = root.join("sessions");
    let sessions_arg = path_text(&sessions);
    fs::create_dir_all(&root).expect("create root");

    let first = noel_command()
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

    let second = noel_command()
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
    let root = test_dir("noel-cli-settings-session-dir");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"sessionDir":"settings-sessions"}"#,
    )
    .expect("write settings");

    let output = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
        .args(["-p", "--model", "faux/echo", "hello"])
        .output()
        .expect("run noel");

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
fn rpc_mode_keeps_an_untrusted_project_untrusted() {
    use std::io::BufRead;
    let root = test_dir("noel-cli-rpc-trust");
    let agent = root.join("agent");
    let project = root.join("project");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::create_dir_all(project.join(".noel")).expect("create project config dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    fs::write(
        project.join(".noel/settings.json"),
        r#"{"shellCommandPrefix":"echo project-prefix"}"#,
    )
    .expect("write project settings");

    // The output of `/bash echo hi`, run without the model. RPC mode has no
    // reload: every command uses the config startup built.
    let bash_output = |trust: &str| {
        let mut child = noel_command()
            .current_dir(&project)
            .env("HOME", &root)
            .env("NOEL_DIR", &agent)
            .args(["--mode", "rpc", "--no-session", "--model", "faux/echo"])
            .arg(trust)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn noel");
        let mut stdin = child.stdin.take().expect("child stdin");
        let mut stdout = std::io::BufReader::new(child.stdout.take().expect("child stdout"));
        let mut until = |done: &dyn Fn(&serde_json::Value) -> bool| loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).expect("read record") > 0,
                "noel exited early"
            );
            let record = serde_json::from_str::<serde_json::Value>(&line).expect("json record");
            if done(&record) {
                return record;
            }
        };
        writeln!(stdin, r#"{{"type":"prompt","message":"/bash echo hi"}}"#).expect("write prompt");
        until(&|record| record["type"] == "agent_settled");
        writeln!(stdin, r#"{{"id":"messages","type":"get_messages"}}"#).expect("write request");
        let reply = until(&|record| record["id"] == "messages");
        drop(stdin);
        let output = child.wait_with_output().expect("wait for noel");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let messages = reply["data"]["messages"].as_array().expect("messages");
        let result = messages
            .iter()
            .find(|message| message["role"] == "toolResult")
            .unwrap_or_else(|| panic!("no tool result in {reply}"));
        result["content"][0]["text"]
            .as_str()
            .expect("tool output")
            .trim()
            .to_string()
    };

    // An untrusted repository's settings must not take effect; a trusted
    // one's do, so the prefix would show.
    assert_eq!(bash_output("--no-approve"), "hi");
    assert_eq!(bash_output("--approve"), "project-prefix\nhi");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn print_mode_expands_at_file_and_exports_session() {
    let root = test_dir("noel-cli-at-file-export");
    let sessions = root.join("sessions");
    let export = root.join("export.json");
    let prompt = root.join("prompt.txt");
    let sessions_arg = path_text(&sessions);
    let export_arg = path_text(&export);
    fs::create_dir_all(&root).expect("create root");
    fs::write(&prompt, "from file").expect("write prompt");

    let output = noel_command()
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
        .expect("run noel");

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
    let root = test_dir("noel-cli-block-images");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"images":{"blockImages":true}}"#,
    )
    .expect("write settings");

    let output = noel_command()
        .current_dir(&root)
        .env("NOEL_DIR", &agent)
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
        .expect("run noel");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("images are blocked by settings"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn print_mode_exports_html_and_jsonl_sessions() {
    let root = test_dir("noel-cli-export-formats");
    let sessions = root.join("sessions");
    let html = root.join("export.html");
    let jsonl = root.join("export.jsonl");
    let sessions_arg = path_text(&sessions);
    let html_arg = path_text(&html);
    let jsonl_arg = path_text(&jsonl);
    fs::create_dir_all(&root).expect("create root");

    let html_output = noel_command()
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
        .expect("run noel html export");
    assert!(
        html_output.status.success(),
        "{}",
        String::from_utf8_lossy(&html_output.stderr)
    );

    let jsonl_output = noel_command()
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
        .expect("run noel jsonl export");
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
    let root = test_dir("noel-cli-session-ref-fork");
    let sessions = root.join("sessions");
    let sessions_arg = path_text(&sessions);
    fs::create_dir_all(&root).expect("create root");

    let first = noel_command()
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

    let second = noel_command()
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

    let forked = noel_command()
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

/// A noel command for one isolated home, agent dir, and session dir.
fn isolated_noel(root: &Path, args: &[&str]) -> Command {
    let agent = root.join("agent");
    fs::create_dir_all(&agent).expect("create agent dir");
    fs::write(
        agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    let mut command = noel_command();
    command
        .current_dir(root)
        .env("HOME", root)
        .env("NOEL_DIR", &agent)
        .args(["--offline", "--model", "faux/echo", "--session-dir"])
        .arg(root.join("sessions"))
        .args(args);
    command
}

#[test]
fn a_session_open_in_another_process_makes_startup_wait_then_exit_75() {
    let root = test_dir("noel-cli-session-lock");
    let mut holder = isolated_noel(&root, &["--mode", "rpc", "--session-id", "shared"])
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
    let blocked = isolated_noel(&root, &["-p", "--session-id", "shared", "hi"])
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
            "in use by another noel process (pid {})",
            holder.id()
        )),
        "{stderr}"
    );
    assert!(blocked.stdout.is_empty());

    // SIGKILL runs no cleanup and leaves the lock file behind; the kernel
    // still releases the lock, so the session opens right away.
    holder.kill().expect("kill rpc holder");
    holder.wait().expect("reap rpc holder");
    let after = isolated_noel(&root, &["-p", "--session-id", "shared", "hi"])
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
    let root = test_dir("noel-cli-damaged-sessions");
    let first = isolated_noel(&root, &["-p", "first"]).output().unwrap();
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
    let second = isolated_noel(&root, &["-p", "--continue", "second"])
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
    let resumed = isolated_noel(
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

#[test]
fn first_run_imports_pi_state_once_and_continues_its_session() {
    let root = test_dir("noel-cli-pi-import");
    let home = root.join("home");
    let pi_agent = home.join(".pi/agent");
    fs::create_dir_all(&pi_agent).expect("create pi agent dir");
    fs::write(
        pi_agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    fs::write(
        pi_agent.join("models.json"),
        r#"[{"provider":"faux","id":"imported","api":"faux"}]"#,
    )
    .expect("write models");
    fs::write(
        pi_agent.join("auth.json"),
        r#"{"openai":{"default":{"type":"api_key","key":"dummy-pi-key"}}}"#,
    )
    .expect("write auth");
    let print = |noel_dir: Option<&Path>, args: &[&str]| {
        let mut command = noel_command();
        command.current_dir(&root).env("HOME", &home);
        match noel_dir {
            Some(dir) => command.env("NOEL_DIR", dir),
            // The default directory, in this test's own HOME; the harness
            // marker would refuse it.
            None => command
                .env_remove("NOEL_DIR")
                .env_remove("NOEL_TEST_ISOLATION"),
        };
        command
            .args(["-p", "--offline", "--model", "faux/echo"])
            .args(args)
            .output()
            .expect("run noel")
    };
    // A session in pi's current format, written into pi's directory.
    let first = print(Some(&pi_agent), &["before the import"]);
    assert!(first.status.success(), "{first:?}");
    let pi_files = files_under(&home.join(".pi"));

    let second = print(None, &["--continue", "after the import"]);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(second.status.success(), "{stderr}");
    assert!(
        stderr.contains("noel: imported settings.json, models.json, auth.json, sessions from"),
        "{stderr}"
    );
    assert_eq!(
        String::from_utf8_lossy(&second.stdout),
        "[faux/echo] after the import\n"
    );
    let journals = files_under(&home.join(".noel/sessions"))
        .into_iter()
        .filter(|(path, _)| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(journals.len(), 1);
    let log = String::from_utf8_lossy(&journals[0].1);
    assert!(
        log.contains("before the import") && log.contains("after the import"),
        "{log}"
    );
    assert_eq!(
        fs::read(home.join(".noel/auth.json")).expect("noel auth"),
        fs::read(pi_agent.join("auth.json")).expect("pi auth")
    );
    assert_eq!(files_under(&home.join(".pi")), pi_files);

    let third = print(None, &["--continue", "once more"]);
    assert!(third.status.success(), "{third:?}");
    assert!(!String::from_utf8_lossy(&third.stderr).contains("imported"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pi_import_skips_files_noel_cannot_load_and_never_runs_with_noel_dir() {
    let root = test_dir("noel-cli-pi-import-skips");
    let home = root.join("home");
    let pi_agent = home.join(".pi/agent");
    fs::create_dir_all(&pi_agent).expect("create pi agent dir");
    fs::write(
        pi_agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    fs::write(pi_agent.join("models.json"), "{ not json").expect("write models");
    let print = |command: &mut Command| {
        command
            .current_dir(&root)
            .env("HOME", &home)
            .args([
                "-p",
                "--offline",
                "--no-session",
                "--model",
                "faux/echo",
                "hi",
            ])
            .output()
            .expect("run noel")
    };

    let overridden = print(noel_command().env("NOEL_DIR", root.join("custom")));
    assert!(overridden.status.success(), "{overridden:?}");
    assert!(!home.join(".noel").exists());

    // Under the test harness noel refuses its default directory before it
    // imports anything.
    let refused = print(noel_command().env_remove("NOEL_DIR"));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{stderr}");
    assert!(
        stderr.contains("refusing the default agent dir"),
        "{stderr}"
    );
    assert!(!home.join(".noel").exists());

    let imported = print(
        noel_command()
            .env_remove("NOEL_DIR")
            .env_remove("NOEL_TEST_ISOLATION"),
    );
    let stderr = String::from_utf8_lossy(&imported.stderr);
    assert!(imported.status.success(), "{stderr}");
    assert!(
        stderr.contains("noel: imported settings.json from"),
        "{stderr}"
    );
    assert!(
        stderr.contains("noel: did not import models.json: failed to parse"),
        "{stderr}"
    );
    assert!(home.join(".noel/settings.json").exists());
    assert!(!home.join(".noel/models.json").exists());
    fs::remove_dir_all(root).unwrap();
}

/// A home that has only pi's state, and a noel on its default directory
/// there: no `NOEL_DIR`, and no harness marker, which would refuse it.
fn home_with_pi_state(name: &str) -> (PathBuf, PathBuf) {
    let root = test_dir(name);
    let home = root.join("home");
    let pi_agent = home.join(".pi/agent");
    fs::create_dir_all(pi_agent.join("sessions")).expect("create pi agent dir");
    fs::write(
        pi_agent.join("settings.json"),
        r#"{"modelRefresh":{"enabled":false}}"#,
    )
    .expect("write settings");
    fs::write(
        pi_agent.join("auth.json"),
        r#"{"openai":{"default":{"type":"api_key","key":"dummy-pi-key"}}}"#,
    )
    .expect("write auth");
    (root, home)
}

fn noel_on_default_dir(root: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    noel_command()
        .current_dir(root)
        .env("HOME", home)
        .env_remove("NOEL_DIR")
        .env_remove("NOEL_TEST_ISOLATION")
        .args(args)
        .output()
        .expect("run noel")
}

/// Fails unless `home` still holds pi's state, unchanged, and nothing else:
/// no `~/.noel`, and no directory an import was staged in.
fn assert_nothing_imported(what: &str, home: &Path, pi_files: &[(PathBuf, Vec<u8>)]) {
    let entries = fs::read_dir(home)
        .expect("read home")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, [".pi"], "{what} wrote to the home directory");
    assert_eq!(files_under(&home.join(".pi")), pi_files, "{what}");
}

/// A version probe writes nothing: scripts and supervisors ask for the
/// version, and asking must not import pi's state.
#[test]
fn version_probe_imports_nothing() {
    let (root, home) = home_with_pi_state("noel-cli-pi-import-version");
    let pi_files = files_under(&home.join(".pi"));
    for flag in ["--version", "-v"] {
        let output = noel_on_default_dir(&root, &home, &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("noel {}\n", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{flag}");
        assert_nothing_imported(flag, &home, &pi_files);
    }
    fs::remove_dir_all(root).unwrap();
}

/// Help writes nothing, for noel and for each of its commands.
#[test]
fn help_imports_nothing() {
    let (root, home) = home_with_pi_state("noel-cli-pi-import-help");
    let pi_files = files_under(&home.join(".pi"));
    for args in [
        &["--help"][..],
        &["-h"],
        &["login", "--help"],
        &["logout", "-h"],
        &["accounts", "--help"],
        &["auth", "--help"],
        &["auth"],
        &["install", "--help"],
        &["remove", "--help"],
        &["update", "--help"],
        &["list", "--help"],
        &["config", "--help"],
        &["images", "--help"],
        &["generate-image", "--help"],
    ] {
        let output = noel_on_default_dir(&root, &home, args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .to_lowercase()
                .contains("usage"),
            "{args:?}: {output:?}"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{args:?}");
        assert_nothing_imported(&format!("{args:?}"), &home, &pi_files);
    }
    fs::remove_dir_all(root).unwrap();
}

/// A usage error writes nothing: arguments are checked before any command
/// asks for its directories. The first command that does ask imports.
#[test]
fn usage_errors_import_nothing_and_the_first_command_that_loads_configuration_imports() {
    let (root, home) = home_with_pi_state("noel-cli-pi-import-usage");
    let pi_files = files_under(&home.join(".pi"));
    for args in [
        &["--no-such-flag"][..],
        &["--mode", "bogus"],
        &["--mode", "rpc", "a prompt"],
        &["--session-id", "a", "--continue", "-p", "hi"],
        &["login"],
        &["login", "openai"],
        &["login", "openai-codex", "--api-key", "dummy"],
        &["login", "--no-such-option"],
        &["logout"],
        &["accounts"],
        &["accounts", "bogus"],
        &["auth", "bogus"],
        &["auth", "check"],
        &["install"],
        &["remove"],
        &["update", "one", "two"],
        &["list", "bogus"],
        &["config", "disable"],
        &["config", "disable", "bogus-kind", "name"],
        &["generate-image", "a prompt without an output"],
        &["mcp", "list"],
    ] {
        let output = noel_on_default_dir(&root, &home, args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("imported"), "{args:?}: {stderr}");
        assert_nothing_imported(&format!("{args:?}"), &home, &pi_files);
    }

    // `noel list` reads the settings, so it is the first to need ~/.noel.
    let output = noel_on_default_dir(&root, &home, &["list"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("noel: imported settings.json, auth.json, sessions from"),
        "{stderr}"
    );
    assert!(home.join(".noel/auth.json").exists());
    assert_eq!(files_under(&home.join(".pi")), pi_files);
    fs::remove_dir_all(root).unwrap();
}

/// Every file under `dir` with its contents.
fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                let content = fs::read(&path).expect("read file");
                files.push((path, content));
            }
        }
    }
    files.sort();
    files
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
    use std::io::{BufRead, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    let root = test_dir("noel-cli-semantic-rpc");
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
    let mut command = noel_command();
    command
        .current_dir(&root)
        .env("HOME", &root)
        .env("NOEL_DIR", &agent)
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
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    // Each command waits for the previous run to settle.
    let mut until = |done: &dyn Fn(&serde_json::Value) -> bool| loop {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "noel exited early"
        );
        let record = serde_json::from_str::<serde_json::Value>(&line).unwrap();
        if done(&record) {
            return record;
        }
    };
    let settled = |record: &serde_json::Value| record["type"] == "agent_settled";
    for prompt in [
        format!("use SQLite transactions {}", "reference ".repeat(300)),
        "current migration".into(),
    ] {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"type":"prompt","message":prompt})
        )
        .unwrap();
        assert_eq!(until(&settled)["aborted"], false);
    }
    writeln!(stdin, "{}", serde_json::json!({"id":"compact","type":"compact","customInstructions":"preserve database decisions"})).unwrap();
    let compacted = until(&|record| record["id"] == "compact");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"type":"prompt","message":"continue"})
    )
    .unwrap();
    until(&settled);
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
    assert_eq!(compacted["success"], true, "{compacted}");
    assert_eq!(compacted["data"]["details"]["omittedMessages"], 2);
    assert!(compacted["data"]["summary"]
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
    // The system prompt leads every request; the checkpoint follows it.
    assert_eq!(requests[3]["messages"][0]["role"], "system");
    assert!(requests[3]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are an expert coding assistant"));
    assert!(requests[3]["messages"][1]["content"]
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
    let loaded = noel_core::SessionStore::read(&journal_path).unwrap();
    assert_eq!(loaded.compactions.len(), 1);
    assert_eq!(loaded.usage.requests, 4);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn upstream_v1_1_flags_are_accepted_in_print_mode() {
    let root = test_dir("noel-cli-v1-1-flags");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    let run = |extra: &[&str]| {
        noel_command()
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
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
    let output = noel_command().args(["mcp", "list"]).output().unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no MCP support"));
    assert!(output.stdout.is_empty());
}

#[test]
fn auth_commands_print_and_check_stored_credentials() {
    let root = test_dir("noel-cli-auth-command");
    let agent = root.join("agent");
    fs::create_dir_all(&agent).unwrap();
    fs::write(
        agent.join("auth.json"),
        r#"{"anthropic":{"default":{"type":"api_key","key":"sk-ant-synthetic"}}}"#,
    )
    .unwrap();
    let auth = |args: &[&str]| {
        noel_command()
            .current_dir(&root)
            .env("NOEL_DIR", &agent)
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
    assert!(stdout(&help).contains("noel auth print-bearer-token"));

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

/// Claude Code and Codex logins noel reads from their own files: refreshing one
/// early would spend that CLI's single-use refresh token and sign it out.
#[test]
fn auth_commands_leave_imported_logins_unrefreshed_until_they_expire() {
    use base64::Engine as _;

    let root = test_dir("noel-cli-auth-imports");
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
        let mut command = noel_command();
        command
            .current_dir(&root)
            .env("HOME", &root)
            .env("NOEL_DIR", &agent)
            // NOEL_TEST_ISOLATION switches the login import off. This test
            // checks that import against login files in its own HOME, so it
            // runs without the marker.
            .env_remove("NOEL_TEST_ISOLATION")
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
    // Codex's login is `openai-codex`'s credential; `openai` takes an API
    // key and has none.
    let openai = auth(&["check", "--provider", "openai"]);
    assert_eq!(openai.status.code(), Some(1), "{}", stderr(&openai));
    assert_eq!(stdout(&openai), "not_ready\n");

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

    // Nothing was refreshed: noel stored no copy and the logins are untouched.
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
fn fatal_signals_during_a_print_mode_bash_tool_kill_its_tree_and_end_noel_like_upstream() {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::time::{Duration, Instant};
    let root = test_dir("noel-cli-signal-bash");
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
        // one-response provider, so a dying noel cannot take the next reply.
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
        let mut noel = noel_command()
            .current_dir(&root)
            .env("HOME", &root)
            .env("NOEL_DIR", &agent)
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
            if Instant::now() >= deadline || noel.try_wait().unwrap().is_some() {
                let _ = noel.kill();
                let output = noel.wait_with_output().unwrap();
                panic!(
                    "the bash tool never started: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(Command::new("kill")
            .args([&format!("-{signal}"), &noel.id().to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = noel.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = noel.kill();
                panic!("noel ignored SIG{signal}");
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
        noel.stderr
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
