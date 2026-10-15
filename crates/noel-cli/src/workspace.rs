//! Remote workspaces (spec §10): a session whose tools run on another
//! machine, reached over `ssh` through the `noel-env` daemon.
//!
//! - **Choosing one.** `--workspace ssh://[user@]host[:port]/absolute/path`
//!   when a session starts (the TUI, `-p`, `--mode rpc`), or the same URL in
//!   Mode 2's `noel.sessions.create`. The session's journal saves it as one
//!   `workspace` record ([`SshWorkspace::record`]); reopening the session
//!   uses it, and a different `--workspace` for it is refused.
//! - **Trust.** Never automatic: `noel workspace trust ssh://host[:port]`
//!   shows the host's key fingerprints and stores the key in noel's own
//!   known-hosts file under its state dir (`workspaces/known_hosts`), under
//!   the alias `noel-env-<host>` (`noel-env-[<host>]:<port>` with a port);
//!   `noel workspace forget` removes it. `~/.ssh` is never written. A session
//!   on a host that is not trusted does not start.
//! - **Reaching it.** The connection starts at the first tool call and again
//!   after it is lost, so an unreachable host fails that call, not the
//!   session. `?login=1` starts the daemon through the remote login shell.
//!
//! Only the tools and the project context files come from the workspace:
//! settings, extensions, skills and prompt templates are this machine's.

use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{anyhow, Result};
use base64::Engine;
use clap::{Parser, Subcommand};
use noel_core::{EnvProvider, ExecutionEnv, Runtime, SessionState, SessionWorkspace};
use noel_env::ssh::{
    accept_host_key, forget_host_key, scan_host_key, ssh_connection, SshConnectOptions, SshError,
    SshTarget,
};
use noel_env::{ConnectionStatus, RemoteEnv, RemoteEnvOptions};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The adapter name the journal saves for an ssh workspace.
pub(crate) const SSH_ADAPTER: &str = "ssh";
const SCHEME: &str = "ssh://";
/// What a workspace URL looks like, for error messages.
const URL_FORM: &str = "ssh://[user@]host[:port]/absolute/path";

/// A workspace on another machine, reached with `ssh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshWorkspace {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// An absolute, normalized path on that machine.
    pub cwd: String,
    /// Start the daemon through the remote login shell (`?login=1`).
    pub login: bool,
}

/// `%XX` escapes decoded, as UTF-8 without control characters.
fn percent_decode(text: &str, what: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let byte = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                .ok_or_else(|| anyhow!("{what} has an invalid % escape: {text}"))?;
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| anyhow!("{what} is not UTF-8 once decoded: {text}"))?;
    if decoded.chars().any(char::is_control) {
        return Err(anyhow!("{what} has a control character: {text}"));
    }
    Ok(decoded)
}

/// A path as a URL writes it: `%`, spaces and URL delimiters escaped.
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for character in path.chars() {
        match character {
            '%' | ' ' | '?' | '#' => encoded.push_str(&format!("%{:02X}", character as u32)),
            other => encoded.push(other),
        }
    }
    encoded
}

/// `/a/./b/../c/` as `/a/c`.
fn normalize_absolute(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// A host name, IPv4 address, or (inside brackets) IPv6 address: nothing
/// `ssh` could read as an option, a pattern or a second argument.
fn valid_host(host: &str, bracketed: bool) -> bool {
    let allowed = |character: char| {
        character.is_ascii_alphanumeric()
            || matches!(character, '.' | '-' | '_')
            || (bracketed && character == ':')
    };
    !host.is_empty() && !host.starts_with('-') && host.chars().all(allowed)
}

fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && !user.starts_with('-')
        && user.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '$')
        })
}

/// The authority of an `ssh://` URL: `[user@]host[:port]`.
fn parse_authority(authority: &str) -> Result<(Option<String>, String, Option<u16>)> {
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, rest)) => {
            let user = percent_decode(user, "the user")?;
            if !valid_user(&user) {
                return Err(anyhow!("invalid user in workspace URL: {user:?}"));
            }
            (Some(user), rest)
        }
        None => (None, authority),
    };
    let (host, port, bracketed) = if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("unclosed [ in workspace URL host: {host_port}"))?;
        let port = match after {
            "" => None,
            after => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| anyhow!("unexpected {after:?} after the host's ]"))?,
            ),
        };
        (host, port, true)
    } else {
        match host_port.split_once(':') {
            Some((host, port)) => (host, Some(port), false),
            None => (host_port, None, false),
        }
    };
    if !valid_host(host, bracketed) {
        return Err(anyhow!("invalid host in workspace URL: {host:?}"));
    }
    let port = port
        .map(|port| {
            port.parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| anyhow!("invalid port in workspace URL: {port:?}"))
        })
        .transpose()?;
    Ok((user, host.to_owned(), port))
}

