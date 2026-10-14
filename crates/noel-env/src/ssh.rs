//! SSH deployment (upstream `packages/env/src/ssh.ts`) for Linux and macOS remotes: the system `ssh` with
//! `BatchMode`, strict host-key checking against the application's own known-hosts file under a fixed alias, and no
//! forwarding of any kind; host keys scanned and accepted once; and the daemon uploaded, named by its SHA-256 and
//! verified before every start.
//!
//! The daemon to upload is `noel-env-<platform>-<arch>` (`noel-env-linux-x64`, `noel-env-darwin-arm64`, …) in a
//! directory given by [`SshConnectOptions::daemon_dir`], else `NOEL_ENV_DAEMON_DIR`, else the running executable's
//! directory. On the remote machine it lives at `~/.cache/noel-env/noel-env-<first 32 hex digits of its SHA-256>`.
//!
//! Ported from upstream pi's `packages/env/src/ssh.ts` (<https://github.com/earendil-works/pi>, commit `1692bba`),
//! Copyright (c) 2025 Mario Zechner, MIT License (see LICENSE at the repository root). Upstream's Windows and
//! Android (Termux) paths are not ported.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::connection::{Connection, ConnectionOptions, DaemonCommand, LogCallback};

/// How long a detection, check or upload over `ssh` may take before it is abandoned.
const SSH_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// The environment variable naming the directory of daemon binaries.
pub const DAEMON_DIR_VARIABLE: &str = "NOEL_ENV_DAEMON_DIR";
/// Where deployed daemons live, below the remote home directory.
pub const REMOTE_DAEMON_DIR: &str = ".cache/noel-env";

/// A failure to reach, detect or deploy to a remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshError {
    /// The host's key is not in the known-hosts file; [`scan_host_key`] shows it so the owner can accept it.
    HostKeyUnknown(String),
    /// The host's key differs from the one in the known-hosts file; it is never accepted automatically.
    HostKeyChanged(String),
    /// An `ssh` invocation that failed for another reason, with its exit code and diagnostics.
    Failed {
        message: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    /// Anything else: an invalid target, an unsupported system, a missing daemon binary.
    Other(String),
}

impl SshError {
    pub fn message(&self) -> &str {
        match self {
            Self::HostKeyUnknown(message)
            | Self::HostKeyChanged(message)
            | Self::Other(message)
            | Self::Failed { message, .. } => message,
        }
    }
}

impl fmt::Display for SshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for SshError {}

fn other(message: impl Into<String>) -> SshError {
    SshError::Other(message.into())
}

/// How to reach a machine with the system `ssh`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshTarget {
    /// Host name or `~/.ssh/config` alias.
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// Host keys this application trusts; [`accept_host_key`] adds to it.
    pub known_hosts_file: String,
    /// The name keys are stored under, independent of aliases, ports and jump hosts, e.g. `noel-env-<env name>`.
    pub host_key_alias: String,
    /// The `ssh` program; default `ssh`.
    pub ssh: Option<String>,
    /// An `ssh` configuration file instead of `~/.ssh/config` (`-F`).
    pub config_file: Option<String>,
}

/// JavaScript's `\s`.
fn js_space(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// No leading `-` (it would be read as an `ssh` option) and no whitespace or control characters.
fn check_field(name: &str, value: &str) -> Result<(), SshError> {
    if value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| js_space(character) || character <= '\u{1f}' || character == '\u{7f}')
    {
        return Err(other(format!("Invalid {name}: {}", quoted(value))));
    }
    Ok(())
}

/// A path for an `ssh` option that expands `%` tokens: quoted, with `%` literal.
fn config_path(name: &str, path: &str) -> Result<String, SshError> {
    if path.is_empty()
        || path
            .chars()
            .any(|character| character == '"' || character <= '\u{1f}' || character == '\u{7f}')
    {
        return Err(other(format!("Invalid {name}: {}", quoted(path))));
    }
    Ok(format!("\"{}\"", path.replace('%', "%%")))
}

