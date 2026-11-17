//! Remote access (roadmap phase 11, spec §16): `noel server` with
//! `server.listen` set; a device (its own agent dir) pairs with `noel client
//! pair`, the owner approves its code from the device socket, and the
//! device uses the server with `--server`; a wrong token and a wrong
//! certificate pin are refused; a revoked device is locked out.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn noel(&self, agent: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_noel"));
        command
            .current_dir(self.root.join("cwd"))
            .env("HOME", self.root.join("home"))
            .env("NOEL_DIR", self.root.join(agent))
            .env("NOEL_OFFLINE", "1")
            .env("NOEL_RUNTIME_DIR", self.root.join(format!("run-{agent}")))
            .env_remove("NOEL_SESSION_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .args(args)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, agent: &str, args: &[&str]) -> Output {
        let output = self.noel(agent, args).output().expect("run noel");
        eprintln!(
            "$ [{agent}] noel {} -> {}\n{}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn ok(&self, agent: &str, args: &[&str]) -> String {
        let output = self.run(agent, args);
        assert!(output.status.success(), "noel {args:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let run = self.root.join("run-owner");
        if std::thread::panicking()
            && let Ok(log) = fs::read_to_string(run.join("server.log"))
        {
            eprintln!("--- server.log ---\n{log}--- end ---");
        }
        if let Ok(text) = fs::read_to_string(run.join("server.json"))
            && let Some(pid) = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|r| r["pid"].as_u64())
        {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn set_json(path: &Path, change: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    change(&mut value);
    fs::write(path, value.to_string()).unwrap();
}

#[test]
fn a_device_pairs_uses_the_server_and_is_revoked() {
    let root = std::env::temp_dir().join(format!("nrm-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for dir in ["home", "owner", "phone", "cwd"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let port = free_port();
    let settings = json!({"modelRefresh": {"enabled": false}, "server": {"listen": format!("127.0.0.1:{port}")}});
    fs::write(root.join("owner/settings.json"), settings.to_string()).unwrap();
    let scratch = Scratch { root };
    let url = format!("https://127.0.0.1:{port}");

    // The owner's server, listening.
    let devices = scratch.ok("owner", &["client", "devices"]);
    assert!(
        devices.contains(&format!("listening: 127.0.0.1:{port}")),
        "{devices}"
    );
    let fingerprint = devices
        .lines()
        .find_map(|line| line.strip_prefix("certificate: "))
        .unwrap()
        .to_owned();

    // A device pairs: it shows a code; the owner approves it.
    let mut pairing = scratch
        .noel("phone", &["client", "pair", &url, "--label", "phone"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(pairing.stdout.take().unwrap()).lines();
    let code = lines
        .next()
        .unwrap()
        .unwrap()
        .strip_prefix("pairing code: ")
        .unwrap()
        .to_owned();
    let seen = lines.next().unwrap().unwrap();
    assert_eq!(seen, format!("server certificate: {fingerprint}"));
    assert!(scratch
        .ok("owner", &["client", "devices"])
        .contains(&format!("pending {code} phone")));
    assert!(scratch
        .ok("owner", &["client", "devices", "approve", &code])
        .starts_with("approved: device d-"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = pairing.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the pairing never finished");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success());
    // The code is used up.
    assert!(!scratch
        .run("owner", &["client", "devices", "approve", &code])
        .status
        .success());

    // The device uses the server.
    assert_eq!(
        scratch
            .ok("phone", &["client", "--server", &url, "ls"])
            .trim(),
        "no sessions"
    );
    let listed = scratch.ok("owner", &["client", "devices"]);
    let id = listed
        .lines()
        .find_map(|line| {
            line.strip_prefix("device ")
                .and_then(|rest| rest.strip_suffix(" phone"))
        })
        .unwrap()
        .to_owned();
    // A paired device cannot pair or revoke devices.
    assert!(!scratch
        .run("phone", &["client", "--server", &url, "devices"])
        .status
        .success());

    // A wrong token, and a wrong pin, are refused.
    let remotes = scratch.root.join("phone/remotes.json");
    let saved = fs::read_to_string(&remotes).unwrap();
    let key = format!("127.0.0.1:{port}");
    set_json(&remotes, |value| {
        value[&key]["token"] = json!("0".repeat(64))
    });
    assert!(!scratch
        .run("phone", &["client", "--server", &url, "ls"])
        .status
        .success());
    fs::write(&remotes, &saved).unwrap();
    set_json(&remotes, |value| value[&key]["pin"] = json!("0".repeat(64)));
    let wrong_pin = scratch.run("phone", &["client", "--server", &url, "ls"]);
    assert!(!wrong_pin.status.success());
    assert!(String::from_utf8_lossy(&wrong_pin.stderr).contains("not the paired one"));
    fs::write(&remotes, &saved).unwrap();

    // Revoked, the device is locked out.
    scratch.ok("owner", &["client", "devices", "revoke", &id]);
    assert!(!scratch
        .run("phone", &["client", "--server", &url, "ls"])
        .status
        .success());
    // Without a pairing, `--server` says so.
    let unpaired = scratch.run("home", &["client", "--server", &url, "ls"]);
    assert!(String::from_utf8_lossy(&unpaired.stderr).contains("not paired"));
}