impl SshWorkspace {
    /// `ssh://[user@]host[:port]/absolute/path[?login=1]`. The path is
    /// percent-decoded and normalized; `login` takes `1`, `0`, `true` or
    /// `false`.
    pub fn parse(url: &str) -> Result<Self> {
        let (authority, path, query) = split_url(url)?;
        let (user, host, port) = parse_authority(authority)?;
        let Some(path) = path else {
            return Err(anyhow!(
                "the workspace URL {url} names no directory; use {URL_FORM}"
            ));
        };
        let cwd = normalize_absolute(&percent_decode(path, "the workspace path")?);
        let mut login = false;
        for pair in query.into_iter().flat_map(|query| query.split('&')) {
            match pair.split_once('=').unwrap_or((pair, "1")) {
                ("login", "1" | "true") => login = true,
                ("login", "0" | "false") => login = false,
                ("login", value) => {
                    return Err(anyhow!(
                        "login must be 1 or 0 in a workspace URL, not {value:?}"
                    ))
                }
                (key, _) => return Err(anyhow!("unknown workspace URL option {key:?}")),
            }
        }
        Ok(Self {
            host,
            user,
            port,
            cwd,
            login,
        })
    }

    /// The host part, as the URL writes it: `host`, `host:port`, `[v6]:port`.
    pub fn host_label(&self) -> String {
        host_label(&self.host, self.port)
    }

    /// The URL that names this workspace.
    pub fn url(&self) -> String {
        let user = self
            .user
            .as_ref()
            .map(|user| format!("{user}@"))
            .unwrap_or_default();
        let login = if self.login { "?login=1" } else { "" };
        format!(
            "{SCHEME}{user}{}{}{login}",
            self.host_label(),
            percent_encode_path(&self.cwd)
        )
    }

    /// The journal's record of this workspace. Its id names the machine's
    /// files, `ssh://host[:port]`: any path and user there see the same.
    pub fn record(&self) -> SessionWorkspace {
        let mut options = Map::new();
        options.insert("host".into(), json!(self.host));
        if let Some(user) = &self.user {
            options.insert("user".into(), json!(user));
        }
        if let Some(port) = self.port {
            options.insert("port".into(), json!(port));
        }
        if self.login {
            options.insert("login".into(), json!(true));
        }
        SessionWorkspace {
            adapter: SSH_ADAPTER.to_owned(),
            id: format!("{SCHEME}{}", self.host_label()),
            cwd: self.cwd.clone(),
            options,
        }
    }

    /// The workspace a journal record names.
    pub fn from_record(record: &SessionWorkspace) -> Result<Self, String> {
        if record.adapter != SSH_ADAPTER {
            return Err(format!(
                "This session's workspace uses the {:?} adapter, which this version of noel does not have",
                record.adapter
            ));
        }
        let invalid = |what: &str| format!("This session's workspace record has an invalid {what}");
        let host = record.text("host").ok_or_else(|| invalid("host"))?;
        if !valid_host(host, host.contains(':')) {
            return Err(invalid("host"));
        }
        let user = match record.options.get("user") {
            None => None,
            Some(Value::String(user)) if valid_user(user) => Some(user.clone()),
            Some(_) => return Err(invalid("user")),
        };
        let port = match record.options.get("port") {
            None => None,
            Some(port) => Some(
                port.as_u64()
                    .and_then(|port| u16::try_from(port).ok())
                    .filter(|port| *port > 0)
                    .ok_or_else(|| invalid("port"))?,
            ),
        };
        if !record.cwd.starts_with('/') {
            return Err(invalid("directory"));
        }
        Ok(Self {
            host: host.to_owned(),
            user,
            port,
            cwd: record.cwd.clone(),
            login: record.options.get("login") == Some(&Value::Bool(true)),
        })
    }

    fn target(&self, known_hosts: &Path) -> SshTarget {
        SshTarget {
            host: self.host.clone(),
            user: self.user.clone(),
            port: self.port,
            known_hosts_file: known_hosts.to_string_lossy().into_owned(),
            host_key_alias: host_key_alias(&self.host, self.port),
            ..SshTarget::default()
        }
    }
}