/// Arguments for `ssh` up to the host: no prompts, no forwarding of any kind, no shared connections, no commands
/// from the configuration, no locale forwarding (the remote uses its own), and host keys checked strictly against
/// the application's own file under a fixed alias. Without `strict`, unknown keys are accepted into
/// `known_hosts_file` (for [`scan_host_key`]).
pub fn ssh_arguments(
    target: &SshTarget,
    strict: bool,
    known_hosts_file: &str,
) -> Result<Vec<String>, SshError> {
    check_field("host", &target.host)?;
    check_field("host key alias", &target.host_key_alias)?;
    if let Some(user) = &target.user {
        check_field("user", user)?;
    }
    if target.port == Some(0) {
        return Err(other("Invalid port: 0"));
    }
    let mut arguments = Vec::new();
    if let Some(config_file) = &target.config_file {
        arguments.extend(["-F".to_string(), config_file.clone()]);
    }
    arguments.extend(["-T", "-a", "-x"].map(str::to_string));
    let mut option = |setting: String| arguments.extend(["-o".to_string(), setting]);
    for setting in [
        "BatchMode=yes",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
        "ForwardX11=no",
        "ControlMaster=no",
        "ControlPath=none",
        "RemoteCommand=none",
        "PermitLocalCommand=no",
        "SendEnv=-*",
        "ServerAliveInterval=15",
    ] {
        option(setting.to_string());
    }
    option(format!(
        "StrictHostKeyChecking={}",
        if strict { "yes" } else { "accept-new" }
    ));
    option(format!(
        "UserKnownHostsFile={}",
        config_path("known hosts file", known_hosts_file)?
    ));
    option("GlobalKnownHostsFile=none".to_string());
    option("HashKnownHosts=no".to_string());
    option(format!("HostKeyAlias={}", target.host_key_alias));
    if let Some(user) = &target.user {
        arguments.extend(["-l".to_string(), user.clone()]);
    }
    if let Some(port) = target.port {
        arguments.extend(["-p".to_string(), port.to_string()]);
    }
    if let Some(identity_file) = &target.identity_file {
        let identity_file = config_path("identity file", identity_file)?;
        arguments.extend([
            "-o".to_string(),
            format!("IdentityFile={identity_file}"),
            "-o".to_string(),
            "IdentitiesOnly=yes".to_string(),
        ]);
    }
    arguments.extend(["--".to_string(), target.host.clone()]);
    Ok(arguments)
}

async fn collect(mut pipe: impl AsyncRead + Unpin, into: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = vec![0u8; 16 * 1024];
    while let Ok(read) = pipe.read(&mut chunk).await {
        if read == 0 {
            return;
        }
        into.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(&chunk[..read]);
    }
}

fn text(bytes: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&bytes.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
}

/// Whether `ssh` said it does not know the host's key (`/Host key verification failed|No .* host key is known/`).
fn unknown_host_key(diagnostics: &str) -> bool {
    diagnostics.contains("Host key verification failed")
        || diagnostics.lines().any(|line| {
            line.match_indices("No ")
                .any(|(at, _)| line[at + 3..].contains(" host key is known"))
        })
}

