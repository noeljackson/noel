//! noel-env's SSH deployment (`noel_env::ssh`) through a stand-in for `ssh` (`scripts/ts-parity/fake-ssh.sh`): it
//! checks host keys as ssh does against the known-hosts file and alias it is given, and runs each remote command
//! here, in a scratch home. Nothing touches the user's ssh configuration, keys, agent or known hosts.
//!
//! - `invokes_ssh_as_upstream_does` compares every invocation (options, host, detection, the daemon check, the
//!   upload script, the launch) with what upstream's `ssh.ts` did through the same stand-in
//!   (`tests/fixtures/ts-parity/pi-env-ssh.json`, `scripts/ts-parity/pi-env-ssh-fixture.ts`). They differ only by
//!   the names in `RENAMES`.
//! - The other tests port upstream's `packages/env/test/ssh.test.ts`, which uses a disposable `sshd` where these use
//!   the stand-in, and deploy and run `noel-env serve` (or `NOEL_ENV_TEST_DAEMON`).
//!
//! Ported in part from upstream pi's `packages/env/test/ssh.test.ts` (<https://github.com/earendil-works/pi>, commit
//! `1692bba`), Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root).

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use noel_env::ssh::{
    accept_host_key, connect_ssh, daemon_name, forget_host_key, scan_host_key, ssh_arguments,
    ssh_connection, RemoteArch, RemotePlatform, RemoteSystem, SshConnectOptions, SshError,
    SshTarget,
};
use noel_env::{
    Connection, Context, ExecOptions, ExecutionErrorCode, FileKind, FileSystem, ListFilesOptions,
    OutputChunk, RemoteEnv, RemoteEnvOptions, Shell, ShellCommand,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use support::{daemon, Scratch};

const FAKE_SSH: &str = include_str!("../../../scripts/ts-parity/fake-ssh.sh");
const FIXTURE: &str = include_str!("../../../tests/fixtures/ts-parity/pi-env-ssh.json");
const MAKEFILE: &str = include_str!("../../../Makefile");

/// Where upstream's deployment says `pi-env` and noel's says `noel-env`; nothing else may differ.
const RENAMES: &[(&str, &str)] = &[
    // The daemon's home: upstream's under `~/.pi`, noel's in its own cache directory.
    ("/.pi/mobile/tools/pi-env-", "/.cache/noel-env/noel-env-"),
    (".pi-env.XXXXXX", ".noel-env.XXXXXX"),
    ("pi-env upload is corrupt", "noel-env upload is corrupt"),
    (r#""$d"/pi-env-*"#, r#""$d"/noel-env-*"#),
];

fn cx() -> Context {
    Context::background()
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A `type key` public key, generated for the test.
fn new_key(path: &Path) -> String {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .stdin(Stdio::null())
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
    let public = std::fs::read_to_string(path.with_extension("pub")).unwrap();
    public.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

/// A "remote machine" behind the stand-in: a scratch home, a host key, and the log of ssh invocations.
struct FakeHost {
    root: Scratch,
    home: PathBuf,
    log: PathBuf,
    host_key_pub: PathBuf,
    target: SshTarget,
}

impl FakeHost {
    fn new(name: &str) -> FakeHost {
        let root = Scratch::new(&format!("ssh-{name}"));
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();
        let host_key = new_key(&root.join("host_key"));
        let ssh = root.join("ssh");
        std::fs::write(&ssh, FAKE_SSH).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = root.join("ssh.log");
        std::fs::write(&log, "").unwrap();
        std::fs::write(
            root.join("fake-ssh.env"),
            format!(
                "LOG={}\nHOME_DIR={}\nHOST_KEY={}\n",
                shell_quote(&log.to_string_lossy()),
                shell_quote(&home.to_string_lossy()),
                shell_quote(&host_key)
            ),
        )
        .unwrap();
        let target = SshTarget {
            host: "fake-host".to_string(),
            known_hosts_file: root.join("known_hosts").to_string_lossy().into_owned(),
            host_key_alias: "env-test".to_string(),
            ssh: Some(ssh.to_string_lossy().into_owned()),
            ..SshTarget::default()
        };
        FakeHost {
            host_key_pub: root.join("host_key.pub"),
            root,
            home,
            log,
            target,
        }
    }

    fn home(&self) -> String {
        self.home.to_string_lossy().into_owned()
    }

    fn options(&self) -> SshConnectOptions {
        SshConnectOptions {
            target: self.target.clone(),
            binary: Some(PathBuf::from(daemon())),
            ..SshConnectOptions::default()
        }
    }

    fn tools(&self) -> PathBuf {
        self.home.join(".cache/noel-env")
    }

    fn deployed(&self) -> PathBuf {
        let bytes = std::fs::read(daemon()).unwrap();
        let sha256: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.tools().join(format!("noel-env-{}", &sha256[..32]))
    }

    async fn trust(&self, target: &SshTarget) {
        let scanned = scan_host_key(target).await.unwrap();
        accept_host_key(target, &scanned.lines).await.unwrap();
    }

    /// Invocations in the log: an argument count, then the arguments, each followed by a NUL byte.
    fn invocations(&self) -> Vec<Vec<String>> {
        let text = std::fs::read_to_string(&self.log).unwrap();
        let mut fields: Vec<&str> = text.split('\0').collect();
        fields.pop();
        let mut result = Vec::new();
        let mut index = 0;
        while index < fields.len() {
            let count: usize = fields[index].parse().unwrap();
            result.push(
                fields[index + 1..index + 1 + count]
                    .iter()
                    .map(|field| field.to_string())
                    .collect(),
            );
            index += 1 + count;
        }
        result
    }
}

fn env(connection: &Connection, cwd: &str) -> RemoteEnv {
    RemoteEnv::new(RemoteEnvOptions::new(
        connection.clone(),
        "pi-env:test",
        cwd,
    ))
}

async fn output(env: &RemoteEnv, script: &str) -> String {
    let mut text = String::new();
    let mut on_output = |chunk: OutputChunk<'_>| {
        text.push_str(chunk.text);
        Ok(())
    };
    let result = env
        .exec(
            ShellCommand::Argv(vec!["sh".into(), "-c".into(), script.into()]),
            ExecOptions {
                on_output: Some(&mut on_output),
                ..ExecOptions::default()
            },
            &cx(),
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    text
}

/// What the fixture's placeholders stand for in noel's run.
fn normalized(argv: &[String], root: &str, sha256: &str) -> Vec<String> {
    argv.iter()
        .enumerate()
        .map(|(index, arg)| {
            if index > 0 && argv[index - 1] == "--token" {
                return "<token>".to_string();
            }
            let mut text = arg.replace(root, "<root>");
            if let Some(at) = text.find("/noel-env-hostkey-") {
                let start = text[..at].rfind('"').map_or(0, |quote| quote + 1);
                let end = at + 1 + text[at + 1..].find('/').unwrap();
                text.replace_range(start..=end, "<scan>/");
            }
            text.replace(sha256, "<sha256>")
                .replace(&sha256[..32], "<sha32>")
        })
        .collect()
}

fn renamed(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(
            RENAMES
                .iter()
                .fold(text.clone(), |text, (from, to)| text.replace(from, to)),
        ),
        Value::Array(items) => Value::Array(items.iter().map(renamed).collect()),
        other => other.clone(),
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect()
}

async fn start(connection: Connection) {
    connection.info().await.unwrap();
    connection.close();
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invokes_ssh_as_upstream_does() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let pin = MAKEFILE
        .lines()
        .find_map(|line| line.strip_prefix("CHORD_REFERENCE_REF ?= "))
        .unwrap();
    assert_eq!(fixture["source"]["ref"], pin.trim());

    let full = SshTarget {
        host: "gpu-box".to_string(),
        user: Some("me".to_string()),
        port: Some(2222),
        identity_file: Some("/keys/id %h".to_string()),
        known_hosts_file: "/data/known hosts".to_string(),
        host_key_alias: "env-gpu".to_string(),
        config_file: Some("/data/ssh config".to_string()),
        ssh: None,
    };
    let minimal = SshTarget {
        host: "gpu-box".to_string(),
        known_hosts_file: "/data/known_hosts".to_string(),
        host_key_alias: "env-gpu".to_string(),
        ..SshTarget::default()
    };
    let arguments = &fixture["arguments"];
    let ours = |target: &SshTarget, strict: bool, known: &str| {
        ssh_arguments(target, strict, known).unwrap()
    };
    assert_eq!(
        ours(&minimal, true, &minimal.known_hosts_file),
        strings(&arguments["minimal"])
    );
    assert_eq!(
        ours(&full, true, &full.known_hosts_file),
        strings(&arguments["full"])
    );
    assert_eq!(
        ours(&full, false, "/scan/known_hosts"),
        strings(&arguments["scan"])
    );

    let host = FakeHost::new("golden");
    let sha256: String = Sha256::digest(std::fs::read(daemon()).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut steps: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    let mut seen = 0;
    let mut step = |name: &str| {
        let all = host.invocations();
        let new = all[seen..]
            .iter()
            .map(|argv| normalized(argv, &host.root.text(), &sha256))
            .collect();
        seen = all.len();
        steps.push((name.to_string(), new));
    };

    host.trust(&host.target).await;
    step("scanHostKey");
    let (connection, _) = connect_ssh(host.options()).await.unwrap();
    step("connectSsh deploys");
    start(connection).await;
    step("the first start uses the deployment's check");
    let (connection, _) = connect_ssh(host.options()).await.unwrap();
    step("connectSsh finds the daemon");
    start(connection).await;
    step("start");
    let login = SshConnectOptions {
        login_shell: true,
        ..host.options()
    };
    let (connection, _) = connect_ssh(login).await.unwrap();
    step("connectSsh with loginShell");
    start(connection).await;
    step("start through the login shell");
    start(ssh_connection(host.options()).connection).await;
    step("sshConnection: the first request detects, checks and starts");

    let recorded = fixture["steps"].as_array().unwrap();
    assert_eq!(steps.len(), recorded.len());
    for ((name, invocations), recorded) in steps.iter().zip(recorded) {
        assert_eq!(name, recorded["step"].as_str().unwrap());
        let upstream: Vec<Vec<String>> = recorded["invocations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|argv| strings(&renamed(argv)))
            .collect();
        for argv in &upstream {
            for arg in argv {
                assert!(!arg.contains("pi-env"), "a name RENAMES misses in {arg:?}");
            }
        }
        assert_eq!(invocations.len(), upstream.len(), "{name}: invocations");
        for (index, (noel, upstream)) in invocations.iter().zip(&upstream).enumerate() {
            assert_eq!(noel, upstream, "{name}: invocation {index}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_an_untrusted_host_then_deploys_and_runs_the_daemon_once_its_key_is_accepted() {
    let host = FakeHost::new("deploy");
    let refused = connect_ssh(host.options()).await.unwrap_err();
    assert!(
        matches!(refused, SshError::HostKeyUnknown(_)),
        "{refused:?}"
    );

    let scanned = scan_host_key(&host.target).await.unwrap();
    let expected = Command::new("ssh-keygen")
        .arg("-lf")
        .arg(&host.host_key_pub)
        .output()
        .unwrap();
    let fingerprint = String::from_utf8_lossy(&expected.stdout)
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string();
    assert!(scanned.fingerprints.join("\n").contains(&fingerprint));
    accept_host_key(&host.target, &scanned.lines).await.unwrap();

    std::fs::create_dir_all(host.tools()).unwrap();
    std::fs::write(
        host.tools()
            .join("noel-env-0123456789abcdef0123456789abcdef"),
        "old",
    )
    .unwrap();
    let (connection, remote) = connect_ssh(host.options()).await.unwrap();
    assert_eq!(remote.home, host.home());
    assert_eq!(
        std::fs::read(host.deployed()).unwrap(),
        std::fs::read(daemon()).unwrap()
    );
    // Daemons of other contents are removed.
    let names: Vec<String> = std::fs::read_dir(host.tools())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("noel-env-"))
        .collect();
    assert_eq!(
        names,
        [host.deployed().file_name().unwrap().to_string_lossy()]
    );
    let env = env(&connection, &host.home());
    env.write_file("over-ssh.txt", b"hello", &cx())
        .await
        .unwrap();
    assert_eq!(
        env.read_text_file("over-ssh.txt", &cx()).await.unwrap(),
        "hello"
    );
    assert_eq!(output(&env, r#"printf "%s" "$HOME""#).await, host.home());
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn reuses_a_verified_daemon_and_replaces_a_tampered_one() {
    let host = FakeHost::new("reuse");
    host.trust(&host.target).await;
    connect_ssh(host.options()).await.unwrap().0.close();
    let file = host.deployed();
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    connect_ssh(host.options()).await.unwrap().0.close();
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );

    std::fs::write(&file, "tampered").unwrap();
    let (connection, _) = connect_ssh(host.options()).await.unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(daemon()).unwrap()
    );
    assert_eq!(connection.info().await.unwrap().home, host.home());
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn verifies_the_daemon_again_before_starting_it_after_a_lost_connection() {
    let host = FakeHost::new("verify");
    host.trust(&host.target).await;
    let (connection, _) = connect_ssh(host.options()).await.unwrap();
    let file = host.deployed();
    let pid = connection.info().await.unwrap().pid;
    // SAFETY: kill(2) on the daemon this test started.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Linux refuses to write a running program's file.
    std::fs::write(&file, "tampered").unwrap();
    // The next start redeploys the verified binary instead of running whatever is there.
    assert_ne!(connection.info().await.unwrap().pid, pid);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        std::fs::read(daemon()).unwrap()
    );
    connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_files_over_ssh_as_a_local_daemon_lists_them() {
    let host = FakeHost::new("list");
    let tree = host.home.join("tree");
    for file in [
        "a/b.txt",
        "a/c/d.txt",
        ".hidden",
        "out/x.js",
        "skip/y.txt",
        ".git/HEAD",
    ] {
        let path = tree.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    std::fs::create_dir(tree.join("empty")).unwrap();
    std::fs::write(tree.join(".gitignore"), "out/\n").unwrap();
    host.trust(&host.target).await;
    let (connection, _) = connect_ssh(host.options()).await.unwrap();
    let local = support::connection();
    let options = ListFilesOptions {
        exclude: vec!["skip".to_string()],
        ..ListFilesOptions::default()
    };
    let listing = env(&connection, &host.home())
        .list_files("tree", &options, &cx())
        .await
        .unwrap();
    let entries: Vec<(String, FileKind)> = listing
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.kind))
        .collect();
    let expected = [
        (".gitignore", FileKind::File),
        (".hidden", FileKind::File),
        ("a", FileKind::Directory),
        ("a/b.txt", FileKind::File),
        ("a/c", FileKind::Directory),
        ("a/c/d.txt", FileKind::File),
        ("empty", FileKind::Directory),
    ]
    .map(|(path, kind)| (path.to_string(), kind));
    assert_eq!((entries, listing.truncated), (expected.to_vec(), false));
    let direct = env(&local, &host.home())
        .list_files("tree", &options, &cx())
        .await
        .unwrap();
    assert_eq!(direct, listing);
    connection.close();
    local.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn starts_the_daemon_through_the_login_shell_only_when_asked() {
    let host = FakeHost::new("login");
    // The login shell is the account's shell: sh and bash read .profile, zsh (the macOS default) reads .zprofile.
    std::fs::write(host.home.join(".profile"), "export PI_ENV_LOGIN=yes\n").unwrap();
    std::fs::write(host.home.join(".zprofile"), "export PI_ENV_LOGIN=yes\n").unwrap();
    host.trust(&host.target).await;
    let mut seen = Vec::new();
    for login_shell in [false, true] {
        let options = SshConnectOptions {
            login_shell,
            ..host.options()
        };
        let (connection, _) = connect_ssh(options).await.unwrap();
        seen.push(
            output(
                &env(&connection, &host.home()),
                r#"printf "%s" "$PI_ENV_LOGIN""#,
            )
            .await,
        );
        connection.close();
    }
    assert_eq!(seen, ["", "yes"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_lazily_and_reports_a_failed_start_as_the_error_of_the_operation() {
    let host = FakeHost::new("lazy");
    let lazy = SshTarget {
        known_hosts_file: host
            .root
            .join("lazy_known_hosts")
            .to_string_lossy()
            .into_owned(),
        ..host.target.clone()
    };
    let connection = ssh_connection(SshConnectOptions {
        target: lazy.clone(),
        ..host.options()
    });
    assert_eq!(connection.remote(), None);
    assert!(
        host.invocations().is_empty(),
        "nothing happens before the first request"
    );
    let env = env(&connection.connection, &host.home());
    // Nothing is trusted yet: each operation fails with the reason and tries again.
    let read = env.read_text_file("missing.txt", &cx()).await.unwrap_err();
    assert!(read.message.contains("not trusted"), "{}", read.message);
    let command = env
        .exec("exit 0".into(), ExecOptions::default(), &cx())
        .await
        .unwrap_err();
    assert_eq!(command.code, ExecutionErrorCode::SpawnError);
    host.trust(&lazy).await;
    let accepted = env
        .exec("exit 0".into(), ExecOptions::default(), &cx())
        .await
        .unwrap();
    assert_eq!(accepted.exit_code, 0);
    assert_eq!(connection.remote().unwrap().home, host.home());
    connection.connection.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn accepts_only_host_keys_for_the_alias_and_never_replaces_a_trusted_key_silently() {
    let host = FakeHost::new("accept");
    let scoped = SshTarget {
        known_hosts_file: host
            .root
            .join("accept_known_hosts")
            .to_string_lossy()
            .into_owned(),
        ..host.target.clone()
    };
    let first = new_key(&host.root.join("accept_first"));
    let second = new_key(&host.root.join("accept_second"));
    let line = |text: String| vec![text];
    assert!(
        accept_host_key(&scoped, &line(format!("other-alias {first}")))
            .await
            .is_err()
    );
    assert!(
        accept_host_key(&scoped, &line(format!("@cert-authority env-test {first}")))
            .await
            .is_err()
    );
    let trusted = line(format!("env-test {first}"));
    let (one, two) = tokio::join!(
        accept_host_key(&scoped, &trusted),
        accept_host_key(&scoped, &trusted)
    );
    one.unwrap();
    two.unwrap();
    assert_eq!(
        std::fs::read_to_string(&scoped.known_hosts_file).unwrap(),
        format!("env-test {first}\n")
    );
    let changed = accept_host_key(&scoped, &line(format!("env-test {second}")))
        .await
        .unwrap_err();
    assert!(
        matches!(changed, SshError::HostKeyChanged(_)),
        "{changed:?}"
    );
    forget_host_key(&scoped).await.unwrap();
    accept_host_key(&scoped, &line(format!("env-test {second}")))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&scoped.known_hosts_file).unwrap(),
        format!("env-test {second}\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refuses_a_changed_host_key() {
    let host = FakeHost::new("changed");
    let other = new_key(&host.root.join("other_key"));
    let known_hosts = host.root.join("changed_known_hosts");
    std::fs::write(&known_hosts, format!("env-test {other}\n")).unwrap();
    let options = SshConnectOptions {
        target: SshTarget {
            known_hosts_file: known_hosts.to_string_lossy().into_owned(),
            ..host.target.clone()
        },
        ..host.options()
    };
    let refused = connect_ssh(options).await.unwrap_err();
    assert!(
        matches!(refused, SshError::HostKeyChanged(_)),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn finds_the_daemon_for_the_remote_system_in_its_directory_and_names_a_missing_one() {
    let host = FakeHost::new("dir");
    host.trust(&host.target).await;
    let arch = match std::env::consts::ARCH {
        "aarch64" => RemoteArch::Arm64,
        _ => RemoteArch::X64,
    };
    let this_machine = RemotePlatform {
        platform: if cfg!(target_os = "macos") {
            RemoteSystem::Darwin
        } else {
            RemoteSystem::Linux
        },
        arch,
        home: host.home(),
    };
    let daemons = host.root.join("daemons");
    std::fs::create_dir(&daemons).unwrap();
    let options = SshConnectOptions {
        binary: None,
        daemon_dir: Some(daemons.clone()),
        ..host.options()
    };
    let missing = connect_ssh(options.clone()).await.unwrap_err().to_string();
    let expected = daemons.join(daemon_name(&this_machine));
    assert!(
        missing.contains(&expected.to_string_lossy().into_owned())
            && missing.contains("NOEL_ENV_DAEMON_DIR"),
        "{missing}"
    );
    std::fs::copy(daemon(), &expected).unwrap();
    let (connection, remote) = connect_ssh(options).await.unwrap();
    assert_eq!(remote, this_machine);
    assert_eq!(connection.info().await.unwrap().home, host.home());
    connection.close();
}