/// `ssh://` URLs split into their authority, path and query.
fn split_url(url: &str) -> Result<(&str, Option<&str>, Option<&str>)> {
    let rest = url
        .strip_prefix(SCHEME)
        .ok_or_else(|| anyhow!("a workspace is {URL_FORM}, not {url}"))?;
    if rest.contains('#') {
        return Err(anyhow!("a workspace URL has no #fragment: {url}"));
    }
    let (rest, query) = match rest.split_once('?') {
        Some((rest, query)) => (rest, Some(query)),
        None => (rest, None),
    };
    Ok(match rest.find('/') {
        Some(slash) => (&rest[..slash], Some(&rest[slash..]), query),
        None => (rest, None, query),
    })
}

fn host_label(host: &str, port: Option<u16>) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

/// The name noel's known-hosts file keeps a host's keys under, whatever
/// aliases and jump hosts reach it: `noel-env-<host>`, with a port
/// `noel-env-[<host>]:<port>` as `ssh` writes a host on another port.
pub(crate) fn host_key_alias(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("noel-env-[{host}]:{port}"),
        None => format!("noel-env-{host}"),
    }
}

/// noel's own known-hosts file, under its state dir.
pub(crate) fn known_hosts_file(agent_dir: &Path) -> PathBuf {
    agent_dir.join("workspaces").join("known_hosts")
}

/// Whether noel's known-hosts file has a key for `alias`.
fn trusts(known_hosts: &Path, alias: &str) -> bool {
    std::fs::read_to_string(known_hosts).is_ok_and(|text| {
        text.lines()
            .any(|line| line.split_whitespace().next() == Some(alias))
    })
}

/// The command that trusts the host of `workspace`.
fn trust_command(workspace: &SshWorkspace) -> String {
    format!("noel workspace trust {SCHEME}{}", workspace.host_label())
}

/// Refuses a workspace whose host key noel does not trust yet, naming the
/// command that trusts it.
pub(crate) fn require_trusted(workspace: &SshWorkspace, agent_dir: &Path) -> Result<(), String> {
    let known_hosts = known_hosts_file(agent_dir);
    if trusts(
        &known_hosts,
        &host_key_alias(&workspace.host, workspace.port),
    ) {
        return Ok(());
    }
    Err(format!(
        "noel does not trust the host key of {} yet. Check its fingerprint and trust it with: {}",
        workspace.host_label(),
        trust_command(workspace)
    ))
}

/// The workspace `--workspace` names, checked: a valid URL on a trusted
/// host.
pub(crate) fn from_flag(url: &str, agent_dir: &Path) -> Result<SessionWorkspace> {
    let workspace = SshWorkspace::parse(url)?;
    require_trusted(&workspace, agent_dir).map_err(|message| anyhow!(message))?;
    Ok(workspace.record())
}

/// How a workspace record reads, for messages: its URL.
pub(crate) fn describe(record: &SessionWorkspace) -> String {
    SshWorkspace::from_record(record)
        .map(|workspace| workspace.url())
        .unwrap_or_else(|_| format!("{} {}", record.adapter, record.id))
}

/// Where a session's tools run, for messages.
fn location(record: Option<&SessionWorkspace>) -> String {
    match record {
        None => "on this machine".to_owned(),
        Some(record) => format!("in {}", describe(record)),
    }
}

/// Refuses `--workspace` for a session that already has another workspace.
pub(crate) fn check_reopened(
    state: &SessionState,
    requested: Option<&SessionWorkspace>,
) -> Result<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if state.workspace.as_ref() == Some(requested) {
        return Ok(());
    }
    Err(anyhow!(
        "--workspace {}: session {} runs its tools {}; a session keeps the workspace it started with",
        describe(requested),
        state.session_id,
        location(state.workspace.as_ref())
    ))
}

/// Refuses to start a session whose saved workspace cannot be used: an
/// adapter this version lacks, or a host noel does not trust.
pub(crate) fn check_saved(state: &SessionState, agent_dir: &Path) -> Result<()> {
    let Some(record) = &state.workspace else {
        return Ok(());
    };
    let workspace = SshWorkspace::from_record(record).map_err(|message| anyhow!(message))?;
    require_trusted(&workspace, agent_dir).map_err(|message| anyhow!(message))
}

// ─── opening workspaces ─────────────────────────────────────────────────────

/// The workspaces this process opened, by URL: one connection each, which
/// every tool call of every session on it shares.
static OPENED: Mutex<BTreeMap<String, Arc<RemoteEnv>>> = Mutex::new(BTreeMap::new());