/// Runs one remote command over `ssh`, feeding `stdin`; returns stdout.
async fn run_ssh(
    target: &SshTarget,
    command: &str,
    stdin: &[u8],
    arguments: Option<Vec<String>>,
    timeout: Duration,
) -> Result<String, SshError> {
    let arguments = match arguments {
        Some(arguments) => arguments,
        None => ssh_arguments(target, true, &target.known_hosts_file)?,
    };
    let mut child = Command::new(target.ssh.as_deref().unwrap_or("ssh"))
        .args(&arguments)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| SshError::Failed {
            message: error.to_string(),
            exit_code: None,
            stderr: String::new(),
        })?;
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let readers = (
        child
            .stdout
            .take()
            .map(|pipe| tokio::spawn(collect(pipe, stdout.clone()))),
        child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(collect(pipe, stderr.clone()))),
    );
    if let Some(mut pipe) = child.stdin.take() {
        let input = stdin.to_vec();
        tokio::spawn(async move {
            // A command that does not read its input closes the pipe early.
            let _ = pipe.write_all(&input).await;
            let _ = pipe.shutdown().await;
        });
    }
    let finished = async {
        let status = child.wait().await;
        for reader in [readers.0, readers.1].into_iter().flatten() {
            let _ = reader.await;
        }
        status
    };
    let status = match tokio::time::timeout(timeout, finished).await {
        Ok(status) => status,
        Err(_) => {
            let _ = child.start_kill();
            let diagnostics = text(&stderr);
            return Err(SshError::Failed {
                message: format!(
                    "ssh {} did not finish within {} s: {}",
                    target.host,
                    timeout.as_secs(),
                    diagnostics.trim()
                ),
                exit_code: None,
                stderr: diagnostics,
            });
        }
    };
    let code = status.ok().and_then(|status| status.code());
    let diagnostics = text(&stderr);
    if code == Some(0) {
        return Ok(text(&stdout));
    }
    if diagnostics.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Err(SshError::HostKeyChanged(format!(
            "The host key of {} changed; remove the old key to continue",
            target.host
        )));
    }
    if unknown_host_key(&diagnostics) {
        return Err(SshError::HostKeyUnknown(format!(
            "The host key of {} is not trusted yet",
            target.host
        )));
    }
    Err(SshError::Failed {
        message: format!(
            "ssh {} failed with exit code {}: {}",
            target.host,
            code.map_or("null".to_string(), |code| code.to_string()),
            diagnostics.trim()
        ),
        exit_code: code,
        stderr: diagnostics,
    })
}

/// The host's key as [`scan_host_key`] captured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedHostKey {
    /// Known-hosts lines, `alias type key`, for [`accept_host_key`].
    pub lines: Vec<String>,
    /// `ssh-keygen -lf` fingerprints of those keys, to compare with ones obtained out of band.
    pub fingerprints: Vec<String>,
}

/// Connects once with `accept-new` against a temporary known-hosts file to capture the key the host presents,
/// through the same route (`~/.ssh/config`, jump hosts) as real connections. Showing a fingerprint is not
/// authentication: compare it with one obtained out of band before accepting it.
pub async fn scan_host_key(target: &SshTarget) -> Result<ScannedHostKey, SshError> {
    let directory = crate::local::create_temp_dir_sync("noel-env-hostkey-")
        .map_err(|error| other(error.message))?;
    let scanned = format!("{directory}/known_hosts");
    let result = async {
        // Authentication may fail without the key; the key is recorded before authentication.
        let arguments = ssh_arguments(target, false, &scanned)?;
        match run_ssh(target, "exit 0", &[], Some(arguments), SSH_TIMEOUT).await {
            Ok(_) | Err(SshError::Failed { .. }) | Err(SshError::HostKeyUnknown(_)) => {}
            Err(error) => return Err(error),
        }
        let lines: Vec<String> = fs::read_to_string(&scanned)
            .unwrap_or_default()
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        if lines.is_empty() {
            return Err(other(format!("No host key received from {}", target.host)));
        }
        let output = Command::new("ssh-keygen")
            .args(["-lf", &scanned])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| other(error.to_string()))?;
        let fingerprints = String::from_utf8_lossy(&output.stdout)
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        Ok(ScannedHostKey {
            lines,
            fingerprints,
        })
    }
    .await;
    let _ = fs::remove_dir_all(&directory);
    result
}

/// A known-hosts line for `alias`: `alias type key [comment]`, no markers, patterns or hashed names.
fn parse_host_key_line<'a>(alias: &str, line: &'a str) -> Result<(&'a str, &'a str), SshError> {
    let mut fields = line.split_whitespace();
    let (host, kind, key) = (fields.next(), fields.next(), fields.next());
    let kind_ok = |kind: &str| {
        let tail = |prefix: &str, extra: &str| {
            kind.strip_prefix(prefix).is_some_and(|rest| {
                !rest.is_empty()
                    && rest.chars().all(|character| {
                        character.is_ascii_lowercase()
                            || character.is_ascii_digit()
                            || extra.contains(character)
                    })
            })
        };
        tail("ssh-", "-") || tail("ecdsa-sha2-", "-") || tail("sk-", "@.-")
    };
    let key_ok = |key: &str| {
        let body = key.trim_end_matches('=');
        !body.is_empty()
            && key.len() - body.len() <= 2
            && body.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '+' || character == '/'
            })
    };
    match (host, kind, key) {
        (Some(host), Some(kind), Some(key)) if host == alias && kind_ok(kind) && key_ok(key) => {
            Ok((kind, key))
        }
        _ => Err(other(format!(
            "Not a host key line for {alias}: {}",
            quoted(line)
        ))),
    }
}

