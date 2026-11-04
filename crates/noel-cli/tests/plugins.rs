//! Plugins end to end (roadmap phase 7, spec §11): `noel server` starts the
//! sample plugin (`noel-sample-plugin`) from its manifest, and `noel client`
//! sees its contributions, runs its command and its panel's action; a
//! session's model calls its tool; a permission it was not granted is
//! refused; and a crash restarts it.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(40);

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let root = std::env::temp_dir().join(format!("npl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["home", "agent/plugins/sample", "cwd"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let settings = json!({"modelRefresh": {"enabled": false}, "retry": {"enabled": false}});
        fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
        // The model calls the plugin's tool, then answers.
        let script = json!([
            [{"tool_call": {"name": "sample_reverse", "arguments": {"text": "plugin"}}}],
            [{"text": "Reversed."}],
        ]);
        fs::write(root.join("script.json"), script.to_string()).unwrap();
        let manifest = json!({
            "name": "sample",
            "version": "1.0.0",
            "description": "The sample plugin.",
            "command": [env!("CARGO_BIN_EXE_noel-sample-plugin")],
            "permissions": ["tools", "commands", "panels", "sessions"],
        });
        fs::write(
            root.join("agent/plugins/sample/plugin.json"),
            manifest.to_string(),
        )
        .unwrap();
        // Everything it asks for but `sessions`.
        let grants = json!({"sample": {"permissions": ["tools", "commands", "panels"]}});
        fs::write(root.join("agent/plugins.json"), grants.to_string()).unwrap();
        Scratch { root }
    }

    fn noel(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_noel"))
            .current_dir(self.root.join("cwd"))
            .env("HOME", self.root.join("home"))
            .env("NOEL_DIR", self.root.join("agent"))
            .env("NOEL_FAUX_SCRIPT", self.root.join("script.json"))
            .env("NOEL_OFFLINE", "1")
            .env("NOEL_RUNTIME_DIR", self.root.join("run"))
            .env_remove("NOEL_SESSION_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run noel");
        eprintln!(
            "$ noel {} -> {}\n{}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn client(&self, args: &[&str]) -> String {
        let mut full = vec!["client"];
        full.extend_from_slice(args);
        let output = self.noel(&full);
        assert!(output.status.success(), "noel {full:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }

    fn plugin(&self) -> Value {
        let directory: Value = serde_json::from_str(&self.client(&["plugins", "--json"])).unwrap();
        directory["plugins"][0].clone()
    }

    /// Waits until the plugin's directory entry satisfies `done`.
    fn plugin_until(&self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let plugin = self.plugin();
            if done(&plugin) {
                return plugin;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {plugin}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn journal(&self, session: &str) -> String {
        fs::read_to_string(self.root.join(format!("agent/sessions/{session}.jsonl")))
            .unwrap_or_default()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let run = self.root.join("run");
        if std::thread::panicking() {
            if let Ok(log) = fs::read_to_string(run.join("server.log")) {
                eprintln!("--- server.log ---\n{log}--- end ---");
            }
            for dir in ["workers", "plugins"] {
                for entry in fs::read_dir(run.join(dir)).into_iter().flatten().flatten() {
                    if entry.path().extension().is_some_and(|ext| ext == "log") {
                        let log = fs::read_to_string(entry.path()).unwrap_or_default();
                        eprintln!("--- {} ---\n{log}--- end ---", entry.path().display());
                    }
                }
            }
        }
        let mut records = vec![run.join("server.json")];
        if let Ok(entries) = fs::read_dir(run.join("workers")) {
            records.extend(entries.flatten().map(|entry| entry.path()));
        }
        for record in records {
            let Ok(text) = fs::read_to_string(&record) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(pid) = record["pid"].as_u64() {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn panel_text(plugin: &Value) -> String {
    plugin["contributions"]["panels"][0]["view"]["children"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_plugin_adds_a_tool_a_command_and_a_panel_and_restarts_after_a_crash() {
    let scratch = Scratch::new();
    let plugin = scratch.plugin_until("the plugin running", |plugin| {
        plugin["running"] == json!(true)
            && plugin["contributions"]["tools"][0]["name"] == json!("sample_reverse")
    });
    assert_eq!(plugin["granted"], json!(["tools", "commands", "panels"]));
    assert_eq!(panel_text(&plugin), "count 0");

    // A command, and the panel it changes.
    assert_eq!(
        scratch.client(&["run", "sample", "count"]).trim(),
        "count 1"
    );
    scratch.plugin_until("the panel at 1", |plugin| panel_text(plugin) == "count 1");
    let listed = scratch.client(&["plugins"]);
    assert!(listed.contains("    count 1"), "{listed}");
    assert!(listed.contains("[Reset] (reset)"), "{listed}");
    // The panel's button.
    scratch.client(&["panel", "sample", "counter", "reset"]);
    scratch.plugin_until("the panel reset", |plugin| panel_text(plugin) == "count 0");

    // A permission it asked for but was not granted is refused.
    let denied = scratch.noel(&["client", "run", "sample", "sessions"]);
    assert!(!denied.status.success());
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(stderr.contains("forbidden"), "{stderr}");

    // A session's model calls the plugin's tool.
    let session = scratch
        .client(&["new", "--model", "faux/script"])
        .trim()
        .to_owned();
    scratch.client(&["send", &session, "reverse it"]);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let journal = scratch.journal(&session);
        if journal.contains("nigulp") && journal.contains("Reversed.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no tool result in the journal: {journal}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // A crash: the server starts it again, with a fresh count.
    let crashed = scratch.noel(&["client", "run", "sample", "crash"]);
    assert!(!crashed.status.success());
    scratch.plugin_until("the plugin restarted", |plugin| {
        plugin["running"] == json!(true) && plugin["restarts"].as_u64() >= Some(1)
    });
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let output = scratch.noel(&["client", "run", "sample", "count"]);
        if output.status.success() {
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "count 1");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the restarted plugin never answered"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