fn opened() -> std::sync::MutexGuard<'static, BTreeMap<String, Arc<RemoteEnv>>> {
    OPENED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process markers noel sets for what it runs (`main`), set for the
/// workspace's commands too.
const MARKERS: [&str; 3] = ["AI_AGENT", "NOEL_CODING_AGENT", "PI_CODING_AGENT"];

/// The workspace a session's record names, opened once per process: a
/// [`RemoteEnv`] over a lazy `ssh` connection.
fn open(record: &SessionWorkspace, agent_dir: &Path) -> Result<Arc<dyn ExecutionEnv>, String> {
    let workspace = SshWorkspace::from_record(record)?;
    require_trusted(&workspace, agent_dir)?;
    let url = workspace.url();
    if let Some(env) = opened().get(&url) {
        return Ok(env.clone());
    }
    let connection = ssh_connection(SshConnectOptions {
        target: workspace.target(&known_hosts_file(agent_dir)),
        login_shell: workspace.login,
        ..SshConnectOptions::default()
    })
    .connection;
    let env = Arc::new(RemoteEnv::new(RemoteEnvOptions {
        shell_env: MARKERS
            .iter()
            .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
            .collect(),
        ..RemoteEnvOptions::new(connection, record.id.clone(), workspace.cwd.clone())
    }));
    Ok(opened().entry(url).or_insert(env).clone())
}

/// Lets `runtime` open the workspace of any session it holds, now or after
/// a session switch, and read its project context files unless
/// `context_files` is false (`--no-context-files`). A session without a
/// workspace record runs its tools here, exactly as before.
pub(crate) fn install(runtime: &mut Runtime, agent_dir: &Path, context_files: bool) {
    let agent_dir = agent_dir.to_path_buf();
    runtime.set_env_provider(EnvProvider::Workspaces(Arc::new(move |_target, record| {
        open(record, &agent_dir)
    })));
    runtime.set_workspace_context_enabled(context_files);
}

/// The footer's account of a session's workspace: its URL and whether it is
/// connected.
pub(crate) fn footer_label(record: &SessionWorkspace) -> String {
    let Ok(workspace) = SshWorkspace::from_record(record) else {
        return describe(record);
    };
    let url = workspace.url();
    let status = match opened().get(&url).map(|env| env.connection().status()) {
        None | Some(ConnectionStatus::Idle) => "not connected",
        Some(ConnectionStatus::Connecting) => "connecting",
        Some(ConnectionStatus::Connected) => "connected",
        Some(ConnectionStatus::Lost(_)) => "lost",
        Some(ConnectionStatus::Closed) => "closed",
    };
    format!("{url} ({status})")
}

// ─── noel workspace trust | forget ──────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "noel workspace",
    about = "Trust or forget the hosts of remote workspaces (--workspace ssh://…)"
)]
struct WorkspaceArgs {
    #[command(subcommand)]
    command: WorkspaceCommand,
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Show a host's key fingerprints and trust its key for noel's
    /// workspaces, after a y/N prompt or with --fingerprint.
    Trust {
        /// ssh://[user@]host[:port], with or without a path.
        url: String,
        /// Trust only the key with this fingerprint (`SHA256:…`), without a
        /// prompt; compare it with one obtained from the host itself.
        #[arg(long)]
        fingerprint: Option<String>,
    },
    /// Stop trusting every key noel keeps for a host.
    Forget {
        /// ssh://host[:port], with or without a user or path.
        url: String,
    },
}

/// Runs `noel workspace …` when the command line names it, and returns the
/// exit status; `None` for any other command.
pub(crate) async fn try_run(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some("workspace") {
        return None;
    }
    let rest = std::iter::once("noel workspace".to_string()).chain(args[2..].iter().cloned());
    let parsed = match WorkspaceArgs::try_parse_from(rest) {
        Ok(parsed) => parsed,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            return Some(code);
        }
    };
    Some(match command(parsed.command).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("noel workspace: {error:#}");
            1
        }
    })
}

/// The workspace host a `trust` or `forget` URL names; its path, if any, is
/// not needed.
fn host_of(url: &str) -> Result<SshWorkspace> {
    let (authority, _, _) = split_url(url)?;
    let (user, host, port) = parse_authority(authority)?;
    Ok(SshWorkspace {
        host,
        user,
        port,
        cwd: "/".to_owned(),
        login: false,
    })
}