/// Changes to known-hosts files, one at a time.
static KNOWN_HOSTS_CHANGES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Rewrites a known-hosts file atomically, one change at a time.
async fn change_known_hosts(
    file: &str,
    change: impl FnOnce(Vec<String>) -> Result<Vec<String>, SshError>,
) -> Result<(), SshError> {
    let _turn = KNOWN_HOSTS_CHANGES.lock().await;
    let io = |error: std::io::Error| other(format!("{file}: {error}"));
    if let Some(parent) = Path::new(file)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io)?;
    }
    let existing = match fs::read_to_string(file) {
        Ok(text) => text
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io(error)),
    };
    let lines = change(existing)?;
    let suffix: String = crate::local::random_uuid()
        .replace('-', "")
        .chars()
        .take(12)
        .collect();
    let temporary = format!("{file}.{suffix}.tmp");
    let contents = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut out| out.write_all(contents.as_bytes()))
        .map_err(io)?;
    if let Err(error) = fs::rename(&temporary, file) {
        let _ = fs::remove_file(&temporary);
        return Err(io(error));
    }
    Ok(())
}

/// Trusts host-key lines from [`scan_host_key`] by adding them to the target's known-hosts file. Only plain lines
/// for the target's alias are accepted. A key that differs from a trusted key of the same type is refused with
/// [`SshError::HostKeyChanged`]; [`forget_host_key`] must remove the old keys first.
pub async fn accept_host_key(target: &SshTarget, lines: &[String]) -> Result<(), SshError> {
    let alias = target.host_key_alias.as_str();
    let mut accepted = Vec::new();
    for line in lines {
        let (kind, key) = parse_host_key_line(alias, line)?;
        accepted.push((line.trim().to_string(), kind.to_string(), key.to_string()));
    }
    change_known_hosts(&target.known_hosts_file, |mut existing| {
        let mut trusted: Vec<(String, String)> = Vec::new();
        for line in &existing {
            let mut fields = line.split_whitespace();
            if let (Some(host), Some(kind), Some(key)) =
                (fields.next(), fields.next(), fields.next())
            {
                if host == alias {
                    trusted.retain(|(known, _)| known != kind);
                    trusted.push((kind.to_string(), key.to_string()));
                }
            }
        }
        for (line, kind, key) in accepted {
            match trusted.iter().find(|(known, _)| *known == kind) {
                Some((_, known)) if *known == key => continue,
                Some(_) => {
                    return Err(SshError::HostKeyChanged(format!(
                        "The {kind} host key of {} changed; forget the old key first",
                        target.host
                    )))
                }
                None => {
                    trusted.push((kind, key));
                    existing.push(line);
                }
            }
        }
        Ok(existing)
    })
    .await
}

/// Stops trusting every key stored for the target's alias, e.g. after the owner confirmed a changed host key.
pub async fn forget_host_key(target: &SshTarget) -> Result<(), SshError> {
    change_known_hosts(&target.known_hosts_file, |existing| {
        Ok(existing
            .into_iter()
            .filter(|line| line.split_whitespace().next().unwrap_or("") != target.host_key_alias)
            .collect())
    })
    .await
}

/// A remote system noel ships a daemon for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteSystem {
    Linux,
    Darwin,
}

impl RemoteSystem {
    /// Node's name, as upstream names its daemons.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Darwin => "darwin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteArch {
    X64,
    Arm64,
}

impl RemoteArch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

/// What [`detect_platform`] found (upstream `RemotePlatform`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlatform {
    pub platform: RemoteSystem,
    pub arch: RemoteArch,
    /// The remote home directory.
    pub home: String,
}

