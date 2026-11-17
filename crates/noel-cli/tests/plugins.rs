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
    fn new(name: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("npl{name}-{}", std::process::id()));
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
    let scratch = Scratch::new("p");
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

const SCRIPT_PLUGIN: &str = r#"
const view = (state) => ({ type: "stack", children: [
  { type: "text", text: `count ${state?.count ?? 0}` },
  { type: "button", label: "Reset", action: "reset" },
] });
const plugin = {
  tools: {
    shout: {
      description: "Says the text louder.",
      parameters: { type: "object", required: ["text"], properties: { text: { type: "string" } } },
      run: async (args) => args.text.toUpperCase(),
    },
  },
  commands: {
    count: {
      description: "Counts one more.",
      run: async (_, ctx) => {
        const count = (ctx.state?.count ?? 0) + 1;
        ctx.save({ count });
        return `count ${count}`;
      },
    },
    spin: { description: "Never ends.", run: async () => { for (;;) {} } },
  },
  panels: {
    counter: { title: "Counter", view, action: async (action, _, ctx) => { if (action === "reset") ctx.save({ count: 0 }); } },
  },
};
"#;

/// A djs plugin (phase 8): its contributions, its state across a server
/// restart, and a runaway call stopped by its time limit.
#[test]
fn a_script_plugin_runs_in_process_with_state_and_limits() {
    let scratch = Scratch::new("s");
    // Only the script plugin.
    fs::remove_dir_all(scratch.root.join("agent/plugins/sample")).unwrap();
    let dir = scratch.root.join("agent/plugins/script");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("main.js"), SCRIPT_PLUGIN).unwrap();
    let manifest = json!({
        "name": "script",
        "script": "main.js",
        // The limit of every call, not only the runaway one: long enough
        // for the others on a loaded machine in an unoptimized build.
        "limits": {"timeoutMs": 3000},
        "permissions": ["tools", "commands", "panels"],
    });
    fs::write(dir.join("plugin.json"), manifest.to_string()).unwrap();
    let grants = json!({"script": {"permissions": ["tools", "commands", "panels"]}});
    fs::write(scratch.root.join("agent/plugins.json"), grants.to_string()).unwrap();

    let plugin = scratch.plugin_until("the script plugin running", |plugin| {
        plugin["running"] == json!(true)
            && plugin["contributions"]["tools"][0]["name"] == json!("shout")
    });
    assert_eq!(panel_text(&plugin), "count 0");
    assert_eq!(
        scratch.client(&["run", "script", "count"]).trim(),
        "count 1"
    );
    scratch.plugin_until("the panel at 1", |plugin| panel_text(plugin) == "count 1");

    // A runaway command ends at its time limit; the plugin goes on.
    let spun = scratch.noel(&["client", "run", "script", "spin"]);
    assert!(!spun.status.success());
    assert!(
        String::from_utf8_lossy(&spun.stderr).contains("timeout"),
        "{}",
        String::from_utf8_lossy(&spun.stderr)
    );
    assert_eq!(
        scratch.client(&["run", "script", "count"]).trim(),
        "count 2"
    );

    // The state survives the server: kill it, and the next client starts a
    // new one.
    let record: Value =
        serde_json::from_str(&fs::read_to_string(scratch.root.join("run/server.json")).unwrap())
            .unwrap();
    let pid = record["pid"].as_u64().unwrap().to_string();
    assert!(Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + TIMEOUT;
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        assert!(Instant::now() < deadline, "the server did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    scratch.plugin_until("the script plugin running again", |plugin| {
        plugin["running"] == json!(true) && panel_text(plugin) == "count 2"
    });
    assert_eq!(
        scratch.client(&["run", "script", "count"]).trim(),
        "count 3"
    );
    scratch.client(&["panel", "script", "counter", "reset"]);
    scratch.plugin_until("the panel reset", |plugin| panel_text(plugin) == "count 0");
}