/// `SHA256:…` of a known-hosts line's key, as `ssh-keygen -l` prints it.
fn fingerprint(line: &str) -> Option<String> {
    let key = line.split_whitespace().nth(2)?;
    let blob = base64::engine::general_purpose::STANDARD.decode(key).ok()?;
    let digest = Sha256::digest(&blob);
    Some(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
    ))
}

async fn command(command: WorkspaceCommand) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::config_paths(cwd, None)?;
    let known_hosts = known_hosts_file(&paths.agent_dir);
    match command {
        WorkspaceCommand::Trust { url, fingerprint } => {
            trust(&host_of(&url)?, &known_hosts, fingerprint.as_deref()).await
        }
        WorkspaceCommand::Forget { url } => {
            let workspace = host_of(&url)?;
            forget_host_key(&workspace.target(&known_hosts))
                .await
                .map_err(|error| anyhow!(error.to_string()))?;
            println!(
                "noel no longer trusts the host key of {} ({})",
                workspace.host_label(),
                known_hosts.display()
            );
            Ok(())
        }
    }
}

async fn trust(workspace: &SshWorkspace, known_hosts: &Path, wanted: Option<&str>) -> Result<()> {
    let target = workspace.target(known_hosts);
    let label = workspace.host_label();
    let scanned = scan_host_key(&target)
        .await
        .map_err(|error| anyhow!("{label}: {error}"))?;
    println!("The host key of {label} (ssh-keygen -lf):");
    for line in &scanned.fingerprints {
        println!("  {line}");
    }
    let lines: Vec<String> = match wanted {
        Some(wanted) => {
            let matching: Vec<String> = scanned
                .lines
                .iter()
                .filter(|line| fingerprint(line).as_deref() == Some(wanted))
                .cloned()
                .collect();
            if matching.is_empty() {
                return Err(anyhow!(
                    "no key of {label} has the fingerprint {wanted}; nothing was trusted"
                ));
            }
            matching
        }
        None if io::stdin().is_terminal() => {
            eprint!(
                "Compare it with the host's own (ssh-keygen -lf /etc/ssh/ssh_host_*_key.pub there).\nTrust it for noel workspaces? [y/N] "
            );
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().lock().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
                return Err(anyhow!("{label} was not trusted"));
            }
            scanned.lines.clone()
        }
        None => {
            return Err(anyhow!(
                "not a terminal: pass --fingerprint SHA256:… (the host's own fingerprint) to trust {label} without a prompt"
            ))
        }
    };
    accept_host_key(&target, &lines)
        .await
        .map_err(|error| match error {
            SshError::HostKeyChanged(message) => anyhow!(
                "{message}. If the change is expected, run: noel workspace forget {SCHEME}{label}"
            ),
            other => anyhow!(other.to_string()),
        })?;
    println!(
        "noel trusts the host key of {label} for workspaces ({})",
        known_hosts.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> SshWorkspace {
        SshWorkspace::parse(url).unwrap_or_else(|error| panic!("{url}: {error}"))
    }

    fn refused(url: &str) -> String {
        SshWorkspace::parse(url).unwrap_err().to_string()
    }

    #[test]
    fn urls_name_a_host_an_optional_user_and_port_and_a_directory() {
        assert_eq!(
            parse("ssh://box/work"),
            SshWorkspace {
                host: "box".into(),
                user: None,
                port: None,
                cwd: "/work".into(),
                login: false,
            }
        );
        let full = parse("ssh://me@10.0.0.2:2222/srv/my%20app/./x/../?login=1");
        assert_eq!(
            (
                full.user.as_deref(),
                full.host.as_str(),
                full.port,
                full.cwd.as_str(),
                full.login
            ),
            (Some("me"), "10.0.0.2", Some(2222), "/srv/my app", true)
        );
        assert_eq!(full.url(), "ssh://me@10.0.0.2:2222/srv/my%20app?login=1");
        assert_eq!(parse(&full.url()), full);
        let v6 = parse("ssh://[::1]:22/");
        assert_eq!(
            (v6.host.as_str(), v6.port, v6.cwd.as_str()),
            ("::1", Some(22), "/")
        );
        assert_eq!(v6.url(), "ssh://[::1]:22/");
        assert!(!parse("ssh://box/a?login=0").login);
    }

    #[test]
    fn urls_that_are_not_workspaces_are_refused() {
        assert!(refused("box:/work").contains("ssh://[user@]host[:port]/absolute/path"));
        assert!(refused("ssh://box").contains("names no directory"));
        assert!(refused("ssh://-oProxyCommand=x/work").contains("invalid host"));
        assert!(refused("ssh://-me@box/work").contains("invalid user"));
        assert!(refused("ssh://b x/work").contains("invalid host"));
        assert!(refused("ssh://box:0/work").contains("invalid port"));
        assert!(refused("ssh://box:99999/work").contains("invalid port"));
        assert!(refused("ssh://box:22x/work").contains("invalid port"));
        assert!(refused("ssh://[::1/work").contains("unclosed"));
        assert!(refused("ssh://box/work?shell=zsh").contains("unknown workspace URL option"));
        assert!(refused("ssh://box/work?login=maybe").contains("login must be"));
        assert!(refused("ssh://box/work#x").contains("fragment"));
        assert!(refused("ssh://box/a%2").contains("invalid % escape"));
        assert!(refused("ssh://box/a%0Ab").contains("control character"));
    }

    #[test]
    fn records_round_trip_and_name_the_hosts_files() {
        let workspace = parse("ssh://me@box:2222/work?login=1");
        let record = workspace.record();
        assert_eq!(
            serde_json::to_value(&record).unwrap(),
            json!({
                "adapter": "ssh", "id": "ssh://box:2222", "cwd": "/work",
                "host": "box", "user": "me", "port": 2222, "login": true,
            })
        );
        assert_eq!(SshWorkspace::from_record(&record).unwrap(), workspace);
        // Another path on the same machine sees the same files.
        assert_eq!(
            parse("ssh://other@box:2222/elsewhere").record().id,
            record.id
        );
        assert_eq!(
            serde_json::to_value(parse("ssh://box/w").record()).unwrap(),
            json!({"adapter": "ssh", "id": "ssh://box", "cwd": "/w", "host": "box"})
        );
        let foreign = SessionWorkspace {
            adapter: "codewire".into(),
            ..record.clone()
        };
        assert!(SshWorkspace::from_record(&foreign)
            .unwrap_err()
            .contains("\"codewire\" adapter"));
        assert_eq!(host_key_alias("box", None), "noel-env-box");
        assert_eq!(host_key_alias("::1", Some(2222)), "noel-env-[::1]:2222");
    }

    #[test]
    fn untrusted_hosts_are_refused_with_the_command_that_trusts_them() {
        let dir = std::env::temp_dir().join(format!("noel-ws-trust-{}", std::process::id()));
        let workspace = parse("ssh://me@box:2222/work");
        let message = require_trusted(&workspace, &dir).unwrap_err();
        assert!(
            message.contains("noel workspace trust ssh://box:2222"),
            "{message}"
        );
        let file = known_hosts_file(&dir);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "noel-env-box ssh-ed25519 AAAA\n").unwrap();
        // Trust is per host and port.
        assert!(require_trusted(&workspace, &dir).is_err());
        std::fs::write(&file, "noel-env-[box]:2222 ssh-ed25519 AAAA\n").unwrap();
        require_trusted(&workspace, &dir).unwrap();
        let mut state = SessionState::new("s", "/here".into());
        state.workspace = Some(workspace.record());
        check_saved(&state, &dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_reopened_session_keeps_its_workspace() {
        let mut state = SessionState::new("s1", "/here".into());
        let workspace = parse("ssh://box/work").record();
        // A local session refuses a workspace.
        let error = check_reopened(&state, Some(&workspace))
            .unwrap_err()
            .to_string();
        assert!(error.contains("runs its tools on this machine"), "{error}");
        check_reopened(&state, None).unwrap();
        state.workspace = Some(workspace.clone());
        check_reopened(&state, Some(&workspace)).unwrap();
        check_reopened(&state, None).unwrap();
        let other = parse("ssh://box/elsewhere").record();
        let error = check_reopened(&state, Some(&other))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("runs its tools in ssh://box/work"),
            "{error}"
        );
    }

    #[test]
    fn fingerprints_are_those_ssh_keygen_prints() {
        // A throwaway key; `ssh-keygen -lf` printed this fingerprint for it.
        let line = "noel-env-box ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEkTfD9MHTJ17XaccHgmbzbKL4eXWllTUvlX6Loix89";
        assert_eq!(
            fingerprint(line).unwrap(),
            "SHA256:4kpFPkPYt8i5iJind9AkMoQX0fbuoShKlhcybNZqQwU"
        );
        assert_eq!(fingerprint("noel-env-box ssh-ed25519"), None);
    }
}