/// POSIX detection, run by the remote login shell.
pub const POSIX_PROBE: &str = r#"sh -c 'echo PI-ENV-PROBE; uname -s; uname -m; uname -o 2>/dev/null || echo -; printf "%s\n" "$HOME" "${TMPDIR:--}" "${LD_PRELOAD:--}"'"#;

fn normalize_arch(machine: &str) -> Result<RemoteArch, SshError> {
    match machine.to_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok(RemoteArch::X64),
        "aarch64" | "arm64" => Ok(RemoteArch::Arm64),
        _ => Err(other(format!("Unsupported remote architecture: {machine}"))),
    }
}

struct Probe {
    system: String,
    machine: String,
    os: String,
    home: String,
}

fn parse_probe(target: &SshTarget, output: &str) -> Result<Probe, SshError> {
    // Login shells may print a banner first.
    let lines: Vec<&str> = output
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let Some(start) = lines.iter().position(|line| *line == "PI-ENV-PROBE") else {
        return Err(other(format!(
            "Unexpected answer from {}: {}",
            target.host,
            output.trim()
        )));
    };
    let field = |index: usize| {
        lines
            .get(start + 1 + index)
            .copied()
            .unwrap_or("")
            .to_string()
    };
    Ok(Probe {
        system: field(0),
        machine: field(1),
        os: field(2),
        home: field(3),
    })
}

/// Which system the target runs: `uname` through the login shell.
pub async fn detect_platform(target: &SshTarget) -> Result<RemotePlatform, SshError> {
    let probe = parse_probe(
        target,
        &run_ssh(target, POSIX_PROBE, &[], None, SSH_TIMEOUT).await?,
    )?;
    let system = probe.system.as_str();
    if ["MINGW", "MSYS", "CYGWIN"]
        .iter()
        .any(|prefix| system.starts_with(prefix))
    {
        return Err(other(format!(
            "{} runs Windows, which noel-env does not support",
            target.host
        )));
    }
    let arch = normalize_arch(&probe.machine)?;
    let platform = match system {
        "Darwin" => RemoteSystem::Darwin,
        "Linux" if probe.os == "Android" => {
            return Err(other(format!(
                "{} runs Android (Termux), which noel-env does not support",
                target.host
            )))
        }
        "Linux" => RemoteSystem::Linux,
        _ => return Err(other(format!("Unsupported remote system: {system}"))),
    };
    Ok(RemotePlatform {
        platform,
        arch,
        home: probe.home,
    })
}

/// The file name of the daemon for a remote system: `noel-env-<platform>-<arch>`, in Node's names.
pub fn daemon_name(remote: &RemotePlatform) -> String {
    format!(
        "noel-env-{}-{}",
        remote.platform.as_str(),
        remote.arch.as_str()
    )
}

/// The directory of daemon binaries: `dir`, else `NOEL_ENV_DAEMON_DIR`, else the running executable's directory.
pub fn daemon_dir(dir: Option<&Path>) -> Result<PathBuf, SshError> {
    if let Some(dir) = dir {
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = std::env::var_os(DAEMON_DIR_VARIABLE).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let executable = std::env::current_exe()
        .map_err(|error| other(format!("Cannot find the noel-env daemons: {error}")))?;
    Ok(executable
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default())
}

/// The daemon binary for a remote system in `dir` (see [`daemon_dir`]).
pub fn packaged_daemon(remote: &RemotePlatform, dir: Option<&Path>) -> Result<PathBuf, SshError> {
    Ok(daemon_dir(dir)?.join(daemon_name(remote)))
}

pub fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Where a daemon with this content lives on the remote machine: named by its SHA-256, so versions never collide.
fn daemon_path(remote: &RemotePlatform, sha256: &str) -> String {
    format!(
        "{}/{REMOTE_DAEMON_DIR}/noel-env-{}",
        remote.home,
        &sha256[..32]
    )
}

const POSIX_HASH: &str = r#"hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/.*= //'; else echo none; fi; }"#;

/// The remote command that tells whether `file` has this content: `present`, `missing`, or (without a hash tool)
/// `nohash`.
pub fn check_command(file: &str, sha256: &str) -> String {
    let check = format!(
        r#"{POSIX_HASH}; f={}; if [ -f "$f" ] && [ "$(hash "$f")" = {sha256} ]; then echo present; elif [ "$(hash /dev/null)" = none ]; then echo nohash; else echo missing; fi"#,
        quote_posix(file)
    );
    format!("sh -c {}", quote_posix(&check))
}

/// The remote command that uploads the daemon from stdin to a new temporary file, renames it into place only once
/// its SHA-256 matches, and removes daemons of other contents.
pub fn upload_command(file: &str, sha256: &str) -> String {
    let upload = [
        "set -e".to_string(),
        POSIX_HASH.to_string(),
        format!("f={}", quote_posix(file)),
        r#"d=$(dirname "$f")"#.to_string(),
        r#"mkdir -p "$d""#.to_string(),
        r#"chmod 700 "$d""#.to_string(),
        r#"t=$(mktemp "$d/.noel-env.XXXXXX")"#.to_string(),
        r#"cat > "$t""#.to_string(),
        format!(
            r#"if [ "$(hash "$t")" != {sha256} ]; then rm -f "$t"; echo "noel-env upload is corrupt" >&2; exit 1; fi"#
        ),
        r#"chmod 700 "$t""#.to_string(),
        r#"mv -f "$t" "$f""#.to_string(),
        // Older daemons; running ones keep their file open until they exit.
        r#"for old in "$d"/noel-env-*; do [ "$old" = "$f" ] || rm -f "$old"; done"#.to_string(),
        "echo deployed".to_string(),
    ]
    .join("\n");
    format!("sh -c {}", quote_posix(&upload))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Makes sure the daemon `binary` is on the remote machine, verified by its SHA-256 before it ever runs, and removes
/// daemons of other contents. The upload goes to a new temporary file and is renamed into place only once its hash
/// matches. Returns the remote path of the daemon.
pub async fn deploy_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    binary: &Path,
) -> Result<String, SshError> {
    let bytes = fs::read(binary).map_err(|error| {
        other(format!(
            "No noel-env daemon for {}-{} at {}: {error}; build {} for that system (make env-daemons builds the Linux ones) and put it there, or set {DAEMON_DIR_VARIABLE}",
            remote.platform.as_str(),
            remote.arch.as_str(),
            binary.display(),
            daemon_name(remote),
        ))
    })?;
    let sha256 = hex(&Sha256::digest(&bytes));
    let file = daemon_path(remote, &sha256);
    let state = run_ssh(
        target,
        &check_command(&file, &sha256),
        &[],
        None,
        SSH_TIMEOUT,
    )
    .await?;
    match state.trim().split('\n').next_back().unwrap_or("missing") {
        "present" => return Ok(file),
        "nohash" => {
            return Err(other(format!(
                "{} has no sha256sum, shasum or openssl to verify noel-env",
                target.host
            )))
        }
        _ => {}
    }
    run_ssh(
        target,
        &upload_command(&file, &sha256),
        &bytes,
        None,
        UPLOAD_TIMEOUT,
    )
    .await?;
    Ok(file)
}

/// The remote command that starts the daemon at `file`, before the `serve` arguments.
pub fn launch_command(file: &str, login_shell: bool) -> String {
    // `$0` is the daemon, `"$@"` its arguments; `$SHELL` is the login shell's own name for itself.
    if login_shell {
        format!(
            r#"exec "$SHELL" -lc 'exec "$0" "$@"' {}"#,
            quote_posix(file)
        )
    } else {
        quote_posix(file)
    }
}

/// Options of [`ssh_connection`] and [`connect_ssh`]: the target, and the daemon to deploy.
#[derive(Clone, Default)]
pub struct SshConnectOptions {
    pub target: SshTarget,
    /// The daemon binary to deploy; by default `noel-env-<platform>-<arch>` in [`daemon_dir`].
    pub binary: Option<PathBuf>,
    /// The directory of daemon binaries (see [`daemon_dir`]).
    pub daemon_dir: Option<PathBuf>,
    /// Start the daemon through the user's login shell (`$SHELL -l`), so commands see the environment of
    /// `~/.profile` and similar files. Off by default: `ssh` runs commands without a login shell.
    pub login_shell: bool,
    pub on_log: Option<LogCallback>,
}

impl fmt::Debug for SshConnectOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshConnectOptions")
            .field("target", &self.target)
            .field("binary", &self.binary)
            .field("daemon_dir", &self.daemon_dir)
            .field("login_shell", &self.login_shell)
            .finish_non_exhaustive()
    }
}

impl SshConnectOptions {
    fn binary_for(&self, remote: &RemotePlatform) -> Result<PathBuf, SshError> {
        match &self.binary {
            Some(binary) => Ok(binary.clone()),
            None => packaged_daemon(remote, self.daemon_dir.as_deref()),
        }
    }
}

/// What an SSH connection learned about its remote system, and the daemon it verified last.
#[derive(Default)]
struct SshState {
    remote: Option<RemotePlatform>,
    verified: Option<String>,
}

/// A [`Connection`] whose every start detects the remote system (once), verifies or deploys the daemon, then starts
/// it over `ssh`.
fn ssh_connection_for(options: SshConnectOptions, state: Arc<Mutex<SshState>>) -> Connection {
    let on_log = options.on_log.clone();
    let options = Arc::new(options);
    let command = DaemonCommand::Computed(Arc::new(move || {
        let options = options.clone();
        let state = state.clone();
        Box::pin(async move {
            let known = state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remote
                .clone();
            let remote = match known {
                Some(remote) => remote,
                None => {
                    let remote = detect_platform(&options.target)
                        .await
                        .map_err(|error| error.to_string())?;
                    state.lock().unwrap_or_else(PoisonError::into_inner).remote =
                        Some(remote.clone());
                    remote
                }
            };
            // A deployment that just verified the binary counts for the first start.
            let verified = state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .verified
                .take();
            let file = match verified {
                Some(file) => file,
                None => {
                    let binary = options
                        .binary_for(&remote)
                        .map_err(|error| error.to_string())?;
                    deploy_daemon(&options.target, &remote, &binary)
                        .await
                        .map_err(|error| error.to_string())?
                }
            };
            let target = &options.target;
            let mut argv = vec![target.ssh.clone().unwrap_or_else(|| "ssh".to_string())];
            argv.extend(
                ssh_arguments(target, true, &target.known_hosts_file)
                    .map_err(|error| error.to_string())?,
            );
            argv.push(launch_command(&file, options.login_shell));
            Ok(argv)
        })
    }));
    Connection::new(ConnectionOptions { command, on_log })
}

/// A lazy SSH connection and what it learned about the remote system.
#[derive(Clone)]
pub struct SshConnection {
    pub connection: Connection,
    state: Arc<Mutex<SshState>>,
}

impl SshConnection {
    /// The detected system, once known.
    pub fn remote(&self) -> Option<RemotePlatform> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remote
            .clone()
    }
}

/// A [`Connection`] to the target that does nothing until its first request. Each start detects the remote system
/// (the first time), verifies the daemon and deploys it if it is missing or changed, then starts it over `ssh`. A
/// failure (no network, an untrusted or changed host key) fails the requests waiting for that start with code
/// `spawn_error` and the reason as message; the next request tries again.
pub fn ssh_connection(options: SshConnectOptions) -> SshConnection {
    let state = Arc::new(Mutex::new(SshState::default()));
    SshConnection {
        connection: ssh_connection_for(options, state.clone()),
        state,
    }
}

/// Detects the remote system and deploys the daemon now, then returns a [`Connection`] that starts it over `ssh`,
/// like [`ssh_connection`]. Host keys must already be trusted ([`scan_host_key`], [`accept_host_key`]), otherwise
/// this fails with [`SshError::HostKeyUnknown`].
pub async fn connect_ssh(
    options: SshConnectOptions,
) -> Result<(Connection, RemotePlatform), SshError> {
    let remote = detect_platform(&options.target).await?;
    let verified = deploy_daemon(&options.target, &remote, &options.binary_for(&remote)?).await?;
    let state = Arc::new(Mutex::new(SshState {
        remote: Some(remote.clone()),
        verified: Some(verified),
    }));
    Ok((ssh_connection_for(options, state), remote))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            host: "gpu-box".to_string(),
            known_hosts_file: "/data/ssh/known_hosts".to_string(),
            host_key_alias: "noel-env-gpu".to_string(),
            ..SshTarget::default()
        }
    }

    #[test]
    fn arguments_refuse_options_in_fields_and_quote_paths() {
        let refused =
            |target: SshTarget| ssh_arguments(&target, true, "/k").unwrap_err().to_string();
        assert_eq!(
            refused(SshTarget {
                host: "-oProxyCommand=x".to_string(),
                ..target()
            }),
            r#"Invalid host: "-oProxyCommand=x""#
        );
        assert_eq!(
            refused(SshTarget {
                user: Some("a b".to_string()),
                ..target()
            }),
            r#"Invalid user: "a b""#
        );
        assert_eq!(
            refused(SshTarget {
                host_key_alias: "x\u{3000}y".to_string(),
                ..target()
            }),
            "Invalid host key alias: \"x\u{3000}y\""
        );
        assert_eq!(
            refused(SshTarget {
                port: Some(0),
                ..target()
            }),
            "Invalid port: 0"
        );
        assert!(ssh_arguments(&target(), true, "/a\"b").is_err());
        assert_eq!(config_path("x", "/a b/%h").unwrap(), r#""/a b/%%h""#);
    }

    #[test]
    fn host_key_lines_must_be_plain_lines_for_the_alias() {
        let parse = |line: &str| {
            parse_host_key_line("noel-env-test", line).map(|(kind, _)| kind.to_string())
        };
        assert_eq!(
            parse("noel-env-test ssh-ed25519 AAAAC3Nza+/9== comment").unwrap(),
            "ssh-ed25519"
        );
        assert_eq!(
            parse("noel-env-test sk-ssh-ed25519@openssh.com AAAA").unwrap(),
            "sk-ssh-ed25519@openssh.com"
        );
        assert!(parse("other ssh-ed25519 AAAA").is_err());
        assert!(parse("@cert-authority noel-env-test ssh-ed25519 AAAA").is_err());
        assert!(parse("noel-env-test ssh-ed25519 AAAA===").is_err());
        assert!(parse("noel-env-test ssh-ED25519 AAAA").is_err());
        assert!(parse("noel-env-test ssh-ed25519").is_err());
    }

    #[test]
    fn probes_skip_banners_and_name_systems() {
        let probe = parse_probe(
            &target(),
            "Welcome!\r\nPI-ENV-PROBE\r\nLinux\r\naarch64\r\nGNU/Linux\r\n/home/me\r\n-\r\n-\r\n",
        )
        .unwrap();
        assert_eq!(
            (
                probe.system.as_str(),
                probe.machine.as_str(),
                probe.os.as_str(),
                probe.home.as_str()
            ),
            ("Linux", "aarch64", "GNU/Linux", "/home/me")
        );
        assert!(parse_probe(&target(), "no marker").is_err());
        assert_eq!(normalize_arch("AMD64").unwrap(), RemoteArch::X64);
        assert!(normalize_arch("riscv64").is_err());
        assert!(unknown_host_key(
            "No ED25519 host key is known for x and you have requested strict checking."
        ));
        assert!(unknown_host_key("Host key verification failed."));
        assert!(!unknown_host_key("Permission denied (publickey)."));
    }

    #[test]
    fn daemons_are_named_by_system_and_content() {
        let remote = RemotePlatform {
            platform: RemoteSystem::Darwin,
            arch: RemoteArch::Arm64,
            home: "/Users/me".to_string(),
        };
        assert_eq!(daemon_name(&remote), "noel-env-darwin-arm64");
        assert_eq!(
            packaged_daemon(&remote, Some(Path::new("/opt/noel"))).unwrap(),
            Path::new("/opt/noel/noel-env-darwin-arm64")
        );
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(
            daemon_path(&remote, &sha),
            "/Users/me/.cache/noel-env/noel-env-0123456789abcdef0123456789abcdef"
        );
        assert_eq!(launch_command("/h/it's", false), r#"'/h/it'\''s'"#);
        assert_eq!(
            launch_command("/h/d", true),
            r#"exec "$SHELL" -lc 'exec "$0" "$@"' '/h/d'"#
        );
    }
}
